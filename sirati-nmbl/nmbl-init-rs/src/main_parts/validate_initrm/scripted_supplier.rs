//! [`ScriptedPasswordSupplier`] — the dry-run's headless passphrase
//! supplier.
//!
//! The genuine boot uses `TuiPasswordSupplier`, which pops a ratatui
//! modal and drives `console.poll_event` until the operator types a
//! passphrase. Under `--validate-initrm` there is no operator and the
//! console is a `NoopConsole` whose `poll_event` ignores its timeout, so
//! driving the real prompt would busy-spin at 100% CPU forever. This
//! supplier instead returns a FIXED placeholder passphrase immediately,
//! with ZERO console interaction — the dry-run only needs *some* bytes to
//! feed cryptsetup's presence-check, never a correct key.

use std::future::Future;
use std::pin::Pin;

use zeroize::Zeroizing;

use nmbl_init::activation::PasswordSupplier;
use nmbl_init::error::Result;
use nmbl_init::ui::console::Console;

/// Headless [`PasswordSupplier`] for the dry run: returns a fixed
/// placeholder passphrase without touching the console. Modelled on the
/// activation tests' `MockSupplier`.
pub(super) struct ScriptedPasswordSupplier;

impl PasswordSupplier for ScriptedPasswordSupplier {
    fn prompt<'a>(
        &'a mut self,
        _console: &'a mut dyn Console,
        _label: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Zeroizing<String>>> + 'a>> {
        // Fixed placeholder — the dry-run's `DryRunSys::run_with_tick`
        // only presence-checks cryptsetup and never forks it, so the
        // passphrase value is irrelevant. No console interaction, no spin.
        Box::pin(async move { Ok(Zeroizing::new("dryrun".to_string())) })
    }
}
