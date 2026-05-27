//! Network-rescue orchestrator (PLAN.md Phase E.1).
//!
//! Drives the fallback path that activates when the disk-rescue
//! arm of [`super::dispatch`] fails (or there is no `nmbl-rescue.sfs`
//! on the boot partition to begin with). The flow is, in order:
//!
//! 1. Enumerate Ethernet interfaces via [`crate::net::iface`] and
//!    pick the first one that comes up with a carrier.
//! 2. Acquire a DHCPv4 lease on that interface with
//!    [`crate::net::dhcp::acquire`].
//! 3. Configure the interface (IP, netmask, default route) with the
//!    granted lease.
//! 4. Prompt the operator (via the [`RescueUi`] trait) for the rescue
//!    URL — pre-filled from `rescue.default_url` — and the expected
//!    SHA-256 hex.
//! 5. Open a `memfd_create(2)` in-RAM fd and stream the HTTP body
//!    through `sha2::Sha256` and `rustix::io::write` in one pass.
//! 6. Show the computed hash to the operator and let them confirm
//!    against the pre-filled expected value.
//! 7. Loop-mount the memfd at `/rescue`, `switch_root` into it via
//!    the shared [`super::switch_root_and_exec`] helper, and
//!    `execve("/bin/sh", …)`.
//!
//! [`RescueUi`] is a trait so the TUI (E.2) can later plug in a
//! ratatui-backed implementation while this module stays
//! end-to-end testable with a stdin/stdout [`ConsoleRescueUi`] or a
//! canned-answer fake.
//!
//! All failure points map onto [`NmblError::Rescue { stage, ... }`]
//! so the emergency-shell banner surfaces a structured cause.

use std::cell::Cell;
use std::convert::Infallible;
use std::io::{self, Write as _};
use std::mem;
use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

use nix::sys::socket::{AddressFamily, SockFlag, SockType, socket};
use rustix::fs::MemfdFlags;
use rustix::io::Errno as RustixErrno;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::net::dhcp::{self, DhcpLease};
use crate::net::http::{self, HttpUrl};
use crate::net::iface::{self, Interface};
use crate::nmbl_warn;
use crate::sys::loopdev::{allocate_loop_device, configure_loop_device, open_loop_device};
use crate::sys::mount::mount_fs;

/// Mountpoint where the downloaded squashfs is staged before the
/// `switch_root`. Mirrors `rescue::disk::RESCUE_MOUNT` so the operator
/// experience is identical whether the rescue blob came from disk
/// or from the network.
const RESCUE_MOUNT: &str = "/rescue";
/// Per-NIC link-up wait. 10s rides out PHY autonegotiation on
/// gigabit copper without burning the operator's patience.
const LINK_WAIT: Duration = Duration::from_secs(10);
/// Per-NIC DHCP exchange budget. 30s covers a slow embedded relay
/// without making a dead network feel hung forever.
const DHCP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// UI contract
// ---------------------------------------------------------------------------

/// Snapshot of in-flight download progress used by [`RescueUi::progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadStatus {
    /// Bytes received so far.
    pub bytes: u64,
    /// Total bytes expected from the HTTP `Content-Length` header.
    /// `None` when the origin closed the connection to signal EOF
    /// instead.
    pub total: Option<u64>,
}

/// Three-way operator choice from the source picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescueSource {
    /// Attempt the network-rescue path.
    Network,
    /// Reboot the system (operator opts out of rescue entirely).
    Reboot,
    /// Halt the system (operator opts out of rescue entirely).
    Halt,
}

/// Outcome of the hash-confirm screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashConfirmation {
    /// Operator confirmed the computed hash matches the expected one.
    Confirmed,
    /// Operator flagged the hashes as mismatched; redownload.
    Mismatch,
    /// Operator aborted the whole rescue attempt.
    Aborted,
}

/// Interaction surface the network-rescue orchestrator needs. E.2
/// supplies a ratatui-backed implementation; until then the
/// `ConsoleRescueUi` here keeps the path testable end-to-end.
pub trait RescueUi {
    /// Source picker. `disk_reason` is the error chain from the most
    /// recent disk-rescue attempt — surfaced verbatim so the operator
    /// knows why they're here.
    fn pick_source(&mut self, disk_reason: &str) -> Result<RescueSource>;

