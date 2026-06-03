//! TPM transport: open `/dev/tpmrm0` and round-trip a marshaled command
//! frame to a response frame.
//!
//! Uses ONLY rustix safe wrappers (`rustix::fs::open` + `rustix::io::{read,
//! write}`) — NO `ioctl`, ZERO new `unsafe` (FIX-43). The same idioms are
//! used by `validate::hardware::read_luks_magic` (the rustix open + short-read
//! loop) and `rescue::net::download::write_all_to_fd` (the rustix write-all
//! loop); this module mirrors both.

use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use rustix::io::Errno as RustixErrno;

use crate::error::{NmblError, Result};

/// The Linux in-kernel TPM 2.0 resource-manager character device. Using the
/// resource manager (`tpmrm0`) rather than the raw `tpm0` lets the kernel
/// virtualize transient handles and serialize concurrent access; for our
/// purposes (a single `PcrExtend` / `PcrRead`) it is the correct,
/// always-available endpoint when a TPM is present.
pub const TPM_RM_DEVICE: &str = "/dev/tpmrm0";

/// A complete TPM response is never larger than this. The TPM 2.0 spec caps
/// a command/response at `TPM_MAX_COMMAND_SIZE` (4096); we round up to a 4 KiB
/// stack buffer so `transact` never heap-allocates a read scratch area and a
/// malicious/buggy device can never make us grow without bound.
const MAX_RESPONSE: usize = 4096;

/// The fixed TPM command/response header size (tag:u16 + size:u32 +
/// code:u32 = 10 bytes). The `size` field at offset 2 is the authoritative
/// total frame length, so `transact` reads the 10-byte header first and then
/// exactly `size - 10` more bytes.
const HEADER_LEN: usize = 10;

/// Offset of the big-endian `u32` total-size field within a TPM frame header.
const SIZE_OFFSET: usize = 2;

/// An open handle to the TPM resource-manager device.
///
/// The single `OwnedFd` is the only resource held; `Drop` closes it. No
/// `unsafe`, no `ioctl` — every operation is a plain `read(2)`/`write(2)`
/// through rustix.
#[derive(Debug)]
pub struct TpmDevice {
    fd: OwnedFd,
}

impl TpmDevice {
    /// Opens the default resource-manager device [`TPM_RM_DEVICE`]
    /// read-write (`O_CLOEXEC`). Returns `Err` if the device is absent or
    /// cannot be opened; the caller (the cap path) maps a clean "absent" to
    /// [`super::CapOutcome::NoTpm`] only after the deterministic sysfs
    /// presence check (FIX-28) — opening alone is not the presence oracle.
    pub fn open() -> Result<Self> {
        Self::open_path(Path::new(TPM_RM_DEVICE))
    }

    /// Opens an arbitrary TPM device path (used by tests against a fixture
    /// fd; production callers use [`TpmDevice::open`]).
    pub fn open_path(path: &Path) -> Result<Self> {
        let fd = rustix::fs::open(path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| tpm_io("open(/dev/tpmrm0)", e))?;
        Ok(Self { fd })
    }

    /// Wraps an already-open fd (test seam: a `socketpair`/`pipe`/regular
    /// file standing in for the device). Production code uses
    /// [`TpmDevice::open`].
    #[must_use]
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    /// Writes the whole `command` frame, then reads back one complete
    /// response frame.
    ///
    /// Robust against short writes (loop until drained) and short reads
    /// (read the 10-byte header, parse the authoritative `size` field, then
    /// read exactly the remainder). A response larger than [`MAX_RESPONSE`]
    /// or one that lies about its own size is rejected as a protocol error
    /// rather than over-read or truncated silently.
    pub fn transact(&self, command: &[u8]) -> Result<Vec<u8>> {
        self.write_all(command)?;
        self.read_response()
    }

