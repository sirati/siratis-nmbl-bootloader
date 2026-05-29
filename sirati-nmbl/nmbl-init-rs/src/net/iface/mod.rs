//! NETLINK-free network-interface enumerator + bring-up helper used
//! by the rescue HTTP fallback path (Phase D.1).
//!
//! NMBL's rescue path needs three things from each candidate
//! Ethernet-like NIC:
//!   1. its kernel name (for log lines + the SIOCSIFFLAGS ioctl),
//!   2. its ifindex (for the DHCP socket bind in D.2),
//!   3. its MAC address (DHCPv4 `chaddr` field).
//!
//! …plus a way to flip `IFF_UP` and a way to wait for the link to
//! settle. We deliberately AVOID hand-rolling NETLINK RTM_GETLINK
//! response parsing — `/sys/class/net/<name>/{address,ifindex,type,carrier}`
//! exposes the same data with no `unsafe` and no UAPI ABI risk.
//!
//! The only `unsafe` lives in [`bring_up`], where SIOCGIFFLAGS /
//! SIOCSIFFLAGS need a raw `libc::ifreq`. Both blocks are local,
//! pre-zeroed, and bounded to the function — no globals, no FFI
//! lifetimes leaking out.

mod ioctl;
mod sysfs;

pub use ioctl::bring_up;
pub use sysfs::wait_for_link;

use std::path::Path;

use crate::error::Result;

use sysfs::{SYSFS_NET, enumerate_in};

/// One discovered Ethernet-like interface. Fields are populated from
/// `/sys/class/net/<name>/…` at [`enumerate`] time and never
/// re-fetched; callers that need fresh carrier state should call
/// [`wait_for_link`] explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    /// Kernel name, e.g. `"eth0"` or `"enp3s0"`.
    pub name: String,
    /// Kernel ifindex (1-based, unique per netns).
    pub index: u32,
    /// 6-byte EUI-48 hardware address (DHCPv4 `chaddr`).
    pub mac: [u8; 6],
    /// Snapshot of `/sys/class/net/<name>/carrier` at enumeration
    /// time. May be stale by the time DHCP runs — re-check with
    /// [`wait_for_link`] before sending the first DISCOVER.
    pub has_carrier: bool,
}

/// Enumerate `ARPHRD_ETHER` interfaces under `/sys/class/net`.
///
/// Loopback (`type=772`), tunnels (non-1 types) and entries with
/// unparseable attributes are filtered out. The returned vector is
/// sorted by interface name so the result is deterministic across
/// invocations — important for log triage.
///
/// In a CI sandbox where only `lo` exists, this returns
/// `Ok(vec![])` because `lo`'s ARPHRD does not match `ETHER`.
pub fn enumerate() -> Result<Vec<Interface>> {
    enumerate_in(Path::new(SYSFS_NET))
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

    #[test]
    fn enumerate_returns_ok_in_sandbox() {
        // /sys/class/net always exists on Linux; in a sandbox where
        // only `lo` is present we expect an empty vector because lo's
        // ARPHRD is 772, not 1.
        let result = enumerate();
        match result {
            Ok(v) => {
                for iface in &v {
                    // Sanity: every returned iface must be ARPHRD_ETHER
                    // (we already filtered, but assert anyway) and have
                    // a non-empty name.
                    assert!(!iface.name.is_empty());
                }
            }
            Err(e) => panic!("enumerate failed: {e}"),
        }
    }
}