    /// URL entry screen, pre-filled with `prefill`. Returns the final
    /// URL the operator confirmed (empty string allowed only when the
    /// caller re-validates).
    fn prompt_url(&mut self, prefill: &str) -> Result<String>;

    /// Progress callback while bytes are streaming. Called at least
    /// once per chunk; implementations should be cheap.
    fn progress(&mut self, status: DownloadStatus);

    /// Hash confirm screen — show the computed hex digest, let the
    /// operator confirm against `prefill_expected`.
    fn confirm_hash(
        &mut self,
        computed_hex: &str,
        prefill_expected: &str,
    ) -> Result<HashConfirmation>;
}

// ---------------------------------------------------------------------------
// Public entrypoint
// ---------------------------------------------------------------------------

/// Run the full network-rescue flow.
///
/// `disk_reason` is the formatted error chain of the disk-rescue
/// attempt that triggered the fallback; it is shown verbatim on the
/// source-picker screen. On success this function does not return —
/// the process is replaced by the rescue shell.
///
/// When `config.rescue.network` is `false` the function short-circuits
/// with `NmblError::Rescue { stage: "net-disabled", ... }`, letting
/// the caller fall back to a halt-with-banner.
pub fn try_network_rescue<R: RescueUi>(
    config: &Config,
    ui: &mut R,
    disk_reason: &str,
) -> Result<Infallible> {
    if !config.rescue.network {
        return Err(NmblError::Rescue {
            stage: "net-disabled",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "network rescue is disabled in [rescue].network".to_string(),
                context: "entering try_network_rescue".to_string(),
            }),
        });
    }

    // Outer loop so the operator can redownload after a hash mismatch
    // without re-running the whole DHCP exchange.
    let mut latest_reason = disk_reason.to_string();
    loop {
        match ui.pick_source(&latest_reason)? {
            RescueSource::Reboot => return reboot_system(),
            RescueSource::Halt => return halt_system(),
            RescueSource::Network => {}
        }

        match run_network_attempt(config, ui) {
            Ok(infallible) => match infallible {},
            Err(NetAttemptOutcome::Restart(reason)) => {
                // Mismatched hash / operator-aborted download — show
                // the picker again with the updated reason so they
                // know which step failed this round.
                latest_reason = reason;
                continue;
            }
            Err(NetAttemptOutcome::Fatal(e)) => return Err(e),
        }
    }
}

/// Internal flow control for [`try_network_rescue`]. `Restart` loops
/// back to the source picker; `Fatal` aborts the whole rescue and
/// propagates the error to the caller (which will halt-with-banner).
enum NetAttemptOutcome {
    Restart(String),
    Fatal(NmblError),
}

impl From<NmblError> for NetAttemptOutcome {
    fn from(e: NmblError) -> Self {
        NetAttemptOutcome::Fatal(e)
    }
}

/// One trip through "bring up NIC + DHCP + download + verify + pivot".
/// Returns `Infallible` on the success path (process is replaced),
/// `NetAttemptOutcome::Restart` on operator-driven retries, and
/// `NetAttemptOutcome::Fatal` for non-recoverable errors.
fn run_network_attempt<R: RescueUi>(
    config: &Config,
    ui: &mut R,
) -> std::result::Result<Infallible, NetAttemptOutcome> {
    let (iface, lease) = bring_up_and_dhcp()?;
    apply_lease(&iface, &lease)?;

    let prefill_url = config.rescue.default_url.as_str();
    let url_str = ui
        .prompt_url(prefill_url)
        .map_err(NetAttemptOutcome::Fatal)?;
    let url = HttpUrl::parse(&url_str).map_err(NetAttemptOutcome::Fatal)?;

    let (memfd, computed_hex) = download_to_memfd(&url, ui)?;

    let prefill_hash = config.rescue.default_sha256.as_str();
    match ui
        .confirm_hash(&computed_hex, prefill_hash)
        .map_err(NetAttemptOutcome::Fatal)?
    {
        HashConfirmation::Confirmed => {}
        HashConfirmation::Mismatch => {
            // Drop the memfd by letting it fall out of scope. squashfs
            // bytes are not secret so no zeroize pass is required.
            drop(memfd);
            return Err(NetAttemptOutcome::Restart(format!(
                "hash mismatch: computed {computed_hex} did not match expected"
            )));
        }
        HashConfirmation::Aborted => {
            drop(memfd);
            return Err(NetAttemptOutcome::Restart(
                "operator aborted at hash confirmation".to_string(),
            ));
        }
    }

    // From here on we are committed to the rescue shell — any error
    // is fatal because we've already switched root (or are about to).
    mount_and_switch_root(&memfd).map_err(NetAttemptOutcome::Fatal)
}

