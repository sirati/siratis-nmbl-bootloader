//! `kexec_file_load(2)` + `reboot(LINUX_REBOOT_CMD_KEXEC)` wrappers.
//!
//! Replaces `scripts/kexec-boot.sh.nix`'s `kexec -s -l` / `kexec -e` shell
//! calls with direct syscalls. The caller is responsible for unmounting and
//! syncing before invoking [`execute`] — this module only touches the kexec
//! image slot and the reboot syscall.

use std::convert::Infallible;
use std::ffi::CString;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::io::AsRawFd;
use std::path::Path;

use nix::errno::Errno;
use nix::sys::reboot::{RebootMode, reboot};
use rustix::fs::{MemfdFlags, Mode, OFlags};

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
    load_with_initrd_fd(kernel, None, initrd, None, cmdline, flags)
}

/// Like [`load`], but the initrd is supplied as a `memfd`-style
/// in-memory file descriptor (anything `kexec_file_load(2)` will accept
/// as an fd argument). `initrd_path_for_errors` is the path the
/// `initrd` buffer was *derived from* — only used to enrich
/// `NmblError::KexecLoad`'s context fields when the syscall fails, and
/// passed through as-is so the operator sees the same path they'd see
/// without injection.
///
/// When `kernel_fd` is `Some`, that PINNED, already-open kernel fd is loaded
/// directly and the `kernel` PATH is NEVER re-opened — this is the secure-boot
/// path closing the verify→measure→load TOCTOU (FIX-02 / MED-1): the bytes
/// loaded are byte-identical to the bytes verified+measured. When `kernel_fd`
/// is `None` (the non-secure-boot path, where no verify fd exists), the kernel
/// is opened by path here as before — `kernel` then doubles as the load source.
///
/// Internal helper for [`load_with_extra_initrd_cpio`]. Prefer that
/// for the common case.
fn load_with_initrd_fd(
    kernel: &Path,
    kernel_fd: Option<BorrowedFd<'_>>,
    initrd_path_for_errors: Option<&Path>,
    initrd_fd: Option<&OwnedFd>,
    cmdline: &str,
    flags: u32,
) -> Result<()> {
    // Reuse the verified, pinned kernel fd when supplied; only open-by-path on
    // the non-secure-boot path where no such fd exists (FIX-02 / MED-1).
    let opened_kernel: Option<OwnedFd> = match kernel_fd {
        Some(_) => None,
        None => Some(
            rustix::fs::open(kernel, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty()).map_err(
                |e| NmblError::KexecLoad {
                    kernel: kernel.to_path_buf(),
                    initrd: initrd_path_for_errors.map(Path::to_path_buf),
                    source: nix::Error::from_raw(e.raw_os_error()),
                },
            )?,
        ),
    };
    let kernel_raw: libc::c_int = match (kernel_fd, &opened_kernel) {
        (Some(fd), _) => fd.as_raw_fd(),
        (None, Some(fd)) => fd.as_raw_fd(),
        // Unreachable: exactly one of the two arms above is always set.
        (None, None) => -1,
    };

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
    //   * Why this is safe: the kernel fd (`kernel_raw`) is either the
    //     caller's pinned `BorrowedFd` — kept live by the caller across
    //     this call — or the `opened_kernel` `OwnedFd` held by this
    //     function for the duration of the call; `initrd_fd` (when present)
    //     is likewise a live fd. `cmdline_buf` is a Vec we own that
    //     outlives the syscall. The kernel reads, never writes, our
    //     buffers; the return value + errno report failure.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_kexec_file_load,
            kernel_raw,
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
    // Non-secure-boot path: no verified fds to pin, so the kernel AND the
    // initrd are opened by path (no pinned initrd source fd).
    load_cpio_core(kernel, None, initrd, None, extra_cpio, cmdline, flags)
}

