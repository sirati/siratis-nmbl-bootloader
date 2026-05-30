//! `--validate-hardware`: read-only checks of the NMBL TOML against the
//! ACTUAL hardware on the target machine. Zero side effects.
//!
//! Run by `lib/install-bootloader.nix` just before the bootloader files
//! are written. It tests ONLY what the TOML declares:
//!
//! * for every luks `[[activation]]`, each `source_devices` entry must
//!   exist and carry a LUKS header. The header is verified with the
//!   supplied `cryptsetup isLuks <dev>` (a read-only query; we NEVER
//!   `open`). If no cryptsetup path was supplied we fall back to reading
//!   the 6-byte LUKS magic ourselves.
//! * for every `[[filesystems]]` whose `device` is a real path NOT under
//!   `/dev/mapper/` (mapper nodes are activation OUTPUTS, untestable
//!   before activation), that path must exist.
//!
//! lvm/mdraid/zfs carry no backing-device info in the TOML, so nothing
//! is asserted for them, and no `vgchange`/`mdadm --assemble` is ever
//! run. We collect ALL failures and return them together so one install
//! surfaces every problem at once.

use std::path::Path;

use rustix::fs::{Mode, OFlags};

use crate::config::{ActivationKind, Config};
use crate::sys::activation::run_capture;

use super::tools::ToolPaths;

/// The on-disk LUKS1/LUKS2 magic: ASCII "LUKS" then 0xBA 0xBE at offset 0.
const LUKS_MAGIC: [u8; 6] = [b'L', b'U', b'K', b'S', 0xba, 0xbe];

/// Run the hardware validation. Returns the list of human-readable
/// failure messages (empty == all good). Never mutates anything.
pub fn validate_hardware(config: &Config, tools: &ToolPaths) -> Vec<String> {
    let mut failures = Vec::new();
    let cryptsetup = tools.cryptsetup();

    check_luks_source_devices(config, cryptsetup.as_deref(), &mut failures);
    check_filesystem_devices(config, &mut failures);

    failures
}

fn is_luks_kind(kind: ActivationKind) -> bool {
    matches!(
        kind,
        ActivationKind::LuksTpm | ActivationKind::LuksKeyfile | ActivationKind::LuksPassword
    )
}

/// Every `source_devices` entry of every luks activation must exist and
/// carry a LUKS header.
fn check_luks_source_devices(
    config: &Config,
    cryptsetup: Option<&Path>,
    failures: &mut Vec<String>,
) {
    for act in &config.activations {
        if !is_luks_kind(act.kind) {
            continue;
        }
        for dev in &act.source_devices {
            if !dev.exists() {
                failures.push(format!(
                    "luks activation {:?} (kind {:?}): backing device {} does not exist",
                    act.description,
                    act.kind,
                    dev.display()
                ));
                continue;
            }
            match has_luks_header(dev, cryptsetup) {
                Ok(true) => {}
                Ok(false) => failures.push(format!(
                    "luks activation {:?} (kind {:?}): device {} exists but carries no LUKS header",
                    act.description,
                    act.kind,
                    dev.display()
                )),
                Err(why) => failures.push(format!(
                    "luks activation {:?} (kind {:?}): could not probe LUKS header on {}: {why}",
                    act.description,
                    act.kind,
                    dev.display()
                )),
            }
        }
    }
}

/// Every filesystem `device` that is a real path NOT under
/// `/dev/mapper/` must exist (mapper nodes are activation outputs and
/// are not testable before activation runs).
fn check_filesystem_devices(config: &Config, failures: &mut Vec<String>) {
    for fs in &config.filesystems {
        let dev = fs.device.as_str();
        if !dev.starts_with('/') || dev.starts_with("/dev/mapper/") {
            continue;
        }
        if !Path::new(dev).exists() {
            failures.push(format!(
                "filesystem for {}: device {dev} does not exist",
                fs.mountpoint.display()
            ));
        }
    }
}

/// Probe whether `dev` carries a LUKS header. Prefers the supplied
/// cryptsetup (`isLuks` is a read-only query, exit 0 == LUKS); falls
/// back to reading the 6-byte magic at offset 0 ourselves.
fn has_luks_header(dev: &Path, cryptsetup: Option<&Path>) -> Result<bool, String> {
    if let Some(cs) = cryptsetup {
        return cryptsetup_is_luks(cs, dev);
    }
    read_luks_magic(dev)
}

/// `cryptsetup isLuks <dev>` — exit 0 == LUKS, any other clean exit ==
/// not LUKS. Only a failure to RUN cryptsetup (fork/exec error) is an
/// `Err`; a non-zero exit is a legitimate "not LUKS" answer.
fn cryptsetup_is_luks(cryptsetup: &Path, dev: &Path) -> Result<bool, String> {
    let argv = vec!["isLuks".to_string(), dev.display().to_string()];
    let (outcome, _captured) =
        run_capture(cryptsetup, &argv).map_err(|e| format!("running cryptsetup isLuks: {e}"))?;
    Ok(outcome.normal_exit && outcome.exit_code == 0)
}

