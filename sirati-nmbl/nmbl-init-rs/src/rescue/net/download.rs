//! HTTP download to a memfd + SHA-256 streaming hash.

use std::cell::Cell;
use std::io;

use rustix::fs::MemfdFlags;
use rustix::io::Errno as RustixErrno;
use sha2::{Digest, Sha256};

use crate::error::{NmblError, Result};
use crate::net::http::{self, HttpUrl};
use crate::sys::loopdev::{allocate_loop_device, configure_loop_device, open_loop_device};

use super::NetAttemptOutcome;
use super::types::{DownloadStatus, RescueUi};

/// Mountpoint where the downloaded squashfs is staged before the
/// `switch_root`. Mirrors `rescue::disk::RESCUE_MOUNT` so the operator
/// experience is identical whether the rescue blob came from disk
/// or from the network.
pub(super) const RESCUE_MOUNT: &str = "/rescue";

/// Stream the HTTP body into a `memfd_create(2)` in-RAM fd while
/// SHA-256ing every chunk. Returns the populated memfd (rewound to
/// offset 0) plus the lowercase-hex digest.
pub(super) fn download_to_memfd<R: RescueUi>(
    url: &HttpUrl,
    ui: &mut R,
) -> std::result::Result<(rustix::fd::OwnedFd, String), NetAttemptOutcome> {
    let memfd = rustix::fs::memfd_create("nmbl-rescue-sfs", MemfdFlags::CLOEXEC).map_err(|e| {
        NmblError::Rescue {
            stage: "net-memfd",
            source: Box::new(NmblError::Io {
                source: io_error_from_rustix(e),
                context: "memfd_create(nmbl-rescue-sfs)".to_string(),
            }),
        }
    })?;

    let mut hasher = Sha256::new();
    let mut total_written: u64 = 0;
    // `Cell` lets both the progress and the body-sink closures
    // mutate the captured `content_length` without tripping the
    // "two mutable borrows" rule — they each hold a shared
    // reference and use `set`/`get` for interior mutation.
    let content_length: Cell<Option<u64>> = Cell::new(None);

    let write_result = {
        let memfd_ref = &memfd;
        let hasher_ref = &mut hasher;
        let total_ref = &mut total_written;
        let ui_ref: &mut R = ui;
        let length_ref = &content_length;
        let mut progress_cb = |total: u64| {
            length_ref.set(Some(total));
        };
        let mut sink = |chunk: &[u8]| -> Result<()> {
            hasher_ref.update(chunk);
            write_all_to_fd(memfd_ref, chunk)?;
            *total_ref = total_ref.saturating_add(chunk.len() as u64);
            ui_ref.progress(DownloadStatus {
                bytes: *total_ref,
                total: length_ref.get(),
            });
            Ok(())
        };
        http::get(url, &mut sink, Some(&mut progress_cb))
    };
    let _ = write_result.map_err(NetAttemptOutcome::Fatal)?;

    // Rewind so the LOOP_CONFIGURE consumer reads from offset 0.
    rustix::fs::seek(&memfd, rustix::fs::SeekFrom::Start(0)).map_err(|e| {
        NetAttemptOutcome::Fatal(NmblError::Rescue {
            stage: "net-memfd",
            source: Box::new(NmblError::Io {
                source: io_error_from_rustix(e),
                context: "seek(memfd, 0)".to_string(),
            }),
        })
    })?;

    let digest = hasher.finalize();
    let hex = hex_lower(&digest);
    Ok((memfd, hex))
}

/// `rustix::io::write` until the chunk is drained — matches the loop
/// in `sys::kexec::load_with_extra_initrd_cpio`.
fn write_all_to_fd<F: rustix::fd::AsFd>(fd: F, mut buf: &[u8]) -> Result<()> {
    while !buf.is_empty() {
        let n = rustix::io::write(&fd, buf).map_err(|e| NmblError::Rescue {
            stage: "net-memfd",
            source: Box::new(NmblError::Io {
                source: io_error_from_rustix(e),
                context: "write(memfd)".to_string(),
            }),
        })?;
        if n == 0 {
            return Err(NmblError::Rescue {
                stage: "net-memfd",
                source: Box::new(NmblError::Io {
                    source: io::Error::from(io::ErrorKind::WriteZero),
                    context: "write(memfd) returned 0".to_string(),
                }),
            });
        }
        // `rustix::io::write` must return n ≤ buf.len(); anything else is
        // a contract violation we want to surface, not silently treat
        // as "done".
        let Some(rest) = buf.get(n..) else {
            let buf_len = buf.len();
            return Err(NmblError::Rescue {
                stage: "net-memfd",
                source: Box::new(NmblError::Io {
                    source: io::Error::from(io::ErrorKind::InvalidData),
                    context: format!("write(memfd) returned n={n} > buf.len()={buf_len}"),
                }),
            });
        };
        buf = rest;
    }
    Ok(())
}

/// Loop-mount the downloaded memfd squashfs and layer the same writable
/// overlay at `/rescue` the disk path uses, returning the mount path.
/// The caller funnels this into the shared chrooted child runner
/// ([`crate::rescue::child::run_external_rescue_child`]) so the
/// network and disk paths land on an identical writable rescue root —
/// the chrooted rescue `/init` needs to write into the root, which a
/// bare read-only squashfs mount cannot support.
pub(super) fn mount_overlay_for_child(
    backing: &rustix::fd::OwnedFd,
) -> Result<&'static std::path::Path> {
    use std::path::{Path, PathBuf};

    let index = allocate_loop_device().map_err(|source| NmblError::Rescue {
        stage: "loop-alloc",
        source: Box::new(source),
    })?;

    let loop_fd = open_loop_device(index, true).map_err(|source| NmblError::Rescue {
        stage: "loop-open",
        source: Box::new(source),
    })?;

    configure_loop_device(&loop_fd, backing, true).map_err(|source| NmblError::Rescue {
        stage: "loop-configure",
        source: Box::new(source),
    })?;

    let loop_dev = PathBuf::from(format!("/dev/loop{index}"));
    crate::rescue::disk::mount_overlay_root(&loop_dev)?;

    Ok(Path::new(RESCUE_MOUNT))
}

/// Lowercase hex encoder for SHA-256 digests. Avoids pulling in the
/// `hex` crate for a 64-byte string.
pub(super) fn hex_lower(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let hi = TABLE.get(usize::from(b >> 4)).copied().unwrap_or(b'?');
        let lo = TABLE.get(usize::from(b & 0x0f)).copied().unwrap_or(b'?');
        out.push(hi as char);
        out.push(lo as char);
    }
    out
}

/// Compute the lowercase-hex SHA-256 of `bytes`. Exposed so unit
/// tests can pin the hasher behaviour against a known vector without
/// rebuilding the full memfd plumbing. `#[cfg(test)]` because the
/// production flow inlines the hasher into the streaming sink.
#[cfg(test)]
pub(super) fn compute_hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

/// Bridge `rustix::io::Errno` → `std::io::Error`. Same shape as the
/// helper in `sys::loopdev` / `rescue::disk`.
pub(super) fn io_error_from_rustix(e: RustixErrno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}