/// Like [`load_with_extra_initrd_cpio`], but loads the PINNED, already-verified
/// kernel + initrd fds directly (FIX-02 / MED-1 / LOW-A) instead of re-opening
/// either by path. `kernel_fd` is the fd the signature verifier opened, hashed,
/// and verified; `initrd_src_fd` is the verifier's pinned initrd fd — the
/// initrd bytes spliced with `extra_cpio` into the kexec memfd are read from
/// THAT fd (seek-to-0 then read-to-end), never re-read from `initrd` (the path
/// is carried only for error context). So both the kernel the new image runs
/// AND the pristine initrd it unpacks are byte-identical to the ones that were
/// verified and measured.
pub fn load_with_kernel_fd_and_extra_initrd_cpio(
    kernel_path: &Path,
    kernel_fd: BorrowedFd<'_>,
    initrd: &Path,
    initrd_src_fd: BorrowedFd<'_>,
    extra_cpio: &[u8],
    cmdline: &str,
    flags: u32,
) -> Result<()> {
    load_cpio_core(
        kernel_path,
        Some(kernel_fd),
        initrd,
        Some(initrd_src_fd),
        extra_cpio,
        cmdline,
        flags,
    )
}

/// Read the WHOLE pristine initrd into memory for the cpio splice.
///
/// LOW-A: when `initrd_src_fd` is `Some` (the secure-boot path), the bytes are
/// read from THAT verifier-pinned fd — seek-to-0 first so a previously-hashed fd
/// is read from its start, then read to EOF — never re-read from `initrd` by
/// path. The bytes spliced into the kexec memfd are then byte-identical to the
/// ones verified + measured. When `None` (non-secure-boot), fall back to reading
/// the path. `kernel`/`initrd` are carried only for error context.
fn read_initrd_bytes(
    kernel: &Path,
    initrd: &Path,
    initrd_src_fd: Option<BorrowedFd<'_>>,
) -> Result<Vec<u8>> {
    let kexec_err = |e: rustix::io::Errno| NmblError::KexecLoad {
        kernel: kernel.to_path_buf(),
        initrd: Some(initrd.to_path_buf()),
        source: nix::Error::from_raw(e.raw_os_error()),
    };
    match initrd_src_fd {
        Some(fd) => {
            // Seek the pinned fd to 0 (the verify hashed it, leaving it at EOF)
            // and read the whole image from it (LOW-A — no path re-read).
            rustix::fs::seek(fd, rustix::fs::SeekFrom::Start(0)).map_err(kexec_err)?;
            let mut buf = Vec::new();
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let n = rustix::io::read(fd, &mut chunk).map_err(kexec_err)?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(chunk.get(..n).unwrap_or(&[]));
            }
            Ok(buf)
        }
        None => std::fs::read(initrd).map_err(|e| NmblError::KexecLoad {
            kernel: kernel.to_path_buf(),
            initrd: Some(initrd.to_path_buf()),
            source: nix::Error::from_raw(e.raw_os_error().unwrap_or(libc::EIO)),
        }),
    }
}

/// Shared body of the cpio-injecting load: read the initrd bytes (from the
/// pinned `initrd_src_fd` when supplied — LOW-A — else by path), append the cpio
/// fragment into a memfd, and hand it (plus the kernel fd / path) to
/// [`load_with_initrd_fd`]. `kernel_fd` is `Some` for the verified secure-boot
/// path (pin the verified fd — FIX-02) and `None` otherwise (open by path);
/// `initrd_src_fd` follows the same secure-boot/non-secure-boot split.
fn load_cpio_core(
    kernel: &Path,
    kernel_fd: Option<BorrowedFd<'_>>,
    initrd: &Path,
    initrd_src_fd: Option<BorrowedFd<'_>>,
    extra_cpio: &[u8],
    cmdline: &str,
    flags: u32,
) -> Result<()> {
    let mut combined: Vec<u8> = read_initrd_bytes(kernel, initrd, initrd_src_fd)?;
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

    load_with_initrd_fd(
        kernel,
        kernel_fd,
        Some(initrd),
        Some(&memfd),
        cmdline,
        flags,
    )
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
#[path = "kexec_tests.rs"]
mod tests;
