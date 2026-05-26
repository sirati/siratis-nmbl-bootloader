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
use std::path::Path;
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::config::{Activation, ActivationKind, Config};
use crate::error::{NmblError, Result};
use crate::sys::activation::{ProcessOutcome, run};
use crate::{nmbl_info, nmbl_warn};

const PROC_MODULES: &str = "/proc/modules";
/// Per-device wait budget; matches the Phase 3 loop.
const DEVICE_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
/// Poll interval inside `wait_for_local`.
const DEVICE_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Pluggable passphrase prompt; TUI implements it, tests mock it.
/// `Zeroizing` wipes the buffer on drop, including on error paths.
pub trait PasswordSupplier {
    fn prompt(&mut self, label: &str) -> Result<Zeroizing<String>>;
}

/// Run every entry in declaration order. First failure is fatal —
/// activations chain (LUKS → LVM → fs), so a partial run leaves
/// Phase 3 unable to find its devices.
pub fn run_all_activations(
    config: &Config,
    mut password_supplier: Option<&mut dyn PasswordSupplier>,
) -> Result<()> {
    if config.activations.is_empty() {
        return Ok(());
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
            wait_for_local(device, DEVICE_WAIT_TIMEOUT)?;
        }

        nmbl_info!(
            "activation {} completed: {} device(s) ready",
            kind_label(activation.kind),
            device_count
        );
    }

    Ok(())
}

/// `None` for every kind except `LuksPassword`, where we prompt and
/// append a newline so cryptsetup sees interactive-prompt termination.
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
    let mut buf = Zeroizing::new(Vec::with_capacity(secret.len().saturating_add(1)));
    buf.extend_from_slice(secret.as_bytes());
    buf.push(b'\n');
    Ok(Some(buf))
}

fn check_required_modules(activation: &Activation, loaded: &HashSet<String>) {
    for module in &activation.required_modules {
        if !loaded.contains(module) {
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

// TEMP: inline until sibling C.3 lands `crate::devices`. At merge time
// delete this and switch call sites to `crate::devices::wait_for`.
fn wait_for_local(device: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match std::fs::metadata(device) {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(NmblError::Io {
                    source,
                    context: format!("stat({}) while waiting for device", device.display()),
                });
            }
        }
        if Instant::now() >= deadline {
            let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
            return Err(NmblError::DeviceTimeout {
                device: device.to_path_buf(),
                timeout_ms,
            });
        }
        std::thread::sleep(DEVICE_POLL_INTERVAL);
    }
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

    #[test]
    fn wait_for_local_happy_and_timeout() {
        wait_for_local(Path::new("/"), Duration::from_millis(1))
            .expect("root directory always exists");
        let err = wait_for_local(
            Path::new("/nonexistent/nmbl-activation-test-device"),
            Duration::from_millis(50),
        )
        .expect_err("missing device must error");
        match err {
            NmblError::DeviceTimeout { .. } => {}
            other => panic!("expected DeviceTimeout, got {other:?}"),
        }
    }
}
