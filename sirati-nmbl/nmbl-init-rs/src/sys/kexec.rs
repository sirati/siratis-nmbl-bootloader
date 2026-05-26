//! `kexec_file_load(2)` + `reboot(LINUX_REBOOT_CMD_KEXEC)` wrappers.
//!
//! Replaces `scripts/kexec-boot.sh.nix`'s `kexec -s -l` / `kexec -e` shell
//! calls with direct syscalls. The caller is responsible for unmounting and
//! syncing before invoking [`execute`] — this module only touches the kexec
//! image slot and the reboot syscall.

use std::convert::Infallible;
use std::ffi::CString;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use nix::errno::Errno;
use nix::fcntl::{OFlag, open};
use nix::sys::reboot::{RebootMode, reboot};
use nix::sys::stat::Mode;

use crate::error::{NmblError, Result};

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
    let kernel_fd =
        open(kernel, OFlag::O_RDONLY | OFlag::O_CLOEXEC, Mode::empty()).map_err(|e| {
            NmblError::KexecLoad {
                kernel: kernel.to_path_buf(),
                initrd: initrd.map(Path::to_path_buf),
                source: e,
            }
        })?;

    let (initrd_fd_opt, effective_flags) = match initrd {
        Some(path) => {
            let fd =
                open(path, OFlag::O_RDONLY | OFlag::O_CLOEXEC, Mode::empty()).map_err(|e| {
                    NmblError::KexecLoad {
                        kernel: kernel.to_path_buf(),
                        initrd: Some(path.to_path_buf()),
                        source: e,
                    }
                })?;
            (Some(fd), flags)
        }
        None => (None, flags | KEXEC_FILE_NO_INITRAMFS),
    };

    let (cmdline_buf, cmdline_len) = build_cmdline_cstring(cmdline, kernel, initrd)?;

    let initrd_raw: libc::c_int = match initrd_fd_opt.as_ref() {
        Some(fd) => fd.as_raw_fd(),
        None => -1,
    };

    // SAFETY: kernel_fd and (when present) initrd_fd are live OwnedFds we
    // hold for the duration of the call. cmdline_buf is a Vec we own that
    // outlives the syscall. The syscall reads, never writes, our buffers.
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
            initrd: initrd.map(Path::to_path_buf),
            source: nix::Error::from(Errno::last()),
        });
    }
    Ok(())
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
