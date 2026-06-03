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
mod seal;
mod source_wait;

use std::path::PathBuf;
use std::time::Duration;

use zeroize::Zeroizing;

use crate::config::{Activation, ActivationKind, Config};
use crate::error::{NmblError, Result};
use crate::nmbl_info;
use crate::sys::ops::SysOps;
use crate::sys::poller::LocalSender;
use crate::ui::BootReporter;
use crate::ui::console::Console;

use helpers::{
    check_required_modules, collect_stdin, exit_code_error, is_activation_success, kind_label,
    loaded_modules, wrap_runner_error,
};
use luks::{WrongPasswordHandled, handle_wrong_password, run_luks_with_spinner};
use seal::{luks_tpm_mapper_name, register_tpm_mapper_if_luks_tpm};
use source_wait::wait_for_source_device;

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
    sender: &LocalSender,
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

        // Wait for each backing (source) device to materialise BEFORE we
        // prompt for a passphrase / exec cryptsetup. USB storage and slow
        // HBAs enumerate partition nodes asynchronously a moment after the
        // driver loads, so the one-shot phase-2c by-* sweep can miss them
        // and hand cryptsetup a non-existent /dev/disk/by-partlabel/...
        // path (→ exit code 4). Re-sweep on each poll so the link appears
        // the instant the node does. The fast path (device already
        // present) is a single existence check — no added latency.
        wait_for_source_devices(ops, config, activation, reporter).await?;

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

/// Wait for every `source_devices` entry of `activation` to appear,
/// re-running the `/dev/disk/by-*` symlink sweep on each poll so a
/// partition node the kernel enumerated asynchronously gets its by-*
/// links before cryptsetup is reached. Bounded per device by
/// `config.general.device_timeout_secs`; the spinner advances via the
/// reporter so Esc still aborts. Delegates to the generic
/// [`wait_for_source_device`], routing the existence check and re-sweep
/// through `ops` so a dry-run makes the wait trivially-ready.
async fn wait_for_source_devices<S: SysOps>(
    ops: &mut S,
    config: &Config,
    activation: &Activation,
    reporter: &mut BootReporter<'_, '_>,
) -> Result<()> {
    if activation.source_devices.is_empty() {
        return Ok(());
    }
    let timeout = Duration::from_secs(config.general.device_timeout_secs);
    let operation = format!(
        "phase 3: {} waiting for source",
        kind_label(activation.kind)
    );
    for device in &activation.source_devices {
        wait_for_source_device(ops, device, timeout, &operation, Some(&mut *reporter)).await?;
    }
    Ok(())
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
    sender: &LocalSender,
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
            run_luks_with_spinner(activation, stdin_slice, console, ops).await?
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
            // DISTINCT TPM-unseal marker. A `luks-tpm` activation runs
            // `cryptsetup open --token-only …`, which unseals the LUKS key
            // from the TPM2-sealed token and CANNOT fall back to a password
            // keyslot — so a success here is unambiguously a genuine TPM
            // unseal, never a passphrase unlock. The roundtrip test keys on
            // this line (and asserts no password prompt) so it can never
            // false-pass on a fallback (shares no substring with the generic
            // "activation … completed" line below).
            if activation.kind == ActivationKind::LuksTpm {
                nmbl_info!(
                    "luks-tpm: unsealed {} via TPM token (cryptsetup --token-only)",
                    luks_tpm_mapper_name(activation).unwrap_or("cryptroot"),
                );
            }
            // A TPM-unsealed LUKS mapper is now live. Record it on the
            // always-compiled seal registry so `policy::seal_secrets`
            // closes it (cryptsetup close) before any interactive context
            // is reached — a refuse/rescue/shell must leave no readable
            // TPM-unsealed plaintext device behind (FIX-03 / re-audit C-1).
            register_tpm_mapper_if_luks_tpm(activation);
            break stdin_owned;
        }

        // Wrong-password fast path: cryptsetup signals "no key
        // available" via exit code 2. Show the retry modal; any
        // other non-zero exit code is fatal as before.
        if activation.kind == ActivationKind::LuksPassword && outcome.exit_code == 2 {
            attempts = attempts.saturating_add(1);
            match handle_wrong_password(config, console, activation, attempts, sender).await? {
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
#[path = "tests.rs"]
mod tests;
