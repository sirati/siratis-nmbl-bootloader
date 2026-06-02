#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
use super::*;

fn inner_no_generations() -> NmblError {
    NmblError::NoGenerations {
        searched: PathBuf::from("/sysroot/nix/var/nix/profiles"),
    }
}

#[test]
fn bootstrap_display_mentions_stage_and_inner() {
    let e = NmblError::Bootstrap {
        stage: "load-toml",
        source: Box::new(inner_no_generations()),
    };
    let s = e.to_string();
    assert!(s.contains("bootstrap stage load-toml failed"), "{s}");
    assert!(s.contains("no NixOS generations found"), "{s}");
}

#[test]
fn rescue_display_mentions_stage_and_inner() {
    let e = NmblError::Rescue {
        stage: "loop-alloc",
        source: Box::new(inner_no_generations()),
    };
    let s = e.to_string();
    assert!(s.contains("rescue stage loop-alloc failed"), "{s}");
    assert!(s.contains("no NixOS generations found"), "{s}");
}

#[test]
fn bootstrap_source_chain_reaches_inner() {
    let inner = inner_no_generations();
    let inner_msg = inner.to_string();
    let e = NmblError::Bootstrap {
        stage: "mount-boot",
        source: Box::new(inner),
    };
    let src = Error::source(&e).expect("Bootstrap should expose a source");
    assert_eq!(src.to_string(), inner_msg);
}

#[test]
fn rescue_source_chain_reaches_inner() {
    let inner = inner_no_generations();
    let inner_msg = inner.to_string();
    let e = NmblError::Rescue {
        stage: "http-fetch",
        source: Box::new(inner),
    };
    let src = Error::source(&e).expect("Rescue should expose a source");
    assert_eq!(src.to_string(), inner_msg);
}

#[test]
fn operator_chose_reboot_display_mentions_context() {
    // The dispatcher short-circuits on this variant, but the
    // operator-facing log line ("operator chose reboot at
    // wrong-password modal (<ctx>)") is still surfaced through
    // every transcript and grep — pin the exact shape.
    let e = NmblError::OperatorChoseReboot {
        context: "activation luks-password (root)".to_string(),
    };
    let s = e.to_string();
    assert!(s.contains("operator chose reboot"), "{s}");
    assert!(s.contains("wrong-password modal"), "{s}");
    assert!(s.contains("activation luks-password (root)"), "{s}");
}

#[test]
fn wrong_password_shell_exited_display_mentions_context() {
    let e = NmblError::WrongPasswordShellExited {
        context: "activation luks-password (root)".to_string(),
    };
    let s = e.to_string();
    assert!(s.contains("dropped to shell"), "{s}");
    assert!(s.contains("activation luks-password (root)"), "{s}");
}

#[test]
fn format_chain_renders_wrong_password_variants_single_line() {
    // Both variants are leaves (no inner source) — format_chain
    // must emit just the head with no trailing "caused by:" line.
    for variant in [
        NmblError::OperatorChoseReboot {
            context: "activation luks-password".to_string(),
        },
        NmblError::WrongPasswordShellExited {
            context: "activation luks-password".to_string(),
        },
    ] {
        let formatted = format_chain(&variant as &dyn Error);
        assert!(
            !formatted.contains("caused by"),
            "leaf variant must not emit a caused-by line: {formatted}"
        );
        assert!(
            formatted.contains("luks-password"),
            "context must reach the operator: {formatted}"
        );
    }
}

#[test]
fn operator_aborted_display_mentions_context() {
    // The user-facing emergency banner reads this string verbatim,
    // so the exact "operator aborted: <context>" shape is a
    // contract. Pin it.
    let e = NmblError::OperatorAborted {
        context: "waiting for /dev/sda1".to_string(),
    };
    let s = e.to_string();
    assert_eq!(s, "operator aborted: waiting for /dev/sda1");
}

#[test]
fn format_chain_renders_operator_aborted_single_line() {
    // OperatorAborted has no inner source — format_chain must emit
    // just the head error with no trailing "caused by:" line.
    let e = NmblError::OperatorAborted {
        context: "phase 3b: waiting for /dev/nvme0n1p2".to_string(),
    };
    let formatted = format_chain(&e as &dyn Error);
    assert!(
        formatted.contains("operator aborted"),
        "format_chain must lead with the variant prefix: {formatted}"
    );
    assert!(
        formatted.contains("/dev/nvme0n1p2"),
        "format_chain must surface the abort context: {formatted}"
    );
    assert!(
        !formatted.contains("caused by"),
        "OperatorAborted has no source — no caused-by line expected: {formatted}"
    );
}

#[test]
fn format_chain_walks_bootstrap_then_rescue() {
    // Nest a Rescue inside a Bootstrap to prove format_chain follows
    // both layers transparently through the standard `Error::source`.
    let leaf = inner_no_generations();
    let mid = NmblError::Rescue {
        stage: "hash-mismatch",
        source: Box::new(leaf),
    };
    let top = NmblError::Bootstrap {
        stage: "read-config",
        source: Box::new(mid),
    };
    let formatted = format_chain(&top as &dyn Error);
    assert!(
        formatted.contains("bootstrap stage read-config"),
        "{formatted}"
    );
    assert!(
        formatted.contains("caused by: rescue stage hash-mismatch"),
        "{formatted}"
    );
    assert!(
        formatted.contains("caused by: no NixOS generations found"),
        "{formatted}"
    );
}
