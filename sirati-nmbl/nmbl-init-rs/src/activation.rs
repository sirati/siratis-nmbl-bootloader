//! Storage activation orchestrator (Phase C.6) — drives LVM / mdraid /
//! cryptsetup / zpool between module load and system-fs mount. For each
//! `config.activations` entry: warn about missing `required_modules`
//! (built-ins aren't in `/proc/modules`, so a hard check would false-
//! positive), prompt the supplied `PasswordSupplier` for `luks-password`,
//! exec via `sys::activation::run` (non-zero exit is fatal), then block
//! until every `produces_devices` path is stat-able (15 s budget, then
//! `NmblError::DeviceTimeout`). All policy lives here; `sys::activation`
//! is pure exec mechanism.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use zeroize::Zeroizing;

use crate::config::{Activation, ActivationKind, Config};
use crate::devices::wait_for;
use crate::error::{NmblError, Result};
use crate::sys::activation::{ProcessOutcome, run};
use crate::{nmbl_info, nmbl_warn};

/// One passphrase to inject into the kexec'd initrd as a keyfile. The
/// activation runner emits one of these per `luks-password` activation
/// whose TOML carries a `pass_to_stage1 = "<path>"` field. The kexec
/// path appends a cpio fragment containing `<path>` with `secret` as
/// its contents, so stage-1's NixOS init can use it as a keyFile.
pub struct KeyInjection {
    pub path: PathBuf,
    pub secret: Zeroizing<Vec<u8>>,
}

const PROC_MODULES: &str = "/proc/modules";
/// Per-device wait budget; matches the Phase 3 loop.
const DEVICE_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Pluggable passphrase prompt; TUI implements it, tests mock it.
/// `Zeroizing` wipes the buffer on drop, including on error paths.
pub trait PasswordSupplier {
    fn prompt(&mut self, label: &str) -> Result<Zeroizing<String>>;
}

/// Run every entry in declaration order. First failure is fatal —
/// activations chain (LUKS → LVM → fs), so a partial run leaves
/// Phase 3 unable to find its devices.
///
/// Returns the set of [`KeyInjection`]s the kexec path must append to
/// the system initrd (one per `luks-password` activation whose TOML
/// sets `pass_to_stage1`). The vec is empty when no activation opts in.
pub fn run_all_activations(
    config: &Config,
    mut password_supplier: Option<&mut dyn PasswordSupplier>,
) -> Result<Vec<KeyInjection>> {
    let mut injections: Vec<KeyInjection> = Vec::new();
    if config.activations.is_empty() {
        return Ok(injections);
    }

    let loaded = loaded_modules()?;

    for activation in &config.activations {
        check_required_modules(activation, &loaded);

        // Per-iteration reborrow; without it the compiler keeps the
        // mutable borrow live across loop turns and rejects iter #2.
        let supplier_ref: Option<&mut dyn PasswordSupplier> = match password_supplier {
            Some(ref mut s) => Some(&mut **s),
            None => None,
        };
        let stdin_owned = collect_stdin(activation, supplier_ref)?;
        let stdin_slice = stdin_owned.as_ref().map(|z| z.as_slice());

        let outcome = run(&activation.binary, &activation.argv, stdin_slice)
            .map_err(|source| wrap_runner_error(activation, source))?;

        if outcome.exit_code != 0 {
            return Err(exit_code_error(activation, outcome));
        }

        let device_count = activation.produces_devices.len();
        for device in &activation.produces_devices {
            wait_for(device, DEVICE_WAIT_TIMEOUT)?;
        }

        // After a successful luks-password unlock, if pass_to_stage1
        // is set, hand the passphrase bytes off for kexec injection.
        // The stdin buffer is the same Zeroizing-wrapped bytes
        // cryptsetup just consumed; moving it into the injection
        // keeps it under Zeroizing all the way through.
        if activation.kind == ActivationKind::LuksPassword
            && let Some(path) = activation.pass_to_stage1.as_ref()
            && let Some(secret) = stdin_owned
        {
            injections.push(KeyInjection {
                path: path.clone(),
                secret,
            });
        }

        nmbl_info!(
            "activation {} completed: {} device(s) ready",
            kind_label(activation.kind),
            device_count
        );
    }

    Ok(injections)
}

/// `None` for every kind except `LuksPassword`, where we prompt and
/// return the raw passphrase bytes. We do NOT append a newline: the
/// cryptsetup argv uses `--key-file=-`, which reads stdin verbatim as
/// binary key data (no stripping). Appending `\n` would turn a 4-byte
/// passphrase "test" into the 5-byte key "test\n", which doesn't match
/// the stored LUKS header digest.
fn collect_stdin(
    activation: &Activation,
    supplier: Option<&mut dyn PasswordSupplier>,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    if activation.kind != ActivationKind::LuksPassword {
        return Ok(None);
    }

    let Some(supplier) = supplier else {
        return Err(NmblError::Activation {
            kind: kind_label(activation.kind).to_string(),
            source: Box::new(NmblError::ConfigInvalid {
                reason: "luks-password activation requires a TUI to prompt for the \
                         passphrase, but no PasswordSupplier was provided"
                    .to_string(),
                context: format!(
                    "activation {} ({})",
                    kind_label(activation.kind),
                    activation.description
                ),
            }),
        });
    };

    let label = activation
        .prompt_label
        .as_deref()
        .unwrap_or("Enter passphrase");
    let secret = supplier.prompt(label)?;
    let mut buf = Zeroizing::new(Vec::with_capacity(secret.len()));
    buf.extend_from_slice(secret.as_bytes());
    Ok(Some(buf))
}

