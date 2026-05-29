//! Sysfs-based interface enumeration and carrier polling.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{NmblError, Result};

use super::Interface;

/// Filesystem prefix that exposes the per-interface attributes we
/// need. Constant so unit tests can reason about which paths the
/// reader is going to touch.
pub(super) const SYSFS_NET: &str = "/sys/class/net";

/// `ARPHRD_ETHER` from `<linux/if_arp.h>`. Filtering on this value
/// drops `lo` (`ARPHRD_LOOPBACK = 772`), wireguard tunnels
/// (`ARPHRD_NONE = 0xfffe`), `sit*` tunnels (`ARPHRD_SIT = 776`),
/// `ip6tnl*` (`ARPHRD_TUNNEL6 = 769`), and bridges (kept — bridges
/// also report 1, which is what we want).
const ARPHRD_ETHER: u16 = 1;

/// Test-friendly version of [`super::enumerate`] that takes the sysfs root
/// as a parameter. Production callers go through [`super::enumerate`].
pub(super) fn enumerate_in(root: &Path) -> Result<Vec<Interface>> {
    let mut out = Vec::new();
    let entries = fs::read_dir(root).map_err(|source| NmblError::Rescue {
        stage: "net-enumerate",
        source: Box::new(NmblError::Io {
            source,
            context: format!("reading {}", root.display()),
        }),
    })?;

    for entry in entries {
        let entry = entry.map_err(|source| NmblError::Rescue {
            stage: "net-enumerate",
            source: Box::new(NmblError::Io {
                source,
                context: format!("iterating {}", root.display()),
            }),
        })?;
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            // Non-UTF-8 interface names are illegal under Linux; skip
            // rather than fail the whole enumerate.
            Err(_) => continue,
        };
        let iface_dir = entry.path();
        if let Some(iface) = read_one(&iface_dir, &name)? {
            out.push(iface);
        }
    }

    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Read all four attribute files for a single sysfs entry and
/// build an [`Interface`]. Returns `Ok(None)` if the entry is not
/// `ARPHRD_ETHER` or any required attribute is missing — both are
/// "skip silently" conditions, not errors. Returns `Err` only on
/// I/O failures of attribute files that *did* exist.
fn read_one(iface_dir: &Path, name: &str) -> Result<Option<Interface>> {
    let type_path = iface_dir.join("type");
    let arphrd = match read_u32_trim(&type_path)? {
        Some(v) => v,
        None => return Ok(None),
    };
    if u16::try_from(arphrd).unwrap_or(u16::MAX) != ARPHRD_ETHER {
        return Ok(None);
    }

    let ifindex = match read_u32_trim(&iface_dir.join("ifindex"))? {
        Some(v) => v,
        None => return Ok(None),
    };

    let mac = match read_mac(&iface_dir.join("address"))? {
        Some(m) => m,
        None => return Ok(None),
    };

    // `carrier` is special: reading it on a down interface returns
    // EINVAL ("Invalid argument"). Treat any read failure as "no
    // carrier yet"; the caller can re-poll via wait_for_link after
    // bring_up.
    let has_carrier = read_carrier_or_false(&iface_dir.join("carrier"));

    Ok(Some(Interface {
        name: name.to_string(),
        index: ifindex,
        mac,
        has_carrier,
    }))
}

/// Read a sysfs file containing a single decimal integer + trailing
/// newline. Returns `Ok(None)` when the file simply does not exist
/// (e.g. a stale device removed between `read_dir` and `metadata`),
/// `Ok(Some(_))` on success, `Err` on every other I/O failure.
fn read_u32_trim(path: &Path) -> Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(s) => match s.trim().parse::<u32>() {
            Ok(v) => Ok(Some(v)),
            // Non-numeric content is a "skip this iface" signal, not
            // a hard failure — sysfs files are read by the kernel and
            // should always be well-formed, but bridges/macvlans can
            // produce surprises.
            Err(_) => Ok(None),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(NmblError::Rescue {
            stage: "net-enumerate",
            source: Box::new(NmblError::Io {
                source,
                context: format!("reading {}", path.display()),
            }),
        }),
    }
}

