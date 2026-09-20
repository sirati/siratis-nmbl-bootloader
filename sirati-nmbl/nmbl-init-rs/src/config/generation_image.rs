use std::path::PathBuf;

use serde::Deserialize;

fn default_mountpoint() -> PathBuf {
    PathBuf::from("/nix")
}

/// A signed, loop-backed `/nix` image selected through an atomic directory
/// symlink. The image path still comes from the matching `filesystems` entry;
/// this table adds the detached signature that must verify before mounting it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationImageConfig {
    #[serde(default)]
    pub enable: bool,

    #[serde(default = "default_mountpoint")]
    pub mountpoint: PathBuf,

    pub signature_path: PathBuf,

    pub state_root: PathBuf,

    #[serde(default)]
    pub automatic_rollback: bool,

    #[serde(default)]
    pub automatic_rescue: bool,
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests may fail immediately while parsing fixed fixtures"
)]
mod tests {
    use super::*;

    #[test]
    fn parses_enabled_policy_with_default_mountpoint() {
        let policy: GenerationImageConfig =
            toml::from_str("enable = true\nsignature_path = '/.nix-image/active/nix.erofs.sig'\nstate_root = '/.nix-image'\n")
                .expect("valid generation-image config");
        assert!(policy.enable);
        assert_eq!(policy.mountpoint, PathBuf::from("/nix"));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let result = toml::from_str::<GenerationImageConfig>(
            "enable = true\nsignature_path = '/x.sig'\nstate_root = '/x'\nunknown = true\n",
        );
        assert!(result.is_err());
    }
}
