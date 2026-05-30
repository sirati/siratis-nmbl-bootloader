//! `kexec_file_load(2)` + `reboot(LINUX_REBOOT_CMD_KEXEC)` wrappers.
//!
//! Replaces `scripts/kexec-boot.sh.nix`'s `kexec -s -l` / `kexec -e` shell
//! calls with direct syscalls. The caller is responsible for unmounting and
//! syncing before invoking [`execute`] — this module only touches the kexec
//! image slot and the reboot syscall.

use std::convert::Infallible;
use std::ffi::CString;
use std::os::fd::OwnedFd;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::reboot::{RebootMode, reboot};
use rustix::fs::{MemfdFlags, Mode, OFlags};

use crate::error::{NmblError, Result};

/// Post-`sync(2)` settle window before the kexec handoff. `sync` only
/// schedules writeback; real hardware needs a beat to commit before the
/// caller cuts the mounts out. Lives here (next to the genuine kexec
/// load) so [`crate::sys::ops::RealSys::kexec_load`] can apply it inside
/// the seam while a dry-run impl no-ops it.
pub const POST_SYNC_FLUSH: Duration = Duration::from_millis(50);

/// `KEXEC_FILE_NO_INITRAMFS` — passed when no initrd is supplied.
pub const KEXEC_FILE_NO_INITRAMFS: u32 = 0x0000_0004;
/// `KEXEC_FILE_ON_CRASH` — load into the crashkernel slot instead.
pub const KEXEC_FILE_ON_CRASH: u32 = 0x0000_0001;
/// `KEXEC_FILE_PRESERVE_CTX` — preserve userspace context across kexec.
pub const KEXEC_FILE_PRESERVE_CTX: u32 = 0x0000_0002;

/// Build the null-terminated kernel cmdline.
///
/// Returns `(bytes_with_nul, len_including_nul)`. The kernel expects the
/// length to include the terminating NUL byte. Embedded NUL bytes are
/// rejected (they cannot be expressed in a C string). On failure we
/// synthesize a `KexecLoad` error attributing the failure to the kernel
/// path being loaded so the operator can see which entry the bad cmdline
/// belongs to.
fn build_cmdline_cstring(
    cmdline: &str,
    kernel: &Path,
    initrd: Option<&Path>,
) -> Result<(Vec<u8>, usize)> {
    let c = CString::new(cmdline).map_err(|_| NmblError::KexecLoad {
        kernel: kernel.to_path_buf(),
        initrd: initrd.map(Path::to_path_buf),
        source: nix::Error::from(Errno::EINVAL),
    })?;
    let bytes = c.into_bytes_with_nul();
    let len = bytes.len();
    Ok((bytes, len))
}

/// Load the kernel + (optional) initrd + cmdline into the kexec image slot.
///
/// When `initrd` is `None`, [`KEXEC_FILE_NO_INITRAMFS`] is OR-ed into
/// `flags` automatically and `-1` is passed as the initrd fd. Both file
/// descriptors are opened `O_RDONLY | O_CLOEXEC` and closed at drop.
pub fn load(kernel: &Path, initrd: Option<&Path>, cmdline: &str, flags: u32) -> Result<()> {
    load_with_initrd_fd(kernel, initrd, None, cmdline, flags)
}