/// Fallback magic probe: open read-only and read the first 6 bytes.
fn read_luks_magic(dev: &Path) -> Result<bool, String> {
    let fd = rustix::fs::open(dev, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| format!("open {} O_RDONLY: {e}", dev.display()))?;
    let mut buf = [0u8; LUKS_MAGIC.len()];
    let mut filled = 0usize;
    // Block devices can return short reads; loop until the buffer is
    // full, EOF (a device smaller than 6 bytes is trivially not LUKS),
    // or an error.
    while filled < buf.len() {
        let Some(tail) = buf.get_mut(filled..) else {
            // Unreachable: `filled < buf.len()` guarantees the slice is
            // valid, but avoid panicking indexing per the crate lint.
            break;
        };
        match rustix::io::read(&fd, tail) {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(format!("read {}: {e}", dev.display())),
        }
    }
    Ok(buf == LUKS_MAGIC)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "nmbl-hwtest-{}-{}-{name}",
            std::process::id(),
            // monotonic-ish suffix so parallel test fns don't collide
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut f = std::fs::File::create(&path).expect("create temp");
        f.write_all(bytes).expect("write temp");
        path
    }

    #[test]
    fn magic_present_detected() {
        let mut data = LUKS_MAGIC.to_vec();
        data.extend_from_slice(b"trailing payload");
        let p = write_temp("withmagic", &data);
        assert_eq!(read_luks_magic(&p), Ok(true));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn magic_absent_detected() {
        let p = write_temp("nomagic", b"not a luks header at all");
        assert_eq!(read_luks_magic(&p), Ok(false));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn short_file_is_not_luks() {
        let p = write_temp("short", b"LUK");
        assert_eq!(read_luks_magic(&p), Ok(false));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn fs_device_missing_is_failure() {
        let toml = r#"
            [[filesystems]]
            device = "/dev/definitely-not-here-xyz"
            mountpoint = "/"
            fstype = "ext4"
            is_root = true
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let failures = validate_hardware(&cfg, &ToolPaths::default());
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("does not exist"), "{failures:?}");
    }

    #[test]
    fn present_non_mapper_fs_device_ok() {
        // Use a path that genuinely exists on the test host.
        let p = write_temp("fsdev", b"");
        let toml = format!(
            r#"
            [[filesystems]]
            device = "{}"
            mountpoint = "/"
            fstype = "ext4"
            is_root = true
        "#,
            p.display()
        );
        let cfg: Config = toml::from_str(&toml).expect("parse");
        let failures = validate_hardware(&cfg, &ToolPaths::default());
        assert!(failures.is_empty(), "{failures:?}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn mapper_fs_device_is_skipped() {
        // A /dev/mapper node is an activation output; even if absent it
        // must NOT be flagged by the hardware check.
        let toml = r#"
            [[filesystems]]
            device = "/dev/mapper/cryptroot"
            mountpoint = "/"
            fstype = "ext4"
            is_root = true
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let failures = validate_hardware(&cfg, &ToolPaths::default());
        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn luks_source_device_missing_is_failure() {
        let toml = r#"
            [[activations]]
            kind = "luks-password"
            binary = "/bin/cryptsetup"
            source_devices = ["/dev/definitely-not-here-xyz"]
            description = "unlock root"
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let failures = validate_hardware(&cfg, &ToolPaths::default());
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("does not exist"), "{failures:?}");
    }

    #[test]
    fn luks_source_device_without_magic_is_failure() {
        let p = write_temp("luksdev", b"this is not encrypted");
        let toml = format!(
            r#"
            [[activations]]
            kind = "luks-password"
            binary = "/bin/cryptsetup"
            source_devices = ["{}"]
            description = "unlock root"
        "#,
            p.display()
        );
        let cfg: Config = toml::from_str(&toml).expect("parse");
        // No cryptsetup path -> magic fallback, which reads "not LUKS".
        let failures = validate_hardware(&cfg, &ToolPaths::default());
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("no LUKS header"), "{failures:?}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn luks_source_device_with_magic_ok() {
        let p = write_temp("luksok", &LUKS_MAGIC);
        let toml = format!(
            r#"
            [[activations]]
            kind = "luks-password"
            binary = "/bin/cryptsetup"
            source_devices = ["{}"]
            description = "unlock root"
        "#,
            p.display()
        );
        let cfg: Config = toml::from_str(&toml).expect("parse");
        let failures = validate_hardware(&cfg, &ToolPaths::default());
        assert!(failures.is_empty(), "{failures:?}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn non_luks_activation_source_devices_ignored() {
        // An lvm activation never has source_devices, but even if one
        // were present we must not probe it as LUKS.
        let toml = r#"
            [[activations]]
            kind = "lvm"
            binary = "/bin/vgchange"
            description = "activate vg"
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let failures = validate_hardware(&cfg, &ToolPaths::default());
        assert!(failures.is_empty(), "{failures:?}");
    }
}
