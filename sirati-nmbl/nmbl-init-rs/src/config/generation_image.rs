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
    pub stage1_store: Option<GenerationStoreConfig>,

    #[serde(default)]
    pub automatic_rollback: bool,

    /// Record boot attempts in the generation state (`attempted`/`pending`)
    /// so a failed previous boot is detected. Emitted by Nix whenever rollback
    /// is enabled or `boot.nmbl.rescue.automatic` needs failures detected.
    #[serde(default)]
    pub track_state: bool,

    /// Ignored. Automatic rescue is decided only by `[rescue].automatic`;
    /// the key is still accepted so configs written by older NMBL versions
    /// (e.g. inside already-installed generation directories) keep parsing.
    #[serde(default, rename = "automatic_rescue")]
    pub legacy_automatic_rescue: Option<bool>,
}

impl GenerationImageConfig {
    /// Whether boot attempts are tracked. Also true for configs written by
    /// older NMBL versions, which enabled tracking through
    /// `automatic_rollback` or `automatic_rescue` instead of `track_state`.
    #[must_use]
    pub fn tracks_state(&self) -> bool {
        self.track_state || self.automatic_rollback || self.legacy_automatic_rescue == Some(true)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationStoreConfig {
    pub device: PathBuf,
    pub fstype: String,
    pub options: String,
    pub mountpoint: PathBuf,
    pub target_mountpoint: PathBuf,
    pub relative_state_root: PathBuf,
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

    #[test]
    fn parses_root_backing_store_with_confined_state_path() {
        let policy: GenerationImageConfig = toml::from_str(
            "enable = true\nsignature_path = '/nmbl-generations/active/nix.erofs.sig'\nstate_root = '/nmbl-generations'\n[stage1_store]\ndevice = '/dev/root'\nfstype = 'ext4'\noptions = 'rw,noexec'\nmountpoint = '/mnt/nmbl-store'\ntarget_mountpoint = '/'\nrelative_state_root = 'nmbl-generations'\n",
        )
        .expect("root-backed generation store config");
        let store = policy.stage1_store.expect("stage1 store");
        assert_eq!(store.target_mountpoint, PathBuf::from("/"));
        assert_eq!(store.relative_state_root, PathBuf::from("nmbl-generations"));
    }
}
