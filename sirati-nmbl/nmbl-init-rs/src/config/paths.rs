use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    #[serde(default = "default_nix_profiles_dir")]
    pub nix_profiles_dir: PathBuf,

    #[serde(default = "default_system_root")]
    pub system_root: PathBuf,

    #[serde(default = "default_shell")]
    pub shell: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            nix_profiles_dir: default_nix_profiles_dir(),
            system_root: default_system_root(),
            shell: default_shell(),
        }
    }
}

fn default_nix_profiles_dir() -> PathBuf {
    PathBuf::from("/mnt/system/nix/var/nix/profiles")
}

fn default_system_root() -> PathBuf {
    PathBuf::from("/mnt/system")
}

fn default_shell() -> PathBuf {
    PathBuf::from("/bin/sh")
}