    /// `write(2)` until the whole command frame is drained (mirrors
    /// `rescue::net::download::write_all_to_fd`).
    fn write_all(&self, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            let n = match rustix::io::write(&self.fd, buf) {
                Ok(n) => n,
                Err(RustixErrno::INTR) => continue,
                Err(e) => return Err(tpm_io("write(tpm command)", e)),
            };
            if n == 0 {
                return Err(NmblError::TpmProto {
                    context: "transact".to_string(),
                    reason: "write(tpm command) returned 0 (device closed)".to_string(),
                });
            }
            let Some(rest) = buf.get(n..) else {
                // `write` must return n ≤ buf.len(); a larger n is a contract
                // violation we surface rather than panic-index.
                let buf_len = buf.len();
                return Err(NmblError::TpmProto {
                    context: "transact".to_string(),
                    reason: format!("write(tpm command) returned n={n} > buf.len()={buf_len}"),
                });
            };
            buf = rest;
        }
        Ok(())
    }

    /// Reads exactly one response frame: the 10-byte header (to learn the
    /// total `size`), then the remaining `size - 10` bytes.
    fn read_response(&self) -> Result<Vec<u8>> {
        let mut frame = [0u8; MAX_RESPONSE];

        // 1) Fill the fixed header so we can read the size field.
        self.read_exact(frame.get_mut(..HEADER_LEN).unwrap_or_default())?;

        // 2) Parse the authoritative big-endian size at offset 2.
        let size_bytes =
            frame
                .get(SIZE_OFFSET..SIZE_OFFSET + 4)
                .ok_or_else(|| NmblError::TpmProto {
                    context: "transact".to_string(),
                    reason: "response header shorter than size field".to_string(),
                })?;
        let mut size_arr = [0u8; 4];
        size_arr.copy_from_slice(size_bytes);
        let total = u32::from_be_bytes(size_arr) as usize;

        if total < HEADER_LEN {
            return Err(NmblError::TpmProto {
                context: "transact".to_string(),
                reason: format!("response size field {total} < header length {HEADER_LEN}"),
            });
        }
        if total > MAX_RESPONSE {
            return Err(NmblError::TpmProto {
                context: "transact".to_string(),
                reason: format!("response size field {total} > max {MAX_RESPONSE}"),
            });
        }

        // 3) Read the remaining body into the frame buffer.
        let body = frame
            .get_mut(HEADER_LEN..total)
            .ok_or_else(|| NmblError::TpmProto {
                context: "transact".to_string(),
                reason: "response size field out of buffer bounds".to_string(),
            })?;
        self.read_exact(body)?;

        let out = frame.get(..total).ok_or_else(|| NmblError::TpmProto {
            context: "transact".to_string(),
            reason: "response frame slice out of bounds".to_string(),
        })?;
        Ok(out.to_vec())
    }

    /// `read(2)` until `buf` is full (mirrors
    /// `validate::hardware::read_luks_magic`'s short-read loop). A premature
    /// EOF (`read` returns 0 before the buffer is full) is a protocol error.
    fn read_exact(&self, buf: &mut [u8]) -> Result<()> {
        let mut filled = 0usize;
        while filled < buf.len() {
            let Some(tail) = buf.get_mut(filled..) else {
                break;
            };
            match rustix::io::read(&self.fd, tail) {
                Ok(0) => {
                    return Err(NmblError::TpmProto {
                        context: "transact".to_string(),
                        reason: format!(
                            "read(tpm response) hit EOF after {filled} of {} bytes",
                            buf.len()
                        ),
                    });
                }
                Ok(n) => filled += n,
                Err(RustixErrno::INTR) => continue,
                Err(e) => return Err(tpm_io("read(tpm response)", e)),
            }
        }
        Ok(())
    }
}

/// Bridge a rustix `Errno` from a TPM IO call into [`NmblError::TpmProto`].
/// The cap path turns any such error into [`super::CapOutcome::Failed`]
/// (fail-closed — FIX-27); we keep the OS errno text in `reason` for the log.
fn tpm_io(context: &str, e: RustixErrno) -> NmblError {
    let os = std::io::Error::from_raw_os_error(e.raw_os_error());
    NmblError::TpmProto {
        context: context.to_string(),
        reason: os.to_string(),
    }
}
