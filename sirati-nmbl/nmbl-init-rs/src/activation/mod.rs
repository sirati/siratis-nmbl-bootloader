//! Storage activation orchestrator (Phase C.6) — drives LVM / mdraid /
//! cryptsetup / zpool between module load and system-fs mount. For each
//! `config.activations` entry: warn about missing `required_modules`
//! (built-ins aren't in `/proc/modules`, so a hard check would false-
//! positive), prompt the supplied `PasswordSupplier` for `luks-password`,
//! exec via `sys::activation::run` (non-zero exit is fatal), then block
//! until every `produces_devices` path is stat-able (15 s budget, then
//! `NmblError::DeviceTimeout`). All policy lives here; `sys::activation`
//! is pure exec mechanism.

mod helpers;
mod luks;

use std::path::PathBuf;
use std::time::Duration;

use zeroize::Zeroizing;

use crate::config::{Activation, ActivationKind, Config};
use crate::error::{NmblError, Result};
use crate::nmbl_info;
use crate::sys::ops::SysOps;
use crate::ui::BootReporter;
use crate::ui::console::Console;

use helpers::{
    check_required_modules, collect_stdin, exit_code_error, is_activation_success, kind_label,
    loaded_modules, wrap_runner_error,
};
use luks::{WrongPasswordHandled, handle_wrong_password, run_luks_with_spinner};

/// One passphrase to inject into the kexec'd initrd as a keyfile. The
/// activation runner emits one of these per `luks-password` activation
/// whose TOML carries a `pass_to_stage1 = "<path>"` field. The kexec
/// path appends a cpio fragment containing `<path>` with `secret` as
/// its contents, so stage-1's NixOS init can use it as a keyFile.
pub struct KeyInjection {
    pub path: PathBuf,
    pub secret: Zeroizing<Vec<u8>>,
}

/// Pluggable passphrase prompt; TUI implements it, tests mock it.
/// `Zeroizing` wipes the buffer on drop, including on error paths.
///
/// The supplier receives the live boot console for the duration of the
/// prompt so it can render the passphrase modal through the SAME backend
/// (splash framebuffer or raw-mode tty) the operator has been looking at
/// since phase 1 — no parallel console bring-up, no flicker. Production
/// callers (`run_all_activations`) hand it the `BootReporter`'s console;
/// tests can pass a mock that ignores it.
pub trait PasswordSupplier {
    /// Prompt for a passphrase, `.await`ing the modal's input instead of
    /// blocking. Returns a boxed future (not a native async-fn-in-trait)
    /// so the trait stays object-safe — the activation runner drives it
    /// through `&mut dyn PasswordSupplier`.
    fn prompt<'a>(
        &'a mut self,
        console: &'a mut dyn Console,
        label: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Zeroizing<String>>> + 'a>>;
}