// ---------------------------------------------------------------------------
// Network bring-up
// ---------------------------------------------------------------------------

/// Enumerate Ethernet NICs, bring each up in turn, and run DHCP on
/// the first one with a live carrier. Returns the interface chosen +
/// the granted lease.
fn bring_up_and_dhcp() -> Result<(Interface, DhcpLease)> {
    let candidates = iface::enumerate()?;
    if candidates.is_empty() {
        return Err(NmblError::Rescue {
            stage: "net-no-iface",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "no ARPHRD_ETHER interfaces found under /sys/class/net".to_string(),
                context: "enumerating NICs for network rescue".to_string(),
            }),
        });
    }

    let mut last_err: Option<NmblError> = None;
    for cand in &candidates {
        if let Err(e) = iface::bring_up(&cand.name) {
            nmbl_warn!(
                "network rescue: bring_up({}) failed: {e}; trying next NIC",
                cand.name
            );
            last_err = Some(e);
            continue;
        }
        match iface::wait_for_link(&cand.name, LINK_WAIT) {
            Ok(true) => {}
            Ok(false) => {
                nmbl_warn!(
                    "network rescue: no carrier on {} after {:?}; trying next NIC",
                    cand.name,
                    LINK_WAIT,
                );
                continue;
            }
            Err(e) => {
                nmbl_warn!(
                    "network rescue: wait_for_link({}) failed: {e}; trying next NIC",
                    cand.name,
                );
                last_err = Some(e);
                continue;
            }
        }

        match dhcp::acquire(cand, DHCP_TIMEOUT) {
            Ok(lease) => return Ok((cand.clone(), lease)),
            Err(e) => {
                nmbl_warn!(
                    "network rescue: DHCP on {} failed: {e}; trying next NIC",
                    cand.name
                );
                last_err = Some(e);
                continue;
            }
        }
    }

    Err(last_err.unwrap_or(NmblError::Rescue {
        stage: "net-no-iface",
        source: Box::new(NmblError::ConfigInvalid {
            reason: "no candidate NIC produced a DHCP lease".to_string(),
            context: format!("exhausted {} NIC(s)", candidates.len()),
        }),
    }))
}

/// Push the granted lease onto the interface: SIOCSIFADDR for the IP,
/// SIOCSIFNETMASK for the mask, and (when present) SIOCADDRT to
/// install a default route through the lease's gateway.
fn apply_lease(iface: &Interface, lease: &DhcpLease) -> Result<()> {
    let sock = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(|source| NmblError::Rescue {
        stage: "net-apply-lease",
        source: Box::new(NmblError::Io {
            source: io::Error::from_raw_os_error(source as i32),
            context: format!("socket(AF_INET, SOCK_DGRAM) for {}", iface.name),
        }),
    })?;

    let mut req = blank_ifreq_with_addr(&iface.name, lease.ip)?;
    // SAFETY: SIOCSIFADDR reads `ifr_name` and `ifr_ifru.ifru_addr`
    // (an sockaddr_in stuffed into the union by
    // `blank_ifreq_with_addr`). Both are initialized above; the
    // socket fd is live for the duration of the call.
    let rc = unsafe {
        libc::ioctl(
            sock.as_raw_fd(),
            libc::SIOCSIFADDR as _,
            &req as *const libc::ifreq,
        )
    };
    if rc < 0 {
        return Err(NmblError::Rescue {
            stage: "net-apply-lease",
            source: Box::new(NmblError::Io {
                source: io::Error::last_os_error(),
                context: format!("SIOCSIFADDR {} -> {}", iface.name, lease.ip),
            }),
        });
    }

    // SIOCSIFNETMASK — reuse the ifreq but swap in the netmask.
    stuff_sockaddr_in(&mut req, lease.netmask);
    // SAFETY: identical preconditions to the SIOCSIFADDR call above;
    // only the in-union sockaddr_in payload changed.
    let rc = unsafe {
        libc::ioctl(
            sock.as_raw_fd(),
            libc::SIOCSIFNETMASK as _,
            &req as *const libc::ifreq,
        )
    };
    if rc < 0 {
        return Err(NmblError::Rescue {
            stage: "net-apply-lease",
            source: Box::new(NmblError::Io {
                source: io::Error::last_os_error(),
                context: format!("SIOCSIFNETMASK {} -> {}", iface.name, lease.netmask),
            }),
        });
    }

    if let Some(gw) = lease.gateway {
        add_default_route(sock.as_raw_fd(), gw)?;
    }
    Ok(())
}

