//! Unit tests for the activation orchestrator: supplier/label coverage and
//! the `--validate-initrm` dry-run wrong-password-shell side-effect guard.

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
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Zeroizing<String>>> + 'a>> {
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

/// [`Console`] that replays a fixed key sequence (one event per
/// `poll_event`) and then yields `None`. Drives the wrong-password
/// modal to a committed button without a real backend.
struct KeyScriptConsole {
    keys: std::collections::VecDeque<crossterm::event::KeyEvent>,
}

impl KeyScriptConsole {
    fn new(codes: &[crossterm::event::KeyCode]) -> Self {
        use crossterm::event::{KeyEvent, KeyModifiers};
        Self {
            keys: codes
                .iter()
                .map(|c| KeyEvent::new(*c, KeyModifiers::NONE))
                .collect(),
        }
    }
}

impl Console for KeyScriptConsole {
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
        Ok(self.keys.pop_front().map(ConsoleEvent::Key))
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

/// A `--validate-initrm` dry run that scripts an exit-2 (wrong-password)
/// luks step and picks a recovery shell MUST NOT run the real seal nor
/// fork a real shell: under a [`DryRunSealScope`] the wrong-password
/// handler short-circuits to `ShellExited` before either side effect.
/// Real boot (no scope) is unaffected — that path is not exercised here
/// precisely because it would fork a real shell.
#[test]
fn wrong_password_shell_under_dry_run_skips_seal_and_fork() {
    use crossterm::event::KeyCode;

    let config = Config::recovery_default();
    let activation = Activation {
        kind: ActivationKind::LuksPassword,
        required_modules: Vec::new(),
        binary: PathBuf::from("/bin/cryptsetup"),
        argv: Vec::new(),
        produces_devices: Vec::new(),
        source_devices: Vec::new(),
        description: "test cryptroot".to_string(),
        prompt_label: None,
        pass_to_stage1: None,
    };

    // Pick a recovery shell button by its character shortcut: 's' =
    // Raw Shell (present with or without the pretty-shell feature). The
    // dry-run gate must divert BEFORE `seal_secrets`/`spawn_shell`.
    crate::policy::reset_real_seal_ops();
    let outcome = crate::ui::block_on_tui_with_poller(|sender| async move {
        let _scope = crate::policy::DryRunSealScope::enter();
        let mut console = KeyScriptConsole::new(&[KeyCode::Char('s')]);
        handle_wrong_password(&config, &mut console, &activation, 1, &sender).await
    })
    .expect("runtime builds")
    .expect("handler must not error on the dry-run gate");

    assert_eq!(
        outcome,
        WrongPasswordHandled::ShellExited,
        "dry-run shell pick must report ShellExited without forking"
    );
    assert_eq!(
        crate::policy::real_seal_ops(),
        0,
        "dry-run wrong-password shell must perform NO real seal op"
    );
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
