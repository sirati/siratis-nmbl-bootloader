//! `nmblctl verify-rescue`: check the installed external full-system rescue
//! from the booted host.
//!
//! The image checks are NMBL's own ([`nmbl_init::rescue::inspect`]): the same
//! path resolution, SHA-512 pins, sidecar names, signature domains and baked
//! keys the boot path uses. This module adds the `[rescue.system]` facts an
//! operator needs to reach the rescue, and the stable JSON contract.

use std::path::Path;

use nmbl_init::config::Config;
use nmbl_init::rescue::inspect::{RescueInspection, inspect_external_rescue};

/// JSON contract version of [`RescueReport::to_json`].
pub const JSON_VERSION: u32 = 1;

/// Everything `verify-rescue` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RescueReport {
    pub signing_enabled: bool,
    pub signing_enforced: bool,
    pub inspection: RescueInspection,
    /// `[rescue.system].sshd_port`, verbatim.
    pub sshd_port: Option<u16>,
    /// `[rescue.system].authorized_keys`, verbatim.
    pub authorized_keys: Vec<String>,
}

/// Inspect the rescue `config` names, resolved against `boot_root`.
#[must_use]
pub fn verify(config: &Config, boot_root: &Path) -> RescueReport {
    let system = config.rescue.system.as_ref();
    RescueReport {
        signing_enabled: config.signing.enable,
        signing_enforced: config.signing.enable && config.signing.enforce,
        inspection: inspect_external_rescue(config, boot_root),
        sshd_port: system.map(|s| s.sshd_port),
        authorized_keys: system
            .map(|s| s.authorized_keys.clone())
            .unwrap_or_default(),
    }
}

impl RescueReport {
    /// No problem was found; the exit status follows this.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.inspection.ok()
    }

    /// The stable machine-readable report.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let images: Vec<serde_json::Value> = self
            .inspection
            .images
            .iter()
            .map(|image| {
                serde_json::json!({
                    "role": image.role.as_str(),
                    "path": image.path.to_string_lossy(),
                    "present": image.present,
                    "sha512": image.sha512.as_str(),
                    "signature": image.signature.as_str(),
                })
            })
            .collect();
        serde_json::json!({
            "version": JSON_VERSION,
            "ok": self.ok(),
            "signing_enabled": self.signing_enabled,
            "signing_enforced": self.signing_enforced,
            "images": images,
            "sshd_port": self.sshd_port,
            "authorized_keys": self.authorized_keys,
            "problems": self.inspection.problems,
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert"
)]
mod tests {
    use super::*;

    const RESCUE: &[u8] = b"rescue image bytes";
    const TOOLS: &[u8] = b"tools image bytes";