/// Build a `libc::ifreq` whose `ifr_name` is populated and whose
/// union slot already carries the supplied IPv4 address as a
/// `sockaddr_in`. Caller mutates with [`stuff_sockaddr_in`] to reuse
/// the ifreq for SIOCSIFNETMASK.
fn blank_ifreq_with_addr(name: &str, addr: Ipv4Addr) -> Result<libc::ifreq> {
    if name.len() >= libc::IFNAMSIZ {
        return Err(NmblError::Rescue {
            stage: "net-apply-lease",
            source: Box::new(NmblError::ConfigInvalid {
                reason: format!(
                    "interface name {name:?} exceeds IFNAMSIZ ({})",
                    libc::IFNAMSIZ
                ),
                context: "preparing ifreq for SIOCSIFADDR".to_string(),
            }),
        });
    }
    // SAFETY: `libc::ifreq` is POD; the all-zero bit pattern is a
    // valid value. Same justification as `iface::blank_ifreq`.
    let mut req: libc::ifreq = unsafe { mem::zeroed() };
    for (slot, byte) in req.ifr_name.iter_mut().zip(name.as_bytes()) {
        *slot = *byte as libc::c_char;
    }
    stuff_sockaddr_in(&mut req, addr);
    Ok(req)
}

/// Overwrite the union slot of `req` with an IPv4 `sockaddr_in`
/// holding `addr` and port 0. Used to flip the same ifreq between
/// SIOCSIFADDR (IP) and SIOCSIFNETMASK (mask).
fn stuff_sockaddr_in(req: &mut libc::ifreq, addr: Ipv4Addr) {
    // SAFETY: `sockaddr_in` is POD; all-zero is a valid value.
    let mut sin: libc::sockaddr_in = unsafe { mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = 0;
    sin.sin_addr.s_addr = u32::from_ne_bytes(addr.octets());
    // SAFETY: we're writing into the union slot of an ifreq. The
    // `ifr_ifru.ifru_addr` variant is `sockaddr`, which is the
    // C-standard prefix of `sockaddr_in` (same alignment, smaller
    // size). Copying `sockaddr_in` bytes into the union via a
    // matched-size raw write is the canonical pattern for the
    // SIOCSIF* ioctls (see netdevice(7)).
    unsafe {
        let dst: *mut libc::sockaddr_in =
            (&mut req.ifr_ifru as *mut _ as *mut u8).cast::<libc::sockaddr_in>();
        std::ptr::write_unaligned(dst, sin);
    }
}

/// Install `gateway` as the IPv4 default route via SIOCADDRT.
fn add_default_route(sock_fd: libc::c_int, gateway: Ipv4Addr) -> Result<()> {
    // SAFETY: `libc::rtentry` is POD; all-zero is a valid value.
    let mut rt: libc::rtentry = unsafe { mem::zeroed() };

    // Destination = 0.0.0.0 / 0.0.0.0 — i.e. the default route.
    stuff_rt_sockaddr(&mut rt.rt_dst, Ipv4Addr::UNSPECIFIED);
    stuff_rt_sockaddr(&mut rt.rt_genmask, Ipv4Addr::UNSPECIFIED);
    stuff_rt_sockaddr(&mut rt.rt_gateway, gateway);
    // RTF_UP | RTF_GATEWAY — both libc constants are already `u16`
    // (rt_flags' type), so no cast is needed.
    rt.rt_flags = libc::RTF_UP | libc::RTF_GATEWAY;
    rt.rt_metric = 0;

    // SAFETY: SIOCADDRT reads a `struct rtentry` from userspace.
    // `rt` is fully initialised above and outlives the ioctl call.
    let rc = unsafe { libc::ioctl(sock_fd, libc::SIOCADDRT as _, &rt as *const libc::rtentry) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        // EEXIST = the route is already there (e.g. from a stray
        // RA on a dual-stack network). Treat as success rather than
        // forcing the operator to halt over a benign collision.
        if err.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        return Err(NmblError::Rescue {
            stage: "net-apply-lease",
            source: Box::new(NmblError::Io {
                source: err,
                context: format!("SIOCADDRT default via {gateway}"),
            }),
        });
    }
    Ok(())
}

/// Write an IPv4 `sockaddr_in` into one of the `rtentry`'s sockaddr
/// slots. `rt_dst`, `rt_gateway`, and `rt_genmask` are all generic
/// `sockaddr` slots that the kernel re-interprets per the address
/// family — same recipe as `stuff_sockaddr_in`.
fn stuff_rt_sockaddr(dst: &mut libc::sockaddr, addr: Ipv4Addr) {
    // SAFETY: `sockaddr_in` is POD.
    let mut sin: libc::sockaddr_in = unsafe { mem::zeroed() };
    sin.sin_family = libc::AF_INET as libc::sa_family_t;
    sin.sin_port = 0;
    sin.sin_addr.s_addr = u32::from_ne_bytes(addr.octets());
    // SAFETY: `sockaddr_in` and `sockaddr` share a common header;
    // the kernel demands the AF_INET-shaped payload here. We use
    // `write_unaligned` so the cast does not assume alignment.
    unsafe {
        let dst_ptr: *mut libc::sockaddr_in =
            (dst as *mut libc::sockaddr).cast::<libc::sockaddr_in>();
        std::ptr::write_unaligned(dst_ptr, sin);
    }
}

// ---------------------------------------------------------------------------
// Download + memfd staging
// ---------------------------------------------------------------------------

/// Stream the HTTP body into a `memfd_create(2)` in-RAM fd while
/// SHA-256ing every chunk. Returns the populated memfd (rewound to
/// offset 0) plus the lowercase-hex digest.
fn download_to_memfd<R: RescueUi>(
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

// ---------------------------------------------------------------------------
// Loop-mount + switch-root + exec
// ---------------------------------------------------------------------------

/// Loop-mount the memfd at `/rescue`, then hand off to the shared
/// [`super::switch_root_and_exec`] helper. On success the process
/// image is replaced by the rescue shell so this never returns.
fn mount_and_switch_root(backing: &rustix::fd::OwnedFd) -> Result<Infallible> {
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

    let rescue_dir = Path::new(RESCUE_MOUNT);
    ensure_dir(rescue_dir).map_err(|source| NmblError::Rescue {
        stage: "mount-rescue",
        source: Box::new(source),
    })?;

    let loop_dev = PathBuf::from(format!("/dev/loop{index}"));
    mount_fs(Some(&loop_dev), rescue_dir, "squashfs", "ro").map_err(|source| {
        NmblError::Rescue {
            stage: "mount-rescue",
            source: Box::new(source),
        }
    })?;

    super::switch_root_and_exec(rescue_dir)
}

// ---------------------------------------------------------------------------
// Halt / reboot exits
// ---------------------------------------------------------------------------

/// Operator opted to reboot from the source picker. Falls through to
/// [`libc::_exit`] if the kernel refuses the syscall.
fn reboot_system() -> Result<Infallible> {
    let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_AUTOBOOT);
    // SAFETY: libc::_exit is async-signal-safe and unconditionally
    // terminates the process. Mirrors `super::halt_with_banner`.
    unsafe { libc::_exit(1) };
}

/// Operator opted to halt from the source picker.
fn halt_system() -> Result<Infallible> {
    let _ = nix::sys::reboot::reboot(nix::sys::reboot::RebootMode::RB_HALT_SYSTEM);
    // SAFETY: see `reboot_system`.
    unsafe { libc::_exit(1) };
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Lowercase hex encoder for SHA-256 digests. Avoids pulling in the
/// `hex` crate for a 64-byte string.
fn hex_lower(bytes: &[u8]) -> String {
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
fn compute_hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

/// Create `path` (and parents). Mirrors `rescue::disk::ensure_dir`.
fn ensure_dir(path: &Path) -> Result<()> {
    match std::fs::create_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(NmblError::Io {
            source: e,
            context: format!("creating {}", path.display()),
        }),
    }
}

/// Bridge `rustix::io::Errno` → `std::io::Error`. Same shape as the
/// helper in `sys::loopdev` / `rescue::disk`.
fn io_error_from_rustix(e: RustixErrno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

// ---------------------------------------------------------------------------
// Console UI (default until E.2 lands the ratatui version)
// ---------------------------------------------------------------------------

/// Minimal stdin/stdout [`RescueUi`] used until the ratatui screens
/// (E.2) replace it. Intentionally side-effect-y on the controlling
/// TTY so it works under any terminal the initramfs hands us.
pub struct ConsoleRescueUi;

impl ConsoleRescueUi {
    /// Helper that reads one line from stdin, trims the trailing
    /// newline, and surfaces I/O failures as a rescue error so the
    /// caller halts cleanly instead of looping on EOF.
    fn read_line(stage: &'static str) -> Result<String> {
        let mut buf = String::new();
        io::stdin()
            .read_line(&mut buf)
            .map_err(|source| NmblError::Rescue {
                stage,
                source: Box::new(NmblError::Io {
                    source,
                    context: "reading operator input".to_string(),
                }),
            })?;
        // Trim ONLY trailing CR/LF — the operator might legitimately
        // want trailing spaces in a URL.
        while buf.ends_with('\n') || buf.ends_with('\r') {
            buf.pop();
        }
        Ok(buf)
    }
}

impl RescueUi for ConsoleRescueUi {
    fn pick_source(&mut self, disk_reason: &str) -> Result<RescueSource> {
        let mut stderr = io::stderr();
        let _ = writeln!(stderr, "--- nmbl rescue: source picker ---");
        let _ = writeln!(stderr, "disk rescue failed:\n  {disk_reason}");
        let _ = writeln!(stderr, "Choose: [n]etwork / [r]eboot / [h]alt");
        let _ = stderr.flush();
        loop {
            let line = Self::read_line("net-ui-pick-source")?;
            match line.trim() {
                "n" | "N" | "network" => return Ok(RescueSource::Network),
                "r" | "R" | "reboot" => return Ok(RescueSource::Reboot),
                "h" | "H" | "halt" => return Ok(RescueSource::Halt),
                _ => {
                    let _ = writeln!(stderr, "unrecognised choice {line:?}; try n/r/h");
                    let _ = stderr.flush();
                }
            }
        }
    }

    fn prompt_url(&mut self, prefill: &str) -> Result<String> {
        let mut stderr = io::stderr();
        let _ = writeln!(stderr, "--- nmbl rescue: rescue URL ---");
        if !prefill.is_empty() {
            let _ = writeln!(stderr, "default: {prefill}");
            let _ = writeln!(stderr, "Press <enter> to accept, or type a new URL:");
        } else {
            let _ = writeln!(stderr, "Enter rescue URL (http://host/path):");
        }
        let _ = stderr.flush();
        let line = Self::read_line("net-ui-prompt-url")?;
        let trimmed = line.trim();
        if trimmed.is_empty() && !prefill.is_empty() {
            return Ok(prefill.to_string());
        }
        Ok(trimmed.to_string())
    }

    fn progress(&mut self, status: DownloadStatus) {
        // Stay terse — the console UI is a stopgap; rendering a real
        // progress bar belongs to the ratatui impl in E.2.
        let mut stderr = io::stderr();
        match status.total {
            Some(total) if total > 0 => {
                let pct = status.bytes.saturating_mul(100) / total;
                let _ = writeln!(
                    stderr,
                    "[nmbl] download: {} / {} bytes ({}%)",
                    status.bytes, total, pct
                );
            }
            _ => {
                let _ = writeln!(stderr, "[nmbl] download: {} bytes", status.bytes);
            }
        }
    }

    fn confirm_hash(
        &mut self,
        computed_hex: &str,
        prefill_expected: &str,
    ) -> Result<HashConfirmation> {
        let mut stderr = io::stderr();
        let _ = writeln!(stderr, "--- nmbl rescue: hash confirm ---");
        let _ = writeln!(stderr, "computed: {computed_hex}");
        if !prefill_expected.is_empty() {
            let _ = writeln!(stderr, "expected: {prefill_expected}");
            let match_str = if computed_hex.eq_ignore_ascii_case(prefill_expected) {
                "MATCH"
            } else {
                "MISMATCH"
            };
            let _ = writeln!(stderr, "verdict: {match_str}");
        } else {
            let _ = writeln!(stderr, "no expected hash pre-filled");
        }
        let _ = writeln!(stderr, "Confirm? [y]es / [n]o-mismatch / [a]bort");
        let _ = stderr.flush();
        loop {
            let line = Self::read_line("net-ui-confirm-hash")?;
            match line.trim() {
                "y" | "Y" | "yes" => return Ok(HashConfirmation::Confirmed),
                "n" | "N" | "no" => return Ok(HashConfirmation::Mismatch),
                "a" | "A" | "abort" => return Ok(HashConfirmation::Aborted),
                _ => {
                    let _ = writeln!(stderr, "unrecognised choice {line:?}; try y/n/a");
                    let _ = stderr.flush();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::RescueConfig;
    use crate::rescue::RescueMode;
    use std::collections::VecDeque;

    /// Canned-answer UI used by the unit tests. Pushes responses
    /// into the per-method queue; methods pop the front element and
    /// fall back to a default when the queue is empty (lets tests
    /// hit only the screens they care about).
    #[derive(Default)]
    struct FakeUi {
        source_choices: VecDeque<RescueSource>,
        urls: VecDeque<String>,
        confirms: VecDeque<HashConfirmation>,
        progress_calls: u32,
        last_disk_reason: Option<String>,
    }

    impl RescueUi for FakeUi {
        fn pick_source(&mut self, disk_reason: &str) -> Result<RescueSource> {
            self.last_disk_reason = Some(disk_reason.to_string());
            Ok(self
                .source_choices
                .pop_front()
                .unwrap_or(RescueSource::Halt))
        }
        fn prompt_url(&mut self, prefill: &str) -> Result<String> {
            Ok(self.urls.pop_front().unwrap_or_else(|| prefill.to_string()))
        }
        fn progress(&mut self, _status: DownloadStatus) {
            self.progress_calls = self.progress_calls.saturating_add(1);
        }
        fn confirm_hash(
            &mut self,
            _computed_hex: &str,
            _prefill_expected: &str,
        ) -> Result<HashConfirmation> {
            Ok(self
                .confirms
                .pop_front()
                .unwrap_or(HashConfirmation::Aborted))
        }
    }

    fn cfg_with_rescue(rescue: RescueConfig) -> Config {
        let mut c = Config::recovery_default();
        c.rescue = rescue;
        c
    }

    #[test]
    fn try_network_rescue_disabled_returns_net_disabled_error() {
        let cfg = cfg_with_rescue(RescueConfig {
            mode: RescueMode::External,
            network: false,
            ..RescueConfig::default()
        });
        let mut ui = FakeUi::default();
        let err = try_network_rescue(&cfg, &mut ui, "disk: synthetic")
            .expect_err("network=false must short-circuit");
        match err {
            NmblError::Rescue { stage, source } => {
                assert_eq!(stage, "net-disabled");
                match *source {
                    NmblError::ConfigInvalid { reason, .. } => {
                        assert!(
                            reason.contains("network rescue is disabled"),
                            "diagnostic should explain the cause, got: {reason}",
                        );
                    }
                    other => panic!("expected ConfigInvalid inside Rescue, got {other:?}"),
                }
            }
            other => panic!("expected Rescue variant, got {other:?}"),
        }
        // The UI must not have been touched — net-disabled is the
        // very first check.
        assert_eq!(ui.progress_calls, 0);
        assert!(ui.last_disk_reason.is_none());
    }

    /// Empty-input SHA-256 is RFC 6234's canonical vector — pinning
    /// it catches accidental algorithm swaps + the hex encoder.
    #[test]
    fn compute_hex_sha256_of_empty_matches_canonical() {
        assert_eq!(
            compute_hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
    }

    /// "abc" is another classic vector from FIPS 180-2; cheap second
    /// sanity check.
    #[test]
    fn compute_hex_sha256_of_abc_matches_canonical() {
        assert_eq!(
            compute_hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
    }

    #[test]
    fn hex_lower_pads_single_byte_with_zero() {
        assert_eq!(hex_lower(&[0x0a]), "0a");
        assert_eq!(hex_lower(&[0xff, 0x00, 0x10]), "ff0010");
    }

    /// Anything that needs a real DHCP server / loop device / pivot
    /// is documented here as a discoverable smoke-marker so a future
    /// VM-based integration suite can flip the gate.
    #[test]
    #[ignore = "needs CAP_NET_ADMIN/CAP_NET_RAW + a DHCP server + loop devices"]
    fn try_network_rescue_full_flow_smoke() {}
}
