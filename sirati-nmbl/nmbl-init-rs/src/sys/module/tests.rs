//! Tests for kernel module dep parsing, resolution, and loading.

use std::collections::HashMap;
use std::path::PathBuf;

use nix::errno::Errno;

use crate::error::NmblError;

use super::dep::{
    LoadOutcome, ModuleEntry, canonical_module_name, index_by_name, is_recoverable_module_error,
    parse_modules_dep_text, resolve_load_order,
};
use super::load::{Compression, compression_for_path, decompress_module, load_module};

#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
fn by(entries: &[ModuleEntry], name: &str) -> ModuleEntry {
    entries
        .iter()
        .find(|e| e.name == name)
        .cloned()
        .expect("entry present")
}

#[test]
fn parses_names_and_deps() {
    let text = "\
kernel/fs/ext4/ext4.ko.xz: kernel/fs/jbd2/jbd2.ko.xz kernel/lib/crc16.ko.xz
kernel/fs/jbd2/jbd2.ko.xz: kernel/lib/crc32c_generic.ko.xz
kernel/lib/crc16.ko.xz:
kernel/lib/crc32c_generic.ko.xz:
";
    let root = PathBuf::from("/lib/modules/6.6.71");
    let entries = parse_modules_dep_text(text, &root);
    assert_eq!(entries.len(), 4);
    let ext4 = by(&entries, "ext4");
    assert_eq!(ext4.path, root.join("kernel/fs/ext4/ext4.ko.xz"));
    assert_eq!(ext4.deps, vec!["jbd2".to_owned(), "crc16".to_owned()]);
    assert!(by(&entries, "crc16").deps.is_empty());
}

#[test]
fn topological_order_is_deepest_first() {
    let text = "\
a.ko: b.ko
b.ko: c.ko
c.ko:
";
    let root = PathBuf::from("/m");
    let entries = parse_modules_dep_text(text, &root);
    let idx = index_by_name(&entries);
    let order = resolve_load_order("a", &idx).expect("resolve failed");
    let names: Vec<&str> = order.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["c", "b", "a"]);
}

#[test]
fn canonical_lookup_matches_hyphen_and_underscore() {
    // Regression: operators write `dm-crypt` in
    // `boot.initrd.kernelModules`, but the modules tree files the
    // .ko under the canonical underscore name `dm_crypt`. The
    // by-name index must be reachable through either spelling so
    // the explicit-load loop doesn't silently mark the module as
    // built-in and skip it — letting the kernel hit a downstream
    // "unknown target type" failure when activation runs.
    let text = "kernel/dm_crypt.ko.xz:\n";
    let root = PathBuf::from("/m");
    let entries = parse_modules_dep_text(text, &root);
    let idx = index_by_name(&entries);
    // The on-disk filename is `dm_crypt.ko.xz`; the canonical name
    // collapses hyphens to underscores so the index key is
    // `dm_crypt`.
    let dm_crypt = canonical_module_name("dm-crypt");
    assert!(
        idx.contains_key(dm_crypt.as_str()),
        "canonicalized lookup must hit the by-name index"
    );
    let order = resolve_load_order("dm-crypt", &idx).expect("resolve failed");
    let names: Vec<&str> = order.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["dm_crypt"],
        "resolve_load_order must accept the hyphen spelling and produce \
         the canonical entry"
    );
}

#[test]
fn missing_module_resolves_to_empty_order() {
    // A module name absent from modules.dep is treated as built-in
    // (warn-and-skip), not as a fatal error — that's the bash-side
    // modprobe semantics: built-in modules have nothing to load.
    let entries: Vec<ModuleEntry> = Vec::new();
    let idx = index_by_name(&entries);
    let order = resolve_load_order("ghost", &idx).expect("must not error");
    assert!(
        order.is_empty(),
        "built-in modules produce empty load order"
    );
}

#[test]
fn missing_transitive_dep_is_skipped_not_fatal() {
    // Realistic case: a .ko file lists encrypted-keys as a dep but
    // encrypted-keys is built into the kernel. The parent's load
    // order must still include the parent itself; the missing dep
    // is silently skipped.
    let text = "kernel/parent.ko.xz: kernel/builtin_dep.ko.xz\n";
    let root = PathBuf::from("/m");
    let entries = parse_modules_dep_text(text, &root);
    // by_name has only 'parent'; 'builtin_dep' is intentionally
    // not present to simulate kernel-built-in deps.
    let mut idx: HashMap<String, &ModuleEntry> = HashMap::new();
    if let Some(parent) = entries.iter().find(|e| e.name == "parent") {
        idx.insert("parent".to_owned(), parent);
    }
    let order = resolve_load_order("parent", &idx).expect("must not error");
    let names: Vec<&str> = order.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["parent"]);
}

