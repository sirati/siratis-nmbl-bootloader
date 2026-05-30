//! Small utility helpers for the activation orchestrator.

use std::collections::HashSet;

use zeroize::Zeroizing;

use crate::config::{Activation, ActivationKind};
use crate::error::{NmblError, Result};
use crate::nmbl_warn;
use crate::ui::console::Console;

use super::PasswordSupplier;

pub(super) const PROC_MODULES: &str = "/proc/modules";

/// `None` for every kind except `LuksPassword`, where we prompt and
/// return the raw passphrase bytes. We do NOT append a newline: the
/// cryptsetup argv uses `--key-file=-`, which reads stdin verbatim as
/// binary key data (no stripping). Appending `\n` would turn a 4-byte
/// passphrase "test" into the 5-byte key "test\n", which doesn't match
/// the stored LUKS header digest.
pub(super) async fn collect_stdin(
    activation: &Activation,
    console: &mut dyn Console,
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
    let secret = supplier.prompt(console, label).await?;
    let mut buf = Zeroizing::new(Vec::with_capacity(secret.len()));
    buf.extend_from_slice(secret.as_bytes());
    Ok(Some(buf))
}

pub(super) fn check_required_modules(activation: &Activation, loaded: &HashSet<String>) {
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

pub(super) fn wrap_runner_error(a: &Activation, source: NmblError) -> NmblError {
    NmblError::Activation {
        kind: kind_label(a.kind).to_string(),
        source: Box::new(source),
    }
}

/// Exit codes that mean the activation step is satisfied. `0` is the
/// obvious success; `5` from cryptsetup luksOpen means "device already
/// active" — the device-mapper mapping survived from a prior attempt
/// this session, so the LUKS volume is already open and accessible
/// rather than re-opened. Treating it as fatal would block kexec for
/// no reason.
pub(super) fn is_activation_success(exit_code: i32) -> bool {
    exit_code == 0 || exit_code == 5
}

/// Inner `Io` carries the exit code + signalled-vs-normal flag in one line.
pub(super) fn exit_code_error(
    a: &Activation,
    outcome: crate::sys::activation::ProcessOutcome,
) -> NmblError {
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
pub(super) fn kind_label(kind: ActivationKind) -> &'static str {
    match kind {
        ActivationKind::Lvm => "lvm",
        ActivationKind::Mdraid => "mdraid",
        ActivationKind::LuksTpm => "luks-tpm",
        ActivationKind::LuksKeyfile => "luks-keyfile",
        ActivationKind::LuksPassword => "luks-password",
        ActivationKind::Zfs => "zfs",
    }
}

pub(super) fn loaded_modules() -> Result<HashSet<String>> {
    let text = std::fs::read_to_string(PROC_MODULES).map_err(|source| NmblError::Io {
        source,
        context: format!("reading {PROC_MODULES} to check activation prerequisites"),
    })?;
    Ok(parse_loaded_modules(&text))
}

/// First whitespace token of each non-blank line; factored for unit tests.
pub(super) fn parse_loaded_modules(text: &str) -> HashSet<String> {
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
    use crate::sys::activation::ProcessOutcome;

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

    #[test]
    fn cryptsetup_exit_code_5_is_success() {
        // "Device already active" — cryptsetup luksOpen sees an
        // existing device-mapper mapping (e.g. left over from an
        // earlier unlock attempt this same boot) and refuses to
        // re-open. The volume is open and accessible, so NMBL must
        // treat this as a clean success rather than blocking kexec.
        let outcome = ProcessOutcome {
            exit_code: 5,
            normal_exit: true,
        };
        assert!(
            is_activation_success(outcome.exit_code),
            "exit code 5 must classify as success so the activation loop breaks",
        );

        // Exit code 0 stays a success; the wrong-password code 2 and
        // an arbitrary fatal code must remain non-success so the
        // wrong-password modal / fatal error paths still fire.
        assert!(is_activation_success(0));
        assert!(!is_activation_success(2));
        assert!(!is_activation_success(1));
        assert!(!is_activation_success(127));
    }
}
