//! Exit-code classification and error wrapping for activation runs.

use crate::config::{Activation, ActivationKind};
use crate::error::NmblError;

pub(crate) fn wrap_runner_error(a: &Activation, source: NmblError) -> NmblError {
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
pub(crate) fn is_activation_success(exit_code: i32) -> bool {
    exit_code == 0 || exit_code == 5
}

/// Inner `Io` carries the exit code + signalled-vs-normal flag in one line.
pub(crate) fn exit_code_error(
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
pub(crate) fn kind_label(kind: ActivationKind) -> &'static str {
    match kind {
        ActivationKind::Lvm => "lvm",
        ActivationKind::Mdraid => "mdraid",
        ActivationKind::LuksTpm => "luks-tpm",
        ActivationKind::LuksKeyfile => "luks-keyfile",
        ActivationKind::LuksPassword => "luks-password",
        ActivationKind::Zfs => "zfs",
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
    use crate::sys::activation::ProcessOutcome;

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