/// Like [`load`], but the initrd is supplied as a `memfd`-style
/// in-memory file descriptor (anything `kexec_file_load(2)` will accept
/// as an fd argument). `initrd_path_for_errors` is the path the
/// `initrd` buffer was *derived from* — only used to enrich
/// `NmblError::KexecLoad`'s context fields when the syscall fails, and
/// passed through as-is so the operator sees the same path they'd see
/// without injection.
///
/// Internal helper for [`load_with_extra_initrd_cpio`]. Prefer that
/// for the common case.
fn load_with_initrd_fd(
    kernel: &Path,
    initrd_path_for_errors: Option<&Path>,
    initrd_fd: Option<&OwnedFd>,
    cmdline: &str,
    flags: u32,
) -> Result<()> {
    let kernel_fd = rustix::fs::open(kernel, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| NmblError::KexecLoad {
            kernel: kernel.to_path_buf(),
            initrd: initrd_path_for_errors.map(Path::to_path_buf),
            source: nix::Error::from_raw(e.raw_os_error()),
        })?;

    // Open the initrd file if no caller-supplied fd was provided.
    let opened_initrd: Option<OwnedFd> = if initrd_fd.is_none() {
        match initrd_path_for_errors {
            Some(path) => Some(
                rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).map_err(
                    |e| NmblError::KexecLoad {
                        kernel: kernel.to_path_buf(),
                        initrd: Some(path.to_path_buf()),
                        source: nix::Error::from_raw(e.raw_os_error()),
                    },
                )?,
            ),
            None => None,
        }
    } else {
        None
    };

    let (cmdline_buf, cmdline_len) =
        build_cmdline_cstring(cmdline, kernel, initrd_path_for_errors)?;

    let (initrd_raw, effective_flags): (libc::c_int, u32) = match (initrd_fd, &opened_initrd) {
        (Some(fd), _) => (fd.as_raw_fd(), flags),
        (None, Some(fd)) => (fd.as_raw_fd(), flags),
        (None, None) => (-1, flags | KEXEC_FILE_NO_INITRAMFS),
    };

    // SAFETY: Unavoidable raw syscall.
    //   * Why no safe wrapper: no Rust crate wraps `kexec_file_load(2)`
    //     — `nix` 0.29 only wraps `reboot(LINUX_REBOOT_CMD_KEXEC)`, not
    //     the loader; `rustix` 0.38 has no covering API in the `system`
    //     or `process` modules; `libkexec` is a C library we refuse to
    //     link from PID 1.
    //   * Why this is safe: `kernel_fd` and (when present) `initrd_fd`
    //     are live `OwnedFd`s held by this function for the duration
    //     of the call. `cmdline_buf` is a Vec we own that outlives the
    //     syscall. The kernel reads, never writes, our buffers; the
    //     return value + errno report failure.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_kexec_file_load,
            kernel_fd.as_raw_fd() as libc::c_int,
            initrd_raw,
            cmdline_len as libc::c_ulong,
            cmdline_buf.as_ptr() as *const libc::c_char,
            effective_flags as libc::c_ulong,
        )
    };

    if rc < 0 {
        return Err(NmblError::KexecLoad {
            kernel: kernel.to_path_buf(),
            initrd: initrd_path_for_errors.map(Path::to_path_buf),
            source: nix::Error::from(Errno::last()),
        });
    }
    Ok(())
}

/// Like [`load`], but appends `extra_cpio` (an uncompressed cpio
/// fragment, see [`crate::sys::cpio`]) after the system initrd in
/// memory and hands the combined buffer to `kexec_file_load(2)` via a
/// `memfd_create`'d anonymous file descriptor. Used by the LUKS-
/// passphrase pass-through path so the typed passphrase never touches
/// disk: it lives in a `Zeroizing<Vec<u8>>` until written to the memfd
/// (kernel-owned, anonymous memory backed by tmpfs that no path
/// references), and the buffer drops + zeroes the moment we return.
pub fn load_with_extra_initrd_cpio(
    kernel: &Path,
    initrd: &Path,
    extra_cpio: &[u8],
    cmdline: &str,
    flags: u32,
) -> Result<()> {
    let mut combined: Vec<u8> = std::fs::read(initrd).map_err(|e| NmblError::KexecLoad {
        kernel: kernel.to_path_buf(),
        initrd: Some(initrd.to_path_buf()),
        source: nix::Error::from_raw(e.raw_os_error().unwrap_or(libc::EIO)),
    })?;
    // 4-byte align before the next concatenated archive — Linux's
    // initrd unpacker accepts NUL padding between archives.
    while !combined.len().is_multiple_of(4) {
        combined.push(0);
    }
    combined.extend_from_slice(extra_cpio);

    let memfd: OwnedFd =
        rustix::fs::memfd_create("nmbl-initrd", MemfdFlags::CLOEXEC).map_err(|e| {
            NmblError::KexecLoad {
                kernel: kernel.to_path_buf(),
                initrd: Some(initrd.to_path_buf()),
                source: nix::Error::from_raw(e.raw_os_error()),
            }
        })?;
    // Write the combined buffer to the memfd via rustix so we don't
    // consume the OwnedFd into a `File` (we still need it for kexec).
    let mut remaining = combined.as_slice();
    while !remaining.is_empty() {
        let n = rustix::io::write(&memfd, remaining).map_err(|e| NmblError::KexecLoad {
            kernel: kernel.to_path_buf(),
            initrd: Some(initrd.to_path_buf()),
            source: nix::Error::from_raw(e.raw_os_error()),
        })?;
        if n == 0 {
            return Err(NmblError::KexecLoad {
                kernel: kernel.to_path_buf(),
                initrd: Some(initrd.to_path_buf()),
                source: nix::Error::from(Errno::EIO),
            });
        }
        remaining = remaining.get(n..).unwrap_or(&[]);
    }
    // Rewind so `kexec_file_load` reads from offset 0.
    rustix::fs::seek(&memfd, rustix::fs::SeekFrom::Start(0)).map_err(|e| NmblError::KexecLoad {
        kernel: kernel.to_path_buf(),
        initrd: Some(initrd.to_path_buf()),
        source: nix::Error::from_raw(e.raw_os_error()),
    })?;

    load_with_initrd_fd(kernel, Some(initrd), Some(&memfd), cmdline, flags)
}