#[test]
fn cycle_detection_errors() {
    let text = "\
a.ko: b.ko
b.ko: a.ko
";
    let root = PathBuf::from("/m");
    let entries = parse_modules_dep_text(text, &root);
    let idx = index_by_name(&entries);
    let err = resolve_load_order("a", &idx).expect_err("must error");
    match err {
        NmblError::Module { source, .. } => {
            assert_eq!(source, nix::Error::from(Errno::ELOOP));
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn parses_hyphenated_filename_as_underscored_name() {
    // The upstream kernel ships `kernel/drivers/md/dm-mod.ko.xz` and
    // `modules.dep` preserves that filename. The parser must fold
    // the hyphen to an underscore so callers asking for `dm_mod`
    // resolve against the same entry.
    let text = "kernel/drivers/md/dm-mod.ko.xz:\n";
    let root = PathBuf::from("/lib/modules/6.6.71");
    let entries = parse_modules_dep_text(text, &root);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "dm_mod");
    assert_eq!(entries[0].path, root.join("kernel/drivers/md/dm-mod.ko.xz"));
}

#[test]
fn parses_hyphenated_dep_name_as_underscored() {
    // Deps in `modules.dep` are also expressed as on-disk paths, so
    // the same hyphen-fold rule must apply to dependency names.
    let text = "\
kernel/foo/parent.ko.xz: kernel/drivers/md/dm-mod.ko.xz
kernel/drivers/md/dm-mod.ko.xz:
";
    let root = PathBuf::from("/m");
    let entries = parse_modules_dep_text(text, &root);
    let parent = by(&entries, "parent");
    assert_eq!(parent.deps, vec!["dm_mod".to_owned()]);
}

#[test]
fn resolve_underscore_query_against_hyphenated_entry() {
    // Caller passes `dm_mod` (the conventional spelling, e.g. from
    // boot.nmbl.kernelModules or the activation orchestrator), but
    // the on-disk filename is `dm-mod.ko.xz`. The query must
    // resolve, matching modprobe's behaviour.
    let text = "kernel/drivers/md/dm-mod.ko.xz:\n";
    let root = PathBuf::from("/lib/modules/6.6.71");
    let entries = parse_modules_dep_text(text, &root);
    let idx = index_by_name(&entries);
    let order = resolve_load_order("dm_mod", &idx).expect("resolve failed");
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].name, "dm_mod");
}

#[test]
fn resolve_hyphen_query_against_hyphenated_entry() {
    // The reverse direction: caller passes the hyphen spelling,
    // still must resolve.
    let text = "kernel/drivers/md/dm-mod.ko.xz:\n";
    let root = PathBuf::from("/lib/modules/6.6.71");
    let entries = parse_modules_dep_text(text, &root);
    let idx = index_by_name(&entries);
    let order = resolve_load_order("dm-mod", &idx).expect("resolve failed");
    assert_eq!(order.len(), 1);
    assert_eq!(order[0].name, "dm_mod");
}

#[test]
fn recoverable_classifier_covers_kernel_refusals() {
    // Every errno that the task / `init_module(2)` manpage flags as
    // "kernel cannot load this module right now" must be classified
    // as recoverable so we don't abort the boot for it. ENOENT here
    // is the module's own init() returning -ENOENT (e.g. a backend
    // cipher is unavailable); file-not-found at the .ko path itself
    // is intercepted earlier by load_module's existence pre-check.
    for errno in [
        Errno::EOPNOTSUPP,
        Errno::ENOEXEC,
        Errno::ENODEV,
        Errno::ENOSYS,
        Errno::EINVAL,
        Errno::ENOENT,
    ] {
        assert!(
            is_recoverable_module_error(errno),
            "{errno:?} should be recoverable"
        );
    }
}

#[test]
fn recoverable_classifier_does_not_swallow_real_errors() {
    // Filesystem permission / OOM / generic IO failures and
    // dep-graph bugs (ELOOP) must NOT be classified as recoverable.
    // EEXIST is excluded because it has its own
    // `LoadOutcome::AlreadyLoaded` variant and never reaches the
    // classifier.
    for errno in [
        Errno::EACCES,
        Errno::EPERM,
        Errno::ELOOP,
        Errno::EEXIST,
        Errno::ENOMEM,
        Errno::EIO,
    ] {
        assert!(
            !is_recoverable_module_error(errno),
            "{errno:?} must NOT be recoverable"
        );
    }
}

#[test]
fn compression_for_path_classifies_known_suffixes() {
    use std::path::Path;
    assert_eq!(
        compression_for_path(Path::new("/lib/modules/6.6.71/kernel/fs/ext4/ext4.ko")),
        Compression::None
    );
    assert_eq!(
        compression_for_path(Path::new("/lib/modules/6.6.71/kernel/fs/ext4/ext4.ko.xz")),
        Compression::Xz
    );
    assert_eq!(
        compression_for_path(Path::new("/lib/modules/6.6.71/kernel/fs/ext4/ext4.ko.zst")),
        Compression::Zst
    );
    assert_eq!(
        compression_for_path(Path::new("/lib/modules/6.6.71/kernel/fs/ext4/ext4.ko.gz")),
        Compression::Gz
    );
}

#[test]
fn compression_for_path_falls_back_to_none_for_unknown_suffix() {
    // Unrecognised suffixes are treated as "no compression" — the
    // kernel will reject the bytes with ENOEXEC if they're not a
    // valid ELF, which is the right failure mode (clear errno
    // rather than a confused decompression attempt).
    use std::path::Path;
    assert_eq!(
        compression_for_path(Path::new("/m/weird.ko.lz4")),
        Compression::None
    );
    assert_eq!(
        compression_for_path(Path::new("/m/no_suffix_at_all")),
        Compression::None
    );
}