    fn sha512_hex(path: &Path) -> String {
        use std::os::fd::AsFd;
        let file = std::fs::File::open(path).expect("open image");
        let (digest, _len) = nmbl_init::util::hash::sha512_fd(file.as_fd()).expect("hash");
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Stage the images under `boot` and write a config as config-toml.nix
    /// renders it for a full-system rescue, then load it the way nmblctl does.
    fn setup(boot: &Path, signing: bool, tools_pin: Option<&str>) -> Config {
        std::fs::create_dir_all(boot.join("nmbl")).expect("mkdir");
        std::fs::write(boot.join("nmbl-rescue.sfs"), RESCUE).expect("rescue");
        std::fs::write(boot.join("nmbl/rescue-tools.erofs"), TOOLS).expect("tools");
        let rescue_pin = sha512_hex(&boot.join("nmbl-rescue.sfs"));
        let tools_pin = tools_pin
            .map(str::to_string)
            .unwrap_or_else(|| sha512_hex(&boot.join("nmbl/rescue-tools.erofs")));
        let toml = format!(
            r#"
[general]
[paths]
[signing]
enable = {signing}
enforce = {signing}

[rescue]
mode = "external"

[rescue.image]
format = "erofs"
sha512 = "{rescue_pin}"

[rescue.tools]
path = "/nmbl/rescue-tools.erofs"
sha512 = "{tools_pin}"

[rescue.system]
sshd_port = 2222
authorized_keys = ["ssh-ed25519 AAAAC3 operator@host"]
"#
        );
        let path = boot.join("config.toml");
        std::fs::write(&path, toml).expect("config");
        Config::load(&path).expect("load config")
    }

    fn images(json: &serde_json::Value) -> Vec<(String, bool, String, String)> {
        json["images"]
            .as_array()
            .expect("images")
            .iter()
            .map(|i| {
                (
                    i["role"].as_str().expect("role").to_string(),
                    i["present"].as_bool().expect("present"),
                    i["sha512"].as_str().expect("sha512").to_string(),
                    i["signature"].as_str().expect("signature").to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn good_unsigned_rescue_reports_the_full_contract() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = setup(dir.path(), false, None);
        let report = verify(&config, dir.path());
        let json = report.to_json();
        assert!(report.ok(), "{json}");
        assert_eq!(json["version"], 1);
        assert_eq!(json["ok"], true);
        assert_eq!(json["signing_enabled"], false);
        assert_eq!(json["signing_enforced"], false);
        assert_eq!(json["sshd_port"], 2222);
        assert_eq!(
            json["authorized_keys"],
            serde_json::json!(["ssh-ed25519 AAAAC3 operator@host"])
        );
        assert_eq!(json["problems"], serde_json::json!([]));
        assert_eq!(
            images(&json),
            vec![
                (
                    "rescue-image".into(),
                    true,
                    "matches".into(),
                    "not-required".into()
                ),
                (
                    "tools".into(),
                    true,
                    "matches".into(),
                    "not-required".into()
                ),
            ]
        );
        assert_eq!(
            json["images"][0]["path"],
            dir.path()
                .join("nmbl-rescue.sfs")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn missing_image_and_wrong_hash_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = setup(dir.path(), false, None);
        std::fs::write(dir.path().join("nmbl/rescue-tools.erofs"), b"stale").expect("swap");
        std::fs::remove_file(dir.path().join("nmbl-rescue.sfs")).expect("rm");
        let report = verify(&config, dir.path());
        let json = report.to_json();
        assert!(!report.ok());
        assert_eq!(json["ok"], false);
        assert_eq!(
            images(&json),
            vec![
                (
                    "rescue-image".into(),
                    false,
                    "unreadable".into(),
                    "not-required".into()
                ),
                (
                    "tools".into(),
                    true,
                    "mismatch".into(),
                    "not-required".into()
                ),
            ]
        );
        assert_eq!(json["problems"].as_array().expect("problems").len(), 2);
    }

    #[test]
    fn unpinned_tools_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = setup(dir.path(), false, Some(""));
        let report = verify(&config, dir.path());
        assert!(!report.ok());
        assert_eq!(images(&report.to_json())[1].2, "unpinned");
    }

    #[test]
    fn signing_enabled_without_signatures_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = setup(dir.path(), true, None);
        let report = verify(&config, dir.path());
        let json = report.to_json();
        assert!(!report.ok());
        assert_eq!(json["signing_enabled"], true);
        assert_eq!(json["signing_enforced"], true);
        assert!(images(&json).iter().all(|i| i.3 == "missing"), "{json}");

        // A sidecar this build's (empty, test) key set cannot verify.
        std::fs::write(dir.path().join("nmbl-rescue.sfs.sig"), b"garbage").expect("sig");
        let json = verify(&config, dir.path()).to_json();
        assert_eq!(images(&json)[0].3, "invalid");
    }

    #[test]
    fn config_without_full_system_rescue_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[general]\n[paths]\n[rescue]\nmode = \"embedded\"\n").expect("cfg");
        let config = Config::load(&path).expect("load");
        let report = verify(&config, dir.path());
        let json = report.to_json();
        assert!(!report.ok());
        assert_eq!(json["images"], serde_json::json!([]));
        assert_eq!(json["sshd_port"], serde_json::Value::Null);
        assert!(
            json["problems"][0]
                .as_str()
                .expect("problem")
                .contains("full-system")
        );
    }
}
