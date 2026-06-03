//! Seal-registry push for successful `luks-tpm` activations (FIX-03 /
//! LOW-2). Records every TPM-unsealed mapper so `policy::seal_secrets`
//! closes it before any interactive context — deriving the mapper name
//! from `produces_devices` when present, else from the authoritative
//! `cryptsetup open … <name>` argv.

use crate::config::{Activation, ActivationKind};

/// On a successful `luks-tpm` activation, push the unsealed mapper onto
/// the always-compiled seal registry (FIX-03). The mapper name is the
/// `/dev/mapper/<name>` node the activation produces; we strip the
/// `/dev/mapper/` prefix and carry the `cryptsetup` binary so the seal
/// path can run `cryptsetup close <name>` self-contained. A no-op for
/// every other activation kind.
///
/// `produces_devices` is `serde(default)` and is only coverage-validated
/// for mappers a `filesystem` entry references — a luks-tpm mapper
/// consumed indirectly (a PV under LVM, or any mapper no filesystem row
/// names) could carry an EMPTY `produces_devices` and silently bypass the
/// seal. To make the seal authoritative, we fall back to the mapper name
/// the `cryptsetup open … <name>` argv passed whenever `produces_devices`
/// yields nothing.
pub(super) fn register_tpm_mapper_if_luks_tpm(activation: &Activation) {
    if activation.kind != ActivationKind::LuksTpm {
        return;
    }
    let mut registered = false;
    for produced in &activation.produces_devices {
        let Some(name) = produced
            .to_str()
            .and_then(|p| p.strip_prefix("/dev/mapper/"))
        else {
            continue;
        };
        register_tpm_mapper_named(activation, name);
        registered = true;
    }
    // No mapper node was registered from `produces_devices` — derive the
    // close name from the authoritative `cryptsetup open … <name>` argv so
    // a luks-tpm mapper is NEVER left unregistered (FIX-03 / LOW-2).
    if !registered && let Some(name) = mapper_name_from_open_argv(&activation.argv) {
        register_tpm_mapper_named(activation, name);
    }
}

/// Push one `(cryptsetup, name)` mapper onto the seal registry.
fn register_tpm_mapper_named(activation: &Activation, name: &str) {
    crate::policy::register_tpm_mapper(crate::policy::MapperEntry {
        cryptsetup: activation.binary.clone(),
        name: name.to_string(),
    });
}

/// Extract the mapper `<name>` from a `cryptsetup open … <device> <name>`
/// argv. The two positionals after the `open` action are `<device>` then
/// `<name>`, so the name is the LAST non-flag positional. Returns `None`
/// for an argv that isn't an `open` (defensive — luks-tpm always emits
/// `open`).
fn mapper_name_from_open_argv(argv: &[String]) -> Option<&str> {
    let positionals: Vec<&str> = argv
        .iter()
        .filter(|a| !a.starts_with('-'))
        .map(String::as_str)
        .collect();
    // `["open", <device>, <name>]` — at least the action + device + name.
    if positionals.first().copied() != Some("open") || positionals.len() < 3 {
        return None;
    }
    positionals.last().copied()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::policy::registry;

    /// Redirect the on-disk mapper registry at a temp file so these tests
    /// never touch `/run` and stay hermetic. Returns the guard so the dir
    /// outlives the test body.
    fn redirect_persist() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        registry::set_persist_path(dir.path().join("tpm-unsealed-mappers"));
        registry::reset();
        dir
    }

    /// Build a minimal luks-tpm activation with the given argv +
    /// produces_devices for the registry-derivation tests.
    fn luks_tpm(argv: &[&str], produces: &[&str]) -> Activation {
        Activation {
            kind: ActivationKind::LuksTpm,
            required_modules: Vec::new(),
            binary: PathBuf::from("/bin/cryptsetup"),
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
            produces_devices: produces.iter().map(PathBuf::from).collect(),
            source_devices: Vec::new(),
            description: "test".to_string(),
            prompt_label: None,
            pass_to_stage1: None,
        }
    }

    /// `mapper_name_from_open_argv` picks the last positional after the
    /// `open` action — the mapper name — and ignores flags.
    #[test]
    fn derives_mapper_name_from_token_only_open_argv() {
        let argv: Vec<String> = ["open", "--token-only", "/dev/sda2", "cryptroot"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(mapper_name_from_open_argv(&argv), Some("cryptroot"));
    }

    /// A non-`open` argv yields no name (defensive).
    #[test]
    fn non_open_argv_yields_no_name() {
        let argv: Vec<String> = ["close", "cryptroot"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(mapper_name_from_open_argv(&argv), None);
    }

    /// LOW-2 regression: a luks-tpm activation with an EMPTY
    /// `produces_devices` (no filesystem entry names its mapper) is STILL
    /// registered for the seal — derived from the `open … <name>` argv —
    /// so it cannot silently bypass the close (FIX-03).
    #[test]
    fn luks_tpm_with_no_produces_devices_is_still_registered() {
        let _persist = redirect_persist();
        let act = luks_tpm(&["open", "--token-only", "/dev/sda2", "cryptpv"], &[]);
        register_tpm_mapper_if_luks_tpm(&act);
        let snap = registry::snapshot();
        assert_eq!(snap.len(), 1, "the mapper must be registered, got {snap:?}");
        assert_eq!(snap.first().map(|e| e.name.as_str()), Some("cryptpv"));
        registry::reset();
    }

    /// When `produces_devices` is present it is authoritative; the argv
    /// fallback does NOT double-register.
    #[test]
    fn produces_devices_path_does_not_double_register() {
        let _persist = redirect_persist();
        let act = luks_tpm(
            &["open", "--token-only", "/dev/sda2", "cryptroot"],
            &["/dev/mapper/cryptroot"],
        );
        register_tpm_mapper_if_luks_tpm(&act);
        assert_eq!(registry::snapshot().len(), 1, "exactly one registration");
        registry::reset();
    }
}
