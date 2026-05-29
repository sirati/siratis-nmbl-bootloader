//! SIOCGIFFLAGS / SIOCSIFFLAGS bring-up helper.

use std::os::fd::AsRawFd;

use nix::sys::socket::{AddressFamily, SockFlag, SockType, socket};

use crate::error::{NmblError, Result};

/// `IFF_UP` from `<linux/if.h>` — must match `libc::IFF_UP` (which
/// has type `c_int`). Stored as `i16` to match the `ifr_flags` field
/// width in `struct ifreq`.
const IFF_UP_BIT: i16 = libc::IFF_UP as i16;

/// Bring `iface` up by setting `IFF_UP`. No-op if already up.
///
/// Uses the classic SIOCGIFFLAGS / SIOCSIFFLAGS pair on a fresh
/// AF_INET/SOCK_DGRAM socket — per the netdevice(7) contract, the
/// socket's address family is irrelevant. Both ioctls operate on a
/// `struct ifreq` so we drive them with `nix::ioctl_readwrite_bad!`.
pub fn bring_up(name: &str) -> Result<()> {
    if name.len() >= libc::IFNAMSIZ {
        return Err(NmblError::Rescue {
            stage: "net-bring-up",
            source: Box::new(NmblError::ConfigInvalid {
                reason: format!(
                    "interface name {name:?} exceeds IFNAMSIZ ({})",
                    libc::IFNAMSIZ
                ),
                context: "preparing SIOCGIFFLAGS ifreq".to_string(),
            }),
        });
    }

    // Disposable AF_INET datagram socket — never `bind`d or used to
    // send data; only carries the ioctl. CLOEXEC matches the
    // initramfs convention.
    let sock = socket(
        AddressFamily::Inet,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        None,
    )
    .map_err(|source| NmblError::Rescue {
        stage: "net-bring-up",
        source: Box::new(NmblError::Io {
            source: std::io::Error::from_raw_os_error(source as i32),
            context: format!("socket(AF_INET, SOCK_DGRAM) for {name}"),
        }),
    })?;

    // GET current flags.
    let mut req = blank_ifreq(name);
    // `libc::ioctl`'s request-code parameter is `c_int` on musl and
    // `c_ulong` on glibc — both reachable via `as _` from the
    // `c_ulong` constant. The cast is lossless on every Linux ABI
    // because Linux ioctl opcodes are 32-bit.
    let siocgifflags = libc::SIOCGIFFLAGS as _;
    // SAFETY: SIOCGIFFLAGS is a kernel-stable opcode. `req` is a
    // pre-zeroed `libc::ifreq` whose `ifr_name` field has been
    // populated and the rest of the union is zero. The ioctl reads
    // `ifr_name` and writes `ifr_ifru.ifru_flags` (an i16).
    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), siocgifflags, &mut req) };
    if rc < 0 {
        return Err(NmblError::Rescue {
            stage: "net-bring-up",
            source: Box::new(NmblError::Io {
                source: std::io::Error::last_os_error(),
                context: format!("SIOCGIFFLAGS on {name}"),
            }),
        });
    }
    // SAFETY: SIOCGIFFLAGS sets the `ifru_flags` variant of the
    // anonymous union; we read it back through the same field.
    let cur = unsafe { req.ifr_ifru.ifru_flags };
    if cur & IFF_UP_BIT != 0 {
        return Ok(());
    }

    // OR in IFF_UP and push back.
    req.ifr_ifru.ifru_flags = cur | IFF_UP_BIT;
    let siocsifflags = libc::SIOCSIFFLAGS as _;
    // SAFETY: SIOCSIFFLAGS reads `ifr_name` + `ifru_flags`; both are
    // initialized above.
    let rc = unsafe { libc::ioctl(sock.as_raw_fd(), siocsifflags, &req) };
    if rc < 0 {
        return Err(NmblError::Rescue {
            stage: "net-bring-up",
            source: Box::new(NmblError::Io {
                source: std::io::Error::last_os_error(),
                context: format!("SIOCSIFFLAGS on {name}"),
            }),
        });
    }
    Ok(())
}

/// Construct a zeroed `libc::ifreq` and copy `name` into the
/// `ifr_name` slot, including a trailing NUL. Caller guarantees
/// `name.len() < IFNAMSIZ`.
fn blank_ifreq(name: &str) -> libc::ifreq {
    // SAFETY: `libc::ifreq` is a POD made of integers, a fixed-size
    // char array, and a union of POD variants. The all-zero bit
    // pattern is a valid value (`ifru_flags = 0`, etc.).
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (slot, byte) in req.ifr_name.iter_mut().zip(name.as_bytes()) {
        *slot = *byte as libc::c_char;
    }
    req
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests are allowed to assert with panics"
)]
mod tests {
    use super::*;

    fn privileged() -> bool {
        // CAP_NET_ADMIN proxy: euid 0. Skip bring_up tests otherwise
        // — same skip-pattern as sys/loopdev.rs.
        nix::unistd::Uid::effective().is_root()
    }

    #[test]
    fn bring_up_requires_cap_net_admin() {
        if !privileged() {
            eprintln!("skipping: bring_up needs CAP_NET_ADMIN");
            return;
        }
        // Even with caps the test is mostly a smoke check: `lo` is
        // almost always already up, so bring_up should return Ok
        // without changing anything.
        bring_up("lo").expect("bring_up lo");
    }
}
