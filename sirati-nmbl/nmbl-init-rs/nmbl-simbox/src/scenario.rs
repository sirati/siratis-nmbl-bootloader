//! The scenario description: disks, filesystems, files, generation state,
//! keys. A scenario is a TOML file; every path in it is relative to the
//! scenario file's directory.
//!
//! ```toml
//! name = "normal boot"
//! initramfs = "initrd-root"          # tree unpacked as the container's /
//! kernel_release = "6.12.0-sim"
//! cmdline = "console=ttyS0"
//! [[block]]                          # a block device NMBL may see
//! name = "vda2"                      # /dev/vda2, /sys/class/block/vda2
//! major = 254
//! minor = 2
//! blkid = { TYPE = "ext4", PARTLABEL = "disk-main-root", UUID = "…" }
//! tree = "root-fs"                   # what mount(/dev/vda2, …) shows
//! [[luks]]                           # cryptsetup open simulation
//! device = "/dev/vda3"
//! name = "cryptroot"
//! passphrase = "test-passphrase"     # scenario TEST key only
//! mapper_tree = "root-fs"            # contents of /dev/mapper/cryptroot
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Human-readable scenario name (documentation only).
    #[allow(dead_code, reason = "documents the scenario file")]
    pub name: String,
    /// Directory tree used as the container root (the NMBL initramfs).
    pub initramfs: PathBuf,
    /// `uname -r` NMBL sees (module tree lookups use it).
    #[serde(default = "default_release")]
    pub kernel_release: String,
    /// `/proc/cmdline` NMBL sees.
    #[serde(default)]
    pub cmdline: String,
    #[serde(default)]
    pub block: Vec<BlockDevice>,
    #[serde(default)]
    pub luks: Vec<LuksVolume>,
    /// Keys typed on the console after start (escape sequences allowed, e.g.
    /// "\r"), each after `delay_ms`. Used by automated runs.
    #[serde(default)]
    pub keys: Vec<KeyInput>,
    /// Emulated framebuffer size.
    #[serde(default = "default_fb")]
    pub framebuffer: (u32, u32),
    #[serde(skip)]
    pub base: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockDevice {
    pub name: String,
    pub major: u32,
    pub minor: u32,
    /// `blkid -p -o export` attributes.
    #[serde(default)]
    pub blkid: BTreeMap<String, String>,
    /// Directory whose contents a mount of this device shows.
    #[serde(default)]
    pub tree: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LuksVolume {
    pub device: PathBuf,
    pub name: String,
    pub passphrase: String,
    pub mapper_tree: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyInput {
    /// Wait until this text has appeared on the console (after the previous
    /// key input) before counting `delay_ms`. Typing before NMBL owns the
    /// terminal would be flushed when it enters raw mode, as on a real tty.
    #[serde(default)]
    pub after: Option<String>,
    #[serde(default)]
    pub delay_ms: u64,
    pub text: String,
}

fn default_release() -> String {
    "6.12.0-nmbl-simbox".to_string()
}

fn default_fb() -> (u32, u32) {
    (640, 360)
}

impl Scenario {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut s: Scenario =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        s.base = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
            .canonicalize()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(s)
    }

    /// Resolve a scenario-relative path.
    pub fn path(&self, p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.base.join(p)
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, reason = "tests assert")]
mod tests {
    use super::*;

    #[test]
    fn parses_a_luks_scenario() {
        let dir = tempfile::tempdir().expect("tmp");
        let p = dir.path().join("s.toml");
        std::fs::write(
            &p,
            r#"
name = "luks"
initramfs = "initrd"
cmdline = "console=ttyS0"
[[block]]
name = "vda3"
major = 254
minor = 3
blkid = { TYPE = "crypto_LUKS", PARTLABEL = "disk-main-luks" }
[[luks]]
device = "/dev/vda3"
name = "cryptroot"
passphrase = "hunter2"
mapper_tree = "root"
[[keys]]
delay_ms = 500
text = "hunter2\r"
"#,
        )
        .expect("write");
        let s = Scenario::load(&p).expect("load");
        assert_eq!(s.block.first().map(|b| b.minor), Some(3));
        assert_eq!(s.luks.first().map(|l| l.name.as_str()), Some("cryptroot"));
        assert_eq!(s.framebuffer, (640, 360));
        assert!(
            s.path(Path::new("initrd"))
                .starts_with(dir.path().canonicalize().expect("c"))
        );
    }
}