/// Helper: write `bytes` to a fresh temp file with the given
/// suffix and return the path + the holding `TempDir` so the file
/// lives until the dir is dropped.
fn write_temp(suffix: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("widget{suffix}"));
    std::fs::write(&path, bytes).expect("write temp module");
    (dir, path)
}

#[test]
fn decompress_module_passes_through_uncompressed() {
    let payload: Vec<u8> = (0u8..=63).collect();
    let (_dir, path) = write_temp(".ko", &payload);
    let got = decompress_module(&path, "widget").expect("decompress");
    assert_eq!(got, payload);
}

#[test]
fn decompress_module_round_trips_xz() {
    // Encode with the same crate, decode with `decompress_module`.
    let payload: Vec<u8> = b"NMBL-MODULE-LOAD-TEST-XZ".repeat(8);
    let mut compressed: Vec<u8> = Vec::new();
    {
        let mut reader = std::io::Cursor::new(&payload);
        lzma_rs::xz_compress(&mut reader, &mut compressed).expect("xz_compress");
    }
    let (_dir, path) = write_temp(".ko.xz", &compressed);
    let got = decompress_module(&path, "widget").expect("decompress");
    assert_eq!(got, payload);
}

#[test]
fn decompress_module_round_trips_gz() {
    use flate2::Compression as GzLevel;
    use flate2::write::GzEncoder;
    use std::io::Write;
    let payload: Vec<u8> = b"NMBL-MODULE-LOAD-TEST-GZ".repeat(8);
    let mut encoder = GzEncoder::new(Vec::new(), GzLevel::default());
    encoder.write_all(&payload).expect("gz write");
    let compressed = encoder.finish().expect("gz finish");
    let (_dir, path) = write_temp(".ko.gz", &compressed);
    let got = decompress_module(&path, "widget").expect("decompress");
    assert_eq!(got, payload);
}

#[test]
fn decompress_module_decodes_zst_fixture() {
    // `ruzstd` is decode-only, so we can't synthesize a fixture
    // round-trip in-process. Embed a pre-compressed blob whose
    // plaintext is `b"NMBL-MODULE-LOAD-TEST"` (21 bytes, produced
    // once with `zstd -19`). If this ever needs regenerating:
    //
    //     printf '%s' 'NMBL-MODULE-LOAD-TEST' | zstd -19 | od -An -tx1
    const ZST_FIXTURE: &[u8] = &[
        0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x68, 0xa9, 0x00, 0x00, 0x4e, 0x4d, 0x42, 0x4c, 0x2d, 0x4d,
        0x4f, 0x44, 0x55, 0x4c, 0x45, 0x2d, 0x4c, 0x4f, 0x41, 0x44, 0x2d, 0x54, 0x45, 0x53, 0x54,
        0x62, 0xec, 0xd6, 0x51,
    ];
    let (_dir, path) = write_temp(".ko.zst", ZST_FIXTURE);
    let got = decompress_module(&path, "widget").expect("decompress");
    assert_eq!(got, b"NMBL-MODULE-LOAD-TEST");
}

#[test]
fn decompress_module_surfaces_corrupt_xz_as_module_error() {
    // A 4-byte garbage payload labelled `.ko.xz` cannot possibly
    // be a valid XZ stream. The contract is that decompression
    // failure surfaces as `NmblError::Module { source: EIO }`
    // (with the real backend message logged via nmbl_warn!) so
    // the caller's match arms stay simple.
    let (_dir, path) = write_temp(".ko.xz", b"junk");
    let err = decompress_module(&path, "widget").expect_err("must fail");
    match err {
        NmblError::Module { name, source, .. } => {
            assert_eq!(name, "widget");
            assert_eq!(source, nix::Error::from(Errno::EIO));
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn load_module_returns_file_missing_for_absent_ko_file() {
    // makeModulesClosure { allowMissing = true; } prunes transitive
    // modules out of the closure but leaves them referenced in
    // modules.dep. load_module must surface that as a non-fatal
    // FileMissing outcome rather than propagating the underlying
    // ENOENT as a fatal error.
    let dir = tempfile::tempdir().expect("tempdir");
    let entry = ModuleEntry {
        name: "ghostly".to_owned(),
        path: dir.path().join("ghostly.ko.xz"),
        deps: Vec::new(),
    };
    let outcome = load_module(&entry).expect("must not error");
    assert!(matches!(outcome, LoadOutcome::FileMissing));
}

#[test]
fn decompress_module_surfaces_missing_file_as_io_error() {
    // Reading the file itself failing is a real IO error, not a
    // decompression error — surface it as `NmblError::Io` so
    // operators can see the path in the context message.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("does_not_exist.ko.xz");
    let err = decompress_module(&path, "widget").expect_err("must fail");
    match err {
        NmblError::Io { .. } => {}
        other => panic!("wrong variant: {other:?}"),
    }
}