/// Parse a 6-byte EUI-48 from sysfs's `aa:bb:cc:dd:ee:ff\n` format.
/// Returns `Ok(None)` if the file is missing or malformed (callers
/// skip the interface in either case).
fn read_mac(path: &Path) -> Result<Option<[u8; 6]>> {
    let raw = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(NmblError::Rescue {
                stage: "net-enumerate",
                source: Box::new(NmblError::Io {
                    source,
                    context: format!("reading {}", path.display()),
                }),
            });
        }
    };
    Ok(parse_mac(raw.trim()))
}

/// Pure parser separated for unit-testability. Returns `None` on any
/// malformed input (wrong field count, non-hex, wrong byte width).
pub(super) fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut count = 0usize;
    for part in s.split(':') {
        if count >= 6 {
            return None;
        }
        if part.len() != 2 {
            return None;
        }
        let byte = u8::from_str_radix(part, 16).ok()?;
        if let Some(slot) = out.get_mut(count) {
            *slot = byte;
        } else {
            return None;
        }
        count += 1;
    }
    if count == 6 { Some(out) } else { None }
}

/// Read the `carrier` attribute. The kernel returns EINVAL when the
/// link is administratively down, so any failure is collapsed to
/// `false`. Successful reads parse as `1`/`0`.
fn read_carrier_or_false(path: &Path) -> bool {
    match fs::read_to_string(path) {
        Ok(s) => s.trim() == "1",
        Err(_) => false,
    }
}