fn check_required_modules(activation: &Activation, loaded: &HashSet<String>) {
    for module in &activation.required_modules {
        // /proc/modules always uses the underscore spelling; config
        // entries may use either (e.g. "dm-crypt" vs "dm_crypt").
        let canonical = module.replace('-', "_");
        if !loaded.contains(&canonical) {
            nmbl_warn!(
                "activation {} requires module {} but it's not loaded; attempting anyway",
                kind_label(activation.kind),
                module
            );
        }
    }
}

fn wrap_runner_error(a: &Activation, source: NmblError) -> NmblError {
    NmblError::Activation {
        kind: kind_label(a.kind).to_string(),
        source: Box::new(source),
    }
}

/// Inner `Io` carries the exit code + signalled-vs-normal flag in one line.
fn exit_code_error(a: &Activation, outcome: ProcessOutcome) -> NmblError {
    let how = if outcome.normal_exit {
        "exited"
    } else {
        "killed by signal"
    };
    let ctx = format!(
        "activation {} ({}) {} with code {} (binary {})",
        kind_label(a.kind),
        a.description,
        how,
        outcome.exit_code,
        a.binary.display(),
    );
    NmblError::Activation {
        kind: kind_label(a.kind).to_string(),
        source: Box::new(NmblError::Io {
            source: std::io::Error::other(ctx.clone()),
            context: ctx,
        }),
    }
}

/// Kebab-case label per kind; avoids `{:?}` so a rename can't silently
/// change log strings the operator greps for.
fn kind_label(kind: ActivationKind) -> &'static str {
    match kind {
        ActivationKind::Lvm => "lvm",
        ActivationKind::Mdraid => "mdraid",
        ActivationKind::LuksTpm => "luks-tpm",
        ActivationKind::LuksKeyfile => "luks-keyfile",
        ActivationKind::LuksPassword => "luks-password",
        ActivationKind::Zfs => "zfs",
    }
}

fn loaded_modules() -> Result<HashSet<String>> {
    let text = std::fs::read_to_string(PROC_MODULES).map_err(|source| NmblError::Io {
        source,
        context: format!("reading {PROC_MODULES} to check activation prerequisites"),
    })?;
    Ok(parse_loaded_modules(&text))
}

/// First whitespace token of each non-blank line; factored for unit tests.
fn parse_loaded_modules(text: &str) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(name) = trimmed.split_whitespace().next()
            && !name.is_empty()
        {
            out.insert(name.to_string());
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure; production lints are too strict for asserts"
)]
mod tests {
    use super::*;

    /// Mirrors `/proc/modules` (+ a leading-whitespace and a blank line).
    const SAMPLE_PROC_MODULES: &str = "\
ext4 901120 1 - Live 0x0000000000000000
\tnvme 49152 0 - Live 0x0000000000000000
crc32c_generic 16384 1 ext4, Live 0x0000000000000000

";

    #[test]
    fn parse_loaded_modules_extracts_names_and_edge_cases() {
        let set = parse_loaded_modules(SAMPLE_PROC_MODULES);
        assert_eq!(set.len(), 3, "exactly three modules in the sample");
        assert!(set.contains("ext4"));
        assert!(set.contains("nvme"), "leading whitespace must be ignored");
        assert!(set.contains("crc32c_generic"));

        assert!(parse_loaded_modules("").is_empty(), "empty input");
        assert!(
            parse_loaded_modules("\n   \n\t\n").is_empty(),
            "blank / whitespace-only lines must not become entries"
        );

        // Truncated last line (no trailing newline) — the kernel always
        // emits one, but the parser should tolerate its absence.
        let truncated = parse_loaded_modules("ext4 901120 1 - Live 0x0\nvfat");
        assert!(truncated.contains("ext4"));
        assert!(truncated.contains("vfat"));
    }

    /// Fixed-passphrase supplier. Exercises the trait shape; a real
    /// integration test would require a live cryptsetup + LUKS image.
    struct MockSupplier {
        canned: &'static str,
        seen_label: Option<String>,
    }

    impl PasswordSupplier for MockSupplier {
        fn prompt(&mut self, label: &str) -> Result<Zeroizing<String>> {
            self.seen_label = Some(label.to_string());
            Ok(Zeroizing::new(self.canned.to_string()))
        }
    }

    #[test]
    fn mock_supplier_and_kind_labels() {
        let mut sup = MockSupplier {
            canned: "hunter2",
            seen_label: None,
        };
        let got = sup.prompt("Unlock root").expect("mock never errors");
        assert_eq!(&**got, "hunter2");
        assert_eq!(sup.seen_label.as_deref(), Some("Unlock root"));

        // Lock operator-facing log strings against silent enum renames.
        assert_eq!(kind_label(ActivationKind::Lvm), "lvm");
        assert_eq!(kind_label(ActivationKind::Mdraid), "mdraid");
        assert_eq!(kind_label(ActivationKind::LuksTpm), "luks-tpm");
        assert_eq!(kind_label(ActivationKind::LuksKeyfile), "luks-keyfile");
        assert_eq!(kind_label(ActivationKind::LuksPassword), "luks-password");
        assert_eq!(kind_label(ActivationKind::Zfs), "zfs");
    }
}
