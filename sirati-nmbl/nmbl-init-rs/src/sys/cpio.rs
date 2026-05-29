//! Minimal in-memory writer for the cpio "newc" format, sufficient for
//! appending key-injection fragments to a kexec'd initrd.
//!
//! Linux's initrd unpacker walks concatenated cpio archives (each
//! optionally compressed independently); we append our fragment
//! uncompressed after the system initrd's compressed cpio so stage-1
//! sees the injected files as overlay entries.
//!
//! Only the subset we actually need is implemented: directories
//! (mode = `040755`), regular files (mode = `100400`), and the
//! mandatory `TRAILER!!!` end-of-archive marker. Owners are root; the
//! `0400` file mode keeps secrets unreadable to anything but stage-1's
//! init (which runs as root).
//!
//! See Linux's `Documentation/driver-api/early-userspace/buffer-format.rst`
//! and `init/initramfs.c` for the format reference.

use std::path::Path;

use zeroize::Zeroizing;

/// `mode_t` for a directory with `0755` perms (`S_IFDIR | 0o755`).
const MODE_DIR: u32 = 0o040_755;
/// `mode_t` for a regular file with `0400` perms (`S_IFREG | 0o400`).
const MODE_FILE_RO: u32 = 0o100_400;
/// End-of-archive marker name required by the newc format.
const TRAILER: &str = "TRAILER!!!";

/// One file to inject into the initrd. `path` is the in-cpio (relative)
/// path the kernel will create under the initramfs tmpfs root.
pub struct InjectionEntry<'a> {
    pub path: &'a Path,
    pub content: &'a [u8],
}

/// Pad `out` with NUL bytes up to a 4-byte boundary. The newc spec
/// requires header+name and file-data each be 4-byte aligned.
fn pad_to_4(out: &mut Vec<u8>) {
    while !out.len().is_multiple_of(4) {
        out.push(0);
    }
}

/// Emit one newc entry to `out` (`directories pass `&[]` for content).
fn write_entry(out: &mut Vec<u8>, name: &str, content: &[u8], mode: u32) {
    // 6-char magic + 13 fields × 8 hex chars = 110 bytes.
    let header = format!(
        "070701\
         {ino:08x}{mode:08x}{uid:08x}{gid:08x}{nlink:08x}\
         {mtime:08x}{filesize:08x}{devmajor:08x}{devminor:08x}\
         {rdevmajor:08x}{rdevminor:08x}{namesize:08x}{check:08x}",
        ino = 0,
        mode = mode,
        uid = 0,
        gid = 0,
        nlink = if mode & 0o170_000 == 0o040_000 { 2 } else { 1 },
        mtime = 0,
        filesize = content.len(),
        devmajor = 0,
        devminor = 0,
        rdevmajor = 0,
        rdevminor = 0,
        // namesize includes the trailing NUL.
        namesize = name.len().saturating_add(1),
        check = 0,
    );
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    pad_to_4(out);
    out.extend_from_slice(content);
    pad_to_4(out);
}

/// Build the parent-directory list for one in-cpio path, in
/// shallowest→deepest order. Returns the path components joined as
/// in-cpio names. Leading `/` is stripped.
fn parent_dirs_of(path: &Path) -> Vec<String> {
    let s = path.to_string_lossy();
    let stripped = s.trim_start_matches('/');
    let parts: Vec<&str> = stripped.split('/').filter(|p| !p.is_empty()).collect();
    let mut out = Vec::new();
    // All but the last component are directories that must exist.
    for i in 1..parts.len() {
        if let Some(slice) = parts.get(..i) {
            out.push(slice.join("/"));
        }
    }
    out
}

/// Convert a `Path` to its in-cpio name (strip leading `/`).
fn in_cpio_name(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.trim_start_matches('/').to_string()
}