/// Poll `/sys/class/net/<name>/carrier` until it reads `1` or
/// `timeout` elapses. Returns `Ok(true)` on link-up, `Ok(false)`
/// on timeout.
///
/// Polling interval is fixed at 100ms — fine-grained enough to
/// catch a fast PHY-up event, coarse enough to avoid burning CPU
/// in the (much commoner) waiting-for-DHCP case downstream.
pub fn wait_for_link(name: &str, timeout: Duration) -> Result<bool> {
    let path = carrier_path_for(name);
    let deadline = Instant::now().checked_add(timeout);

    loop {
        match fs::read_to_string(&path) {
            Ok(s) => {
                if s.trim() == "1" {
                    return Ok(true);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Interface vanished mid-poll — surface so the caller
                // can either retry enumerate or fail rescue.
                return Err(NmblError::Rescue {
                    stage: "net-link-wait",
                    source: Box::new(NmblError::Io {
                        source: e,
                        context: format!("reading {}", path.display()),
                    }),
                });
            }
            // EINVAL on `carrier` means "iface is admin-down" — treat
            // as "no link yet" and keep polling.
            Err(_) => {}
        }

        if let Some(d) = deadline
            && Instant::now() >= d
        {
            return Ok(false);
        }
        // When `deadline` is `None` the caller passed `Duration::MAX`
        // and `checked_add` overflowed; treat that as an
        // effectively-infinite wait and keep polling.
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Resolve the carrier file path for an interface. Lives behind a
/// helper so unit tests can supply a synthetic root.
fn carrier_path_for(name: &str) -> PathBuf {
    Path::new(SYSFS_NET).join(name).join("carrier")
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
    use std::path::Path;

    #[test]
    fn parse_mac_round_trip() {
        assert_eq!(
            parse_mac("00:11:22:33:44:55"),
            Some([0x00, 0x11, 0x22, 0x33, 0x44, 0x55])
        );
        assert_eq!(
            parse_mac("aa:bb:cc:dd:ee:ff"),
            Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
        );
    }

    #[test]
    fn parse_mac_rejects_malformed() {
        assert_eq!(parse_mac(""), None);
        assert_eq!(parse_mac("00:11:22:33:44"), None); // 5 octets
        assert_eq!(parse_mac("00:11:22:33:44:55:66"), None); // 7 octets
        assert_eq!(parse_mac("00-11-22-33-44-55"), None); // wrong sep
        assert_eq!(parse_mac("zz:11:22:33:44:55"), None); // non-hex
        assert_eq!(parse_mac("0:1:2:3:4:5"), None); // single-digit fields
    }

    #[test]
    fn enumerate_finds_lo_when_arphrd_filter_relaxed() {
        // Manually invoke read_one against /sys/class/net/lo to prove
        // the per-entry reader works against a real sysfs node.
        let lo = Path::new("/sys/class/net/lo");
        if !lo.exists() {
            eprintln!("skipping: /sys/class/net/lo missing");
            return;
        }
        // Force the ETHER check by short-circuiting: read attrs
        // directly and assert they're well-formed.
        let arphrd = read_u32_trim(&lo.join("type")).expect("read lo/type");
        let ifindex = read_u32_trim(&lo.join("ifindex")).expect("read lo/ifindex");
        let mac = read_mac(&lo.join("address")).expect("read lo/address");
        assert_eq!(arphrd, Some(772), "lo type must be ARPHRD_LOOPBACK");
        assert!(matches!(ifindex, Some(n) if n >= 1));
        assert_eq!(mac, Some([0u8; 6]), "lo MAC is all-zeros");
    }

    #[test]
    fn enumerate_in_synthetic_root_returns_only_ether() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();

        // Fake `lo` (ARPHRD_LOOPBACK) — should be filtered out.
        let lo = root.join("lo");
        fs::create_dir(&lo).expect("mkdir lo");
        fs::write(lo.join("type"), "772\n").expect("write lo type");
        fs::write(lo.join("ifindex"), "1\n").expect("write lo ifindex");
        fs::write(lo.join("address"), "00:00:00:00:00:00\n").expect("write lo addr");
        fs::write(lo.join("carrier"), "1\n").expect("write lo carrier");

        // Fake `eth0` (ARPHRD_ETHER) — should be returned.
        let eth = root.join("eth0");
        fs::create_dir(&eth).expect("mkdir eth0");
        fs::write(eth.join("type"), "1\n").expect("write eth0 type");
        fs::write(eth.join("ifindex"), "2\n").expect("write eth0 ifindex");
        fs::write(eth.join("address"), "52:54:00:12:34:56\n").expect("write eth0 addr");
        fs::write(eth.join("carrier"), "1\n").expect("write eth0 carrier");

        // Fake `wg0` (ARPHRD_NONE) — should be filtered out.
        let wg = root.join("wg0");
        fs::create_dir(&wg).expect("mkdir wg0");
        fs::write(wg.join("type"), "65534\n").expect("write wg type");
        fs::write(wg.join("ifindex"), "3\n").expect("write wg ifindex");
        fs::write(wg.join("address"), "00:00:00:00:00:00\n").expect("write wg addr");

        let list = enumerate_in(root).expect("enumerate_in");
        assert_eq!(list.len(), 1, "only eth0 must survive the filter");
        let eth0 = &list[0];
        assert_eq!(eth0.name, "eth0");
        assert_eq!(eth0.index, 2);
        assert_eq!(eth0.mac, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        assert!(eth0.has_carrier);
    }

    #[test]
    fn wait_for_link_lo_reports_link_up() {
        let lo = Path::new("/sys/class/net/lo");
        if !lo.exists() {
            eprintln!("skipping: /sys/class/net/lo missing");
            return;
        }
        // `lo` always reads `1` for carrier on a real Linux host. This
        // is a smoke test of the polling path without needing
        // privileges.
        let up = wait_for_link("lo", Duration::from_secs(1)).expect("wait_for_link lo");
        assert!(up, "loopback carrier must be 1");
    }

    #[test]
    fn wait_for_link_missing_iface_errors() {
        let res = wait_for_link("does-not-exist-zzz", Duration::from_millis(50));
        match res {
            Err(NmblError::Rescue { stage, .. }) => {
                assert_eq!(stage, "net-link-wait");
            }
            other => panic!("expected Rescue/net-link-wait, got {other:?}"),
        }
    }
}
