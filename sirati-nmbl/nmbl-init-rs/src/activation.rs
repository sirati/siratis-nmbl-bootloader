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
use crate::generations::Generation;
use crate::sys::activation::{ProcessOutcome, run, run_with_tick};
use crate::ui::BootReporter;
use crate::ui::app::{App, Screen};
use crate::ui::console::Console;
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
///
/// The supplier receives the live boot console for the duration of the
/// prompt so it can render the passphrase modal through the SAME backend
/// (splash framebuffer or raw-mode tty) the operator has been looking at
/// since phase 1 — no parallel console bring-up, no flicker. Production
/// callers (`run_all_activations`) hand it the `BootReporter`'s console;
/// tests can pass a mock that ignores it.
pub trait PasswordSupplier {
    fn prompt(&mut self, console: &mut dyn Console, label: &str) -> Result<Zeroizing<String>>;
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
pub fn run_all_activations(
    config: &Config,
    reporter: &mut BootReporter<'_, '_>,
    mut password_supplier: Option<&mut dyn PasswordSupplier>,
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
            let supplier_ref: Option<&mut dyn PasswordSupplier> = match password_supplier {
                Some(ref mut s) => Some(&mut **s),
                None => None,
            };
            // Briefly hand the reporter's console to the supplier so the
            // passphrase modal renders through the same backend as the
            // surrounding boot-status screen. The reporter borrow is
            // paused for the duration of the prompt and resumed after.
            let stdin_owned = collect_stdin(activation, &mut *reporter.console, supplier_ref)?;
            let stdin_slice = stdin_owned.as_ref().map(|z| z.as_slice());

            // For luks-password activations, drive a spinner on the
            // passphrase modal while cryptsetup verifies the key — the
            // operator sees the boot is alive rather than hung between
            // pressing Enter and the unlock result. Other activation
            // kinds (LVM, mdraid, …) keep the simpler blocking `run`.
            let outcome = if activation.kind == ActivationKind::LuksPassword {
                run_luks_with_spinner(activation, stdin_slice, &mut *reporter.console)?
            } else {
                run(&activation.binary, &activation.argv, stdin_slice)
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
                match handle_wrong_password(config, &mut *reporter.console, activation, attempts)?
                {
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

        let device_count = activation.produces_devices.len();
        let wait_operation = format!("phase 3: {} waiting for", kind_label(activation.kind));
        for device in &activation.produces_devices {
            // Drive the spinner / status line while we wait so a slow
            // activation (LUKS unlock, LVM scan) doesn't look frozen.
            wait_for(
                device,
                DEVICE_WAIT_TIMEOUT,
                &wait_operation,
                Some(&mut *reporter),
            )?;
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
    let secret = supplier.prompt(console, label)?;
    let mut buf = Zeroizing::new(Vec::with_capacity(secret.len()));
    buf.extend_from_slice(secret.as_bytes());
    Ok(Some(buf))
}

/// Run a `luks-password` activation under [`run_with_tick`], using the
/// boot console to paint a verifying-spinner on the passphrase modal
/// every ~150 ms. Returns the same [`ProcessOutcome`] [`run`] would.
///
/// The App owned here is throwaway: it carries a `Screen::Passphrase`
/// in verifying mode so the existing `render_passphrase` view paints
/// the same modal the operator saw during input, with a spinner row
/// overlaid. We do NOT share state with the supplier's App (which was
/// consumed inside `collect_stdin`) — a fresh App is cheaper than
/// threading a mutable reference through the supplier trait.
fn run_luks_with_spinner(
    activation: &Activation,
    stdin_slice: Option<&[u8]>,
    console: &mut dyn Console,
) -> Result<ProcessOutcome> {
    let label = activation
        .prompt_label
        .as_deref()
        .unwrap_or("Verifying passphrase")
        .to_string();

    // Throwaway App parked on the passphrase modal in verifying mode.
    // `generations` is empty — the modal renders the same way against
    // any (or no) generation slice. `'static` works because we hand
    // out `&[]` at the constructor.
    let empty: [Generation; 0] = [];
    let mut app = App::new(&empty);
    app.screen = Screen::Passphrase {
        prompt_label: label,
        // Buffer length carries through to the dotted mask. We can't
        // know the operator's actual byte count cheaply here without
        // crossing the supplier API; the stdin slice is one byte per
        // input character (the supplier doesn't add a newline), so
        // its length is a faithful approximation.
        buffer: zeroize::Zeroizing::new("*".repeat(stdin_slice.map_or(0, <[u8]>::len))),
        verifying: true,
        spinner_frame: 0,
    };

    // Paint the first verifying frame BEFORE the child starts — so the
    // operator sees the spinner pop up the instant they press Enter,
    // not after the first 150 ms tick.
    let _ = console.render(&app);

    let tick = |c: &mut dyn Console, a: &mut App<'_>| {
        a.tick_passphrase_spinner();
        let _ = c.render(a);
    };

    // The tick closure needs &mut on both console and app at the same
    // time. We can't capture both in one FnMut because run_with_tick
    // would then need to thread them; instead, the closure captures
    // `&mut *console` and `&mut app` via mutable borrows held in this
    // function frame. Use a RefCell-free split: keep the closure
    // borrowing locals declared before the call.
    //
    // (Rust 1.83 closure borrow rules now make this clean — the
    // closure captures `&mut app` and `&mut *console` directly.)
    let mut cb = || tick(console, &mut app);

    let outcome = run_with_tick(
        &activation.binary,
        &activation.argv,
        stdin_slice,
        Some(&mut cb as &mut dyn FnMut()),
    )
    .map_err(|source| wrap_runner_error(activation, source))?;

    // Done verifying — clear the overlay and repaint once so the next
    // screen transition (success → boot-status; wrong-pw → modal) starts
    // from a clean slate.
    app.set_passphrase_verifying(false);
    let _ = console.render(&app);

    Ok(outcome)
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

/// Exit codes that mean the activation step is satisfied. `0` is the
/// obvious success; `5` from cryptsetup luksOpen means "device already
/// active" — the device-mapper mapping survived from a prior attempt
/// this session, so the LUKS volume is already open and accessible
/// rather than re-opened. Treating it as fatal would block kexec for
/// no reason.
fn is_activation_success(exit_code: i32) -> bool {
    exit_code == 0 || exit_code == 5
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

/// Resolved outcome of [`handle_wrong_password`]. Distinct from
/// [`crate::ui::WrongPasswordOutcome`] (the modal-level reply) because
/// the helper also drives the in-process shell session before
/// returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WrongPasswordHandled {
    /// Re-prompt for the passphrase and re-run cryptsetup.
    TryAgain,
    /// Operator picked [Reboot] on the wrong-password modal.
    Reboot,
    /// Operator opened a recovery shell (Pretty Shell or Raw Shell)
    /// and the shell has now exited. Caller turns this into
    /// [`NmblError::WrongPasswordShellExited`] so the standard
    /// emergency menu can surface and offer [Retry boot from config].
    ShellExited,
}

/// Render the wrong-password modal, dispatch on the operator's choice,
/// and — for the shell branches — drive the chosen in-process shell
/// session. Returns when the operator's choice has been fully
/// resolved (modal closed; shell, if any, has exited).
fn handle_wrong_password(
    config: &Config,
    console: &mut dyn Console,
    _activation: &Activation,
    attempt: u32,
) -> Result<WrongPasswordHandled> {
    use crate::ui::{WrongPasswordOutcome, show_wrong_password_modal};

    match show_wrong_password_modal(console, attempt)? {
        WrongPasswordOutcome::TryAgain => Ok(WrongPasswordHandled::TryAgain),
        WrongPasswordOutcome::Reboot => Ok(WrongPasswordHandled::Reboot),
        #[cfg(feature = "pretty-shell")]
        WrongPasswordOutcome::PrettyShell => {
            if let Err(e) = crate::ui::pretty_shell::run_pretty_shell(console, config) {
                let chain = crate::error::format_chain(&e as &dyn std::error::Error);
                crate::nmbl_warn!("wrong-password pretty-shell failed: {chain}");
                let _ = crate::ui::show_modal_error(
                    console,
                    "Pretty Shell failed to start",
                    &chain,
                    std::time::Duration::from_secs(10),
                );
            }
            Ok(WrongPasswordHandled::ShellExited)
        }
        WrongPasswordOutcome::RawShell => {
            // Console-picker + multiplexed busybox PTY (overlap) or
            // fire-and-forget (no overlap). Errors are surfaced via a
            // modal-error so the wrong-password flow doesn't crash the
            // boot — we still want the operator to be able to retry.
            match crate::ui::console_picker::run_picker_session(console, config) {
                Ok(crate::ui::console_picker::PickerSessionOutcome::ShellDetached {
                    targets,
                }) => {
                    let joined = targets
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = crate::ui::show_modal_error(
                        console,
                        "Shell spawned",
                        &format!("Shell spawned on {joined}"),
                        std::time::Duration::from_secs(5),
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    let chain = crate::error::format_chain(&e as &dyn std::error::Error);
                    crate::nmbl_warn!("wrong-password shell-picker session failed: {chain}");
                    let _ = crate::ui::show_modal_error(
                        console,
                        "Emergency shell failed",
                        &chain,
                        std::time::Duration::from_secs(10),
                    );
                }
            }
            Ok(WrongPasswordHandled::ShellExited)
        }
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
        fn prompt(
            &mut self,
            _console: &mut dyn Console,
            label: &str,
        ) -> Result<Zeroizing<String>> {
            self.seen_label = Some(label.to_string());
            Ok(Zeroizing::new(self.canned.to_string()))
        }
    }

    /// Minimal [`Console`] that ignores every call. Lets us exercise the
    /// supplier trait without bringing up a real backend.
    struct NoopConsole;

    impl Console for NoopConsole {
        fn render(&mut self, _app: &crate::ui::app::App<'_>) -> Result<()> {
            Ok(())
        }
        fn poll_event(
            &mut self,
            _timeout: Duration,
        ) -> Result<Option<crate::ui::console::ConsoleEvent>> {
            Ok(None)
        }
        fn size(&self) -> (u16, u16) {
            (80, 24)
        }
        fn kind(&self) -> crate::ui::console::ConsoleKind {
            crate::ui::console::ConsoleKind::Tty
        }
        fn draw_with(
            &mut self,
            _body: &mut dyn FnMut(&mut ratatui::Frame<'_>),
        ) -> Result<()> {
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
        let got = sup
            .prompt(&mut console, "Unlock root")
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
