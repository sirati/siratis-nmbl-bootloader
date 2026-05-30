use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesystemEntry {
    pub device: String,
    pub mountpoint: PathBuf,
    pub fstype: String,

    #[serde(default)]
    pub options: String,

    #[serde(default)]
    pub is_root: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub kind: ActivationKind,

    #[serde(default)]
    pub required_modules: Vec<String>,

    pub binary: PathBuf,

    #[serde(default)]
    pub argv: Vec<String>,

    #[serde(default)]
    pub produces_devices: Vec<PathBuf>,

    /// Backing block devices this activation CONSUMES (not produces).
    /// For luks kinds this is the encrypted backing device(s) whose
    /// header `--validate-hardware` probes; for lvm/mdraid/zfs it is
    /// empty because the config carries no backing-device info for
    /// them. `serde(default)` keeps older configs (which never emitted
    /// this key) parseable.
    #[serde(default)]
    pub source_devices: Vec<PathBuf>,

    pub description: String,

    #[serde(default)]
    pub prompt_label: Option<String>,

    /// When set on a `luks-password` activation, NMBL captures the
    /// typed passphrase and injects it into the kexec'd initrd as a
    /// keyfile at this in-cpio path (e.g. `/etc/nmbl-luks/cryptroot`).
    /// The next stage's NixOS config points
    /// `boot.initrd.luks.devices.<name>.keyFile` at the same path so
    /// the operator only types once. The path stays in memory only —
    /// it lives in the initrd tmpfs, dropped at switch-root.
    #[serde(default)]
    pub pass_to_stage1: Option<PathBuf>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActivationKind {
    Lvm,
    Mdraid,
    LuksTpm,
    LuksKeyfile,
    LuksPassword,
    Zfs,
}
