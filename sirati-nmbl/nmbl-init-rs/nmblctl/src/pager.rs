//! Pager selection with a git-style fallback.
//!
//! `chain` and `status` pipe their coloured output into a pager, like git:
//! `$PAGER` if set, else `less -R`. When `less` is unavailable, stdout is not a
//! TTY, or `--no-pager` is passed, the output goes straight to stdout. This
//! module holds the pure decision so the fallback is testable without spawning
//! anything; `main.rs` acts on the returned [`PagerChoice`].

/// The resolved output route for paged commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PagerChoice {
    /// Pipe through this pager command line (argv, already split).
    Pager(Vec<String>),
    /// Write directly to stdout (no pager).
    Direct,
}

/// Inputs to the pager decision, so the policy is a pure function.
#[derive(Debug, Clone, Copy)]
pub struct PagerEnv<'a> {
    /// `--no-pager` was passed.
    pub no_pager: bool,
    /// stdout is a TTY.
    pub stdout_is_tty: bool,
    /// `$PAGER`, if set and non-empty.
    pub pager_var: Option<&'a str>,
    /// Whether a `less` binary is available on `PATH`.
    pub less_available: bool,
}

/// Decide how to route paged output.
///
/// Order (matching git's spirit):
/// 1. `--no-pager` or a non-TTY stdout → [`PagerChoice::Direct`]; a pager is
///    pointless when the output is redirected or explicitly declined.
/// 2. `$PAGER` set → use it verbatim (split on whitespace).
/// 3. else `less -R` when `less` is available.
/// 4. else [`PagerChoice::Direct`].
#[must_use]
pub fn decide_pager(env: PagerEnv<'_>) -> PagerChoice {
    if env.no_pager || !env.stdout_is_tty {
        return PagerChoice::Direct;
    }
    if let Some(pager) = env.pager_var
        && !pager.trim().is_empty()
    {
        return PagerChoice::Pager(pager.split_whitespace().map(str::to_string).collect());
    }
    if env.less_available {
        // -R passes through the ANSI colour escapes rather than showing them
        // literally; git uses the same flag.
        return PagerChoice::Pager(vec!["less".to_string(), "-R".to_string()]);
    }
    PagerChoice::Direct
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert")]
mod tests {
    use super::*;

    fn env() -> PagerEnv<'static> {
        PagerEnv {
            no_pager: false,
            stdout_is_tty: true,
            pager_var: None,
            less_available: true,
        }
    }

    #[test]
    fn no_pager_flag_forces_direct() {
        let e = PagerEnv {
            no_pager: true,
            ..env()
        };
        assert_eq!(decide_pager(e), PagerChoice::Direct);
    }

    #[test]
    fn non_tty_forces_direct() {
        let e = PagerEnv {
            stdout_is_tty: false,
            ..env()
        };
        assert_eq!(decide_pager(e), PagerChoice::Direct);
    }

    #[test]
    fn pager_var_wins_over_less() {
        let e = PagerEnv {
            pager_var: Some("most -w"),
            ..env()
        };
        assert_eq!(
            decide_pager(e),
            PagerChoice::Pager(vec!["most".to_string(), "-w".to_string()])
        );
    }

    #[test]
    fn empty_pager_var_falls_through_to_less() {
        let e = PagerEnv {
            pager_var: Some("   "),
            ..env()
        };
        assert_eq!(
            decide_pager(e),
            PagerChoice::Pager(vec!["less".to_string(), "-R".to_string()])
        );
    }

    #[test]
    fn less_default_when_no_var() {
        assert_eq!(
            decide_pager(env()),
            PagerChoice::Pager(vec!["less".to_string(), "-R".to_string()])
        );
    }

    #[test]
    fn direct_when_less_missing_and_no_var() {
        let e = PagerEnv {
            less_available: false,
            ..env()
        };
        assert_eq!(decide_pager(e), PagerChoice::Direct);
    }
}