/// Run every entry in declaration order. First failure is fatal —
/// activations chain (LUKS → LVM → fs), so a partial run leaves
/// Phase 3 unable to find its devices.
///
/// `reporter` carries the live boot console; we surface each activation
/// kind + description as the boot-status phase label so the operator
/// sees which step is in flight (LUKS unlock, LVM activate, …).
///
/// Returns the set of [`KeyInjection`]s the kexec path must append to
/// the system initrd (one per `luks-password` activation whose TOML
/// sets `pass_to_stage1`). The vec is empty when no activation opts in.
pub async fn run_all_activations<S: SysOps>(
    ops: &mut S,
    config: &Config,
    reporter: &mut BootReporter<'_, '_>,
    mut password_supplier: Option<&mut dyn PasswordSupplier>,
    sender: &crate::sys::poller::LocalSender,
) -> Result<Vec<KeyInjection>> {
    let mut injections: Vec<KeyInjection> = Vec::new();
    if config.activations.is_empty() {
        return Ok(injections);
    }

    let loaded = loaded_modules()?;

    for activation in &config.activations {
        let _ = reporter.set_phase(format!(
            "phase 3: {} ({})",
            kind_label(activation.kind),
            activation.description,
        ));
        check_required_modules(activation, &loaded);

        // Per-iteration reborrow for password_supplier so the compiler
        // doesn't keep the mutable borrow live across loop turns.
        let supplier_ref: Option<&mut dyn PasswordSupplier> = match password_supplier {
            Some(ref mut s) => Some(&mut **s),
            None => None,
        };
        let stdin_owned = run_one_activation(
            ops,
            config,
            activation,
            &mut *reporter.console,
            supplier_ref,
            sender,
        )
        .await?;

        let device_count = activation.produces_devices.len();
        let wait_operation = format!("phase 3: {} waiting for", kind_label(activation.kind));
        let device_timeout = Duration::from_secs(config.general.device_timeout_secs);
        for device in &activation.produces_devices {
            // Drive the spinner / status line while we wait so a slow
            // activation (LUKS unlock, LVM scan) doesn't look frozen.
            ops.wait_for_device(
                device,
                device_timeout,
                &wait_operation,
                Some(&mut *reporter),
            )
            .await?;
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

/// Drive the retry loop for a single activation entry. Returns the stdin
/// bytes that were fed to the child (passphrase bytes for a
/// `luks-password` activation, `None` for every other kind) so the
/// caller can hand them off for kexec key injection.
///
/// On exit code 2 from a `luks-password` step the wrong-password modal
/// is shown and the operator can retry without restarting the boot.
/// Every other non-zero exit code is fatal.
async fn run_one_activation<S: SysOps>(
    ops: &mut S,
    config: &Config,
    activation: &Activation,
    console: &mut dyn Console,
    mut supplier: Option<&mut dyn PasswordSupplier>,
    sender: &crate::sys::poller::LocalSender,
) -> Result<Option<Zeroizing<Vec<u8>>>> {
    // 1-indexed attempt counter for the wrong-password modal title
    // ("Wrong password (attempt N)"). Resets per activation so a
    // later LUKS device starts at attempt 1 again.
    let mut attempts: u32 = 0;
    // Inner loop: run the activation, and on exit code 2 from a
    // `luks-password` step surface the wrong-password modal so the
    // operator can retry without restarting the boot. Every other
    // exit code, plus a wrong-password modal that returns Reboot
    // or Shell, leaves this loop via `return`.
    let stdin_owned: Option<Zeroizing<Vec<u8>>> = loop {
        // Per-iteration reborrow; without it the compiler keeps the
        // mutable borrow live across loop turns and rejects iter #2.
        let supplier_ref: Option<&mut dyn PasswordSupplier> = match supplier {
            Some(ref mut s) => Some(&mut **s),
            None => None,
        };
        // Briefly hand the console to the supplier so the passphrase
        // modal renders through the same backend as the surrounding
        // boot-status screen.
        let stdin_owned = collect_stdin(activation, console, supplier_ref).await?;
        let stdin_slice = stdin_owned.as_ref().map(|z| z.as_slice());

        // For luks-password activations, drive a spinner on the
        // passphrase modal while cryptsetup verifies the key — the
        // operator sees the boot is alive rather than hung between
        // pressing Enter and the unlock result. Other activation
        // kinds (LVM, mdraid, …) keep the simpler blocking `run`.
        let outcome = if activation.kind == ActivationKind::LuksPassword {
            run_luks_with_spinner(activation, stdin_slice, console, sender).await?
        } else {
            ops.run(&activation.binary, &activation.argv, stdin_slice)
                .await
                .map_err(|source| wrap_runner_error(activation, source))?
        };

        // Exit code 0 is the obvious success; exit code 5 also
        // means success for cryptsetup luksOpen — the device-mapper
        // mapping already exists (already-open volume from a prior
        // attempt this session), so cryptsetup refuses to re-open
        // rather than failing. The LUKS volume is accessible either
        // way, so treat both as a clean break.
        if is_activation_success(outcome.exit_code) {
            break stdin_owned;
        }

        // Wrong-password fast path: cryptsetup signals "no key
        // available" via exit code 2. Show the retry modal; any
        // other non-zero exit code is fatal as before.
        if activation.kind == ActivationKind::LuksPassword && outcome.exit_code == 2 {
            attempts = attempts.saturating_add(1);
            match handle_wrong_password(config, console, activation, attempts).await? {
                WrongPasswordHandled::TryAgain => continue,
                WrongPasswordHandled::Reboot => {
                    return Err(NmblError::OperatorChoseReboot {
                        context: format!(
                            "activation {} ({})",
                            kind_label(activation.kind),
                            activation.description,
                        ),
                    });
                }
                WrongPasswordHandled::ShellExited => {
                    return Err(NmblError::WrongPasswordShellExited {
                        context: format!(
                            "activation {} ({})",
                            kind_label(activation.kind),
                            activation.description,
                        ),
                    });
                }
            }
        }

        return Err(exit_code_error(activation, outcome));
    };
    Ok(stdin_owned)
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
    use std::time::Duration;

    use super::*;
    use crate::config::ActivationKind;
    use crate::ui::console::{ConsoleEvent, ConsoleKind};

    /// Fixed-passphrase supplier. Exercises the trait shape; a real
    /// integration test would require a live cryptsetup + LUKS image.
    struct MockSupplier {
        canned: &'static str,
        seen_label: Option<String>,
    }

    impl PasswordSupplier for MockSupplier {
        fn prompt<'a>(
            &'a mut self,
            _console: &'a mut dyn Console,
            label: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Zeroizing<String>>> + 'a>>
        {
            self.seen_label = Some(label.to_string());
            let canned = self.canned.to_string();
            Box::pin(async move { Ok(Zeroizing::new(canned)) })
        }
    }

    /// Minimal [`Console`] that ignores every call. Lets us exercise the
    /// supplier trait without bringing up a real backend.
    struct NoopConsole;

    impl Console for NoopConsole {
        fn render(&mut self, _app: &crate::ui::app::App<'_>) -> Result<()> {
            Ok(())
        }
        fn poll_event<'a>(
            &'a mut self,
            timeout: Duration,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>>
        {
            Box::pin(async move { self.poll_event_blocking(timeout) })
        }
        fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
            Ok(None)
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> ConsoleKind {
            ConsoleKind::Tty
        }
        fn draw_with(&mut self, _body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
            Ok(())
        }
        fn suspend(&mut self) -> Result<()> {
            Ok(())
        }
        fn resume(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn mock_supplier_and_kind_labels() {
        let mut sup = MockSupplier {
            canned: "hunter2",
            seen_label: None,
        };
        let mut console = NoopConsole;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build_local(tokio::runtime::LocalOptions::default())
            .expect("test runtime");
        let got = rt
            .block_on(sup.prompt(&mut console, "Unlock root"))
            .expect("mock never errors");
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