/// Build a single-archive cpio fragment containing every entry in
/// `entries`, plus all required parent directories (deduplicated), plus
/// the terminating `TRAILER!!!` marker. The result is in a
/// `Zeroizing<Vec<u8>>` because the content bytes typically include
/// secret passphrases.
pub fn build_fragment(entries: &[InjectionEntry<'_>]) -> Zeroizing<Vec<u8>> {
    let mut out: Zeroizing<Vec<u8>> = Zeroizing::new(Vec::new());

    // Walk in two passes so parent directories are emitted before any
    // file that needs them, and shared parents are only emitted once.
    let mut dirs: Vec<String> = Vec::new();
    for entry in entries {
        for d in parent_dirs_of(entry.path) {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
    }
    for d in &dirs {
        write_entry(&mut out, d, &[], MODE_DIR);
    }

    for entry in entries {
        let name = in_cpio_name(entry.path);
        write_entry(&mut out, &name, entry.content, MODE_FILE_RO);
    }

    write_entry(&mut out, TRAILER, &[], 0);
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests assert on byte-level structure"
)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn parse_u32_hex(slice: &[u8]) -> u32 {
        let s = std::str::from_utf8(slice).expect("hex must be ascii");
        u32::from_str_radix(s, 16).expect("valid hex")
    }

    #[test]
    fn pad_to_4_rounds_up_only_when_misaligned() {
        let mut buf: Vec<u8> = vec![1, 2, 3, 4];
        pad_to_4(&mut buf);
        assert_eq!(buf.len(), 4, "already aligned should not grow");

        buf.push(5);
        pad_to_4(&mut buf);
        assert_eq!(buf.len(), 8, "5 → 8");
        assert_eq!(&buf[5..8], &[0, 0, 0]);
    }

    #[test]
    fn parent_dirs_of_emits_intermediates_shallow_first() {
        assert_eq!(
            parent_dirs_of(Path::new("/etc/nmbl-luks/cryptroot")),
            vec!["etc".to_string(), "etc/nmbl-luks".to_string()],
        );
        assert!(parent_dirs_of(Path::new("/key")).is_empty());
        assert!(parent_dirs_of(Path::new("key")).is_empty());
    }

    #[test]
    fn in_cpio_name_strips_leading_slash() {
        assert_eq!(in_cpio_name(Path::new("/etc/foo")), "etc/foo");
        assert_eq!(in_cpio_name(Path::new("etc/foo")), "etc/foo");
    }

    #[test]
    fn fragment_starts_with_newc_magic_and_ends_with_trailer() {
        let path = PathBuf::from("/etc/nmbl-luks/cryptroot");
        let entries = vec![InjectionEntry {
            path: path.as_path(),
            content: b"hunter2",
        }];
        let fragment = build_fragment(&entries);
        assert!(
            fragment.starts_with(b"070701"),
            "must start with newc magic"
        );
        // The TRAILER entry sits near the end; its name is "TRAILER!!!".
        let needle = b"TRAILER!!!";
        let found = (0..fragment.len())
            .rev()
            .any(|i| fragment.get(i..i.saturating_add(needle.len())) == Some(needle));
        assert!(found, "fragment must contain TRAILER!!! marker");
    }

    #[test]
    fn fragment_emits_parent_dirs_then_file() {
        let path = PathBuf::from("/etc/nmbl-luks/cryptroot");
        let entries = vec![InjectionEntry {
            path: path.as_path(),
            content: b"x",
        }];
        let fragment = build_fragment(&entries);
        // Each header is 110 bytes; the first header's mode field is at
        // offset 6+8 (magic + ino).
        let mode_offset = 6 + 8;
        let mode = parse_u32_hex(&fragment[mode_offset..mode_offset + 8]);
        assert_eq!(
            mode & 0o170_000,
            0o040_000,
            "first entry must be a directory (etc)"
        );
    }

    #[test]
    fn fragment_dedups_shared_parent_dirs() {
        let p1 = PathBuf::from("/etc/nmbl-luks/a");
        let p2 = PathBuf::from("/etc/nmbl-luks/b");
        let entries = vec![
            InjectionEntry {
                path: p1.as_path(),
                content: b"1",
            },
            InjectionEntry {
                path: p2.as_path(),
                content: b"2",
            },
        ];
        let fragment = build_fragment(&entries);
        // 2 parent dirs (etc, etc/nmbl-luks) + 2 files + 1 TRAILER = 5
        // newc headers. Counting magic-marker occurrences is the
        // simplest dedup check that doesn't reimplement the parser.
        let header_count = count_subseq(&fragment, b"070701");
        assert_eq!(header_count, 5, "exactly 5 cpio entries expected");
    }

    fn count_subseq(haystack: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() {
            return 0;
        }
        let mut count: usize = 0;
        let mut i: usize = 0;
        while i + needle.len() <= haystack.len() {
            if haystack.get(i..i + needle.len()) == Some(needle) {
                count = count.saturating_add(1);
                i = i.saturating_add(needle.len());
            } else {
                i = i.saturating_add(1);
            }
        }
        count
    }

    #[test]
    fn fragment_contains_secret_bytes() {
        let path = PathBuf::from("/k");
        let secret: &[u8] = b"correct horse battery staple";
        let entries = vec![InjectionEntry {
            path: path.as_path(),
            content: secret,
        }];
        let fragment = build_fragment(&entries);
        let mut found = false;
        for i in 0..fragment.len() {
            if fragment.get(i..i + secret.len()) == Some(secret) {
                found = true;
                break;
            }
        }
        assert!(found, "secret payload must round-trip into the fragment");
    }
}