/// Execute the previously loaded kexec image.
///
/// On success this call does not return — the running kernel is replaced
/// in-place. The return type is [`Infallible`] inside [`Result`] so callers
/// can treat any return at all as a failure. The caller is responsible for
/// having unmounted filesystems and called `sync(2)` beforehand.
pub fn execute() -> Result<Infallible> {
    match reboot(RebootMode::RB_KEXEC) {
        Ok(_) => Err(NmblError::KexecReturned {
            stage: "exec",
            source: nix::Error::from(Errno::EIO),
        }),
        Err(e) => Err(NmblError::KexecReturned {
            stage: "exec",
            source: e,
        }),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]
mod tests {
    use super::*;

    #[test]
    fn cmdline_empty_is_just_nul() {
        let kernel = Path::new("/boot/vmlinuz-test");
        let (buf, len) =
            build_cmdline_cstring("", kernel, None).expect("empty cmdline must succeed");
        assert_eq!(len, 1, "len must be 1 (the NUL)");
        assert_eq!(buf.as_slice(), b"\0");
    }

    #[test]
    fn cmdline_typical_includes_nul() {
        let kernel = Path::new("/boot/vmlinuz-test");
        let s = "init=/sbin/init root=/dev/sda1";
        let (buf, len) =
            build_cmdline_cstring(s, kernel, None).expect("typical cmdline must succeed");
        assert_eq!(len, s.len() + 1, "len must be byte length + 1 for NUL");
        assert_eq!(buf.len(), len, "buffer length must equal reported len");
        let last = match buf.last() {
            Some(b) => *b,
            None => panic!("buffer must be non-empty"),
        };
        assert_eq!(last, 0, "buffer must be NUL-terminated");
    }

    #[test]
    fn cmdline_embedded_nul_is_rejected() {
        let kernel = Path::new("/boot/vmlinuz-test");
        let res = build_cmdline_cstring("init=/sbin/init\0root=/dev/sda1", kernel, None);
        assert!(res.is_err(), "embedded NUL must produce an error");
        match res {
            Err(NmblError::KexecLoad {
                kernel: k, source, ..
            }) => {
                assert_eq!(k, kernel);
                assert_eq!(source, nix::Error::from(Errno::EINVAL));
            }
            _ => panic!("expected NmblError::KexecLoad for embedded-NUL cmdline"),
        }
    }

    #[test]
    fn flag_constants_match_kernel_uapi() {
        // Spot-check the bit values against linux/kexec.h.
        assert_eq!(KEXEC_FILE_ON_CRASH, 0x1);
        assert_eq!(KEXEC_FILE_PRESERVE_CTX, 0x2);
        assert_eq!(KEXEC_FILE_NO_INITRAMFS, 0x4);
    }
}
