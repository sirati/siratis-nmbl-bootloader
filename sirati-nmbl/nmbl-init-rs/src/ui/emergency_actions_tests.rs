use super::*;
use std::path::PathBuf;
use std::pin::Pin;

use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};

use crate::generations::Generation;
use crate::ui::app::SessionInteraction;
use crate::ui::console::{ConsoleEvent, ConsoleKind};
use crate::ui::{build_emergency_app, default_items};

/// Minimal [`Console`] over a ratatui [`TestBackend`] so a test can
/// render a screen and then inspect the resulting cell buffer.
/// `render` and `draw_with` both go through `terminal.draw`, exactly
/// like the real tty/mock backends, so [`clear_console`] exercises
/// the same diff-render path that produced the bleed bug.
struct BufferConsole {
    terminal: Terminal<TestBackend>,
}

impl BufferConsole {
    fn new(w: u16, h: u16) -> Self {
        Self {
            terminal: Terminal::new(TestBackend::new(w, h)).expect("test terminal"),
        }
    }

    /// True if every cell is the default blank: a single space with
    /// no fg/bg colour and no modifiers. A residual emergency cell
    /// (the red "boot failed" header, the bordered "error" block,
    /// the highlighted action line) fails this.
    fn is_blank(&self) -> bool {
        let buf = self.terminal.backend().buffer();
        buf.content().iter().all(|cell| {
            cell.symbol() == " "
                && cell.fg == ratatui::style::Color::Reset
                && cell.bg == ratatui::style::Color::Reset
                && cell.modifier.is_empty()
        })
    }

    fn dump(&self) -> String {
        let buf = self.terminal.backend().buffer();
        buf.content().iter().map(|c| c.symbol()).collect()
    }
}

impl Console for BufferConsole {
    fn render(&mut self, app: &App<'_>) -> Result<()> {
        // TestBackend's draw is infallible; surface a clean unwrap.
        self.terminal
            .draw(|f| crate::ui::render_current_screen(f, app))
            .expect("TestBackend render");
        Ok(())
    }
    fn poll_event<'a>(
        &'a mut self,
        _timeout: Duration,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
        Box::pin(async move { Ok(None) })
    }
    fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
        Ok(None)
    }
    fn size(&self) -> (u16, u16) {
        match self.terminal.backend().size() {
            Ok(s) => (s.width, s.height),
            Err(_) => (0, 0),
        }
    }
    fn kind(&self) -> ConsoleKind {
        ConsoleKind::Tty
    }
    fn draw_with(&mut self, body: &mut dyn FnMut(&mut ratatui::Frame<'_>)) -> Result<()> {
        self.terminal
            .draw(|f| body(f))
            .expect("TestBackend draw_with");
        Ok(())
    }
    fn suspend(&mut self) -> Result<()> {
        Ok(())
    }
    fn resume(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The two emergency actions (retry boot, verify kexec readiness)
/// must blank the boot-failed menu before they render the selector,
/// or the menu's red header / bordered "error" block / highlighted
/// action line bleed through on the diff-rendering backend. Pin the
/// `clear_console` helper they both call: after it runs, no
/// emergency-screen cell survives.
#[test]
fn clear_console_blanks_residual_emergency_cells() {
    let session = SessionInteraction::new();
    let items = default_items();
    let app = build_emergency_app("boot phase failed: disk offline", &items, &session);

    let mut console = BufferConsole::new(80, 24);
    // Paint the emergency screen, exactly what the operator sees
    // behind the menu before picking retry / verify.
    console.render(&app).expect("render emergency screen");
    assert!(
        !console.is_blank(),
        "emergency screen must paint visible chrome before the clear"
    );
    assert!(
        console.dump().contains("boot failed"),
        "sanity: the red header is on screen pre-clear"
    );

    // The transition the two fixed actions perform before run_selector.
    clear_console(&mut console).expect("clear_console");

    assert!(
        console.is_blank(),
        "no emergency-screen cell may survive the clear:\n{}",
        console.dump()
    );
}

fn fake_gen(number: u32) -> Generation {
    Generation {
        number,
        profile_link: PathBuf::from(format!("/p/system-{number}-link")),
        toplevel: PathBuf::from(format!("/p/toplevel-{number}")),
        kernel: PathBuf::from("/p/kernel"),
        initrd: PathBuf::from("/p/initrd"),
        init_path: PathBuf::from(format!("/p/system-{number}-link/init")),
        kernel_params: Vec::new(),
        label: String::new(),
    }
}

#[test]
fn decision_to_action_reboot_yields_reboot() {
    let cfg = Config::recovery_default();
    let gens = vec![fake_gen(1)];
    let mut ops = RealSys::sync_only();
    let action = decision_to_action(&mut ops, &cfg, &gens, &[], Decision::Reboot)
        .expect("Reboot decision must produce TerminalAction::Reboot");
    assert!(matches!(action, TerminalAction::Reboot));
}

#[test]
fn decision_to_action_shell_yields_tui_error() {
    // The emergency-retry path cannot honour Decision::Shell — the
    // operator is already on the emergency screen, so dropping to
    // the shell from here would either be a no-op or an infinite
    // loop. Pin the explicit `NmblError::Tui` translation.
    let cfg = Config::recovery_default();
    let gens = vec![fake_gen(1)];
    let mut ops = RealSys::sync_only();
    let err = decision_to_action(&mut ops, &cfg, &gens, &[], Decision::Shell)
        .expect_err("Shell decision must produce an error inside retry path");
    assert!(matches!(err, NmblError::Tui { .. }));
}

#[test]
fn decision_to_action_out_of_range_index_yields_config_invalid() {
    // Defence-in-depth: a buggy selector returning an index past
    // the generations slice must produce a structured error, not a
    // panic-on-indexing. The error context names the dispatcher so
    // a future regression is easy to bisect from the boot log.
    let cfg = Config::recovery_default();
    let gens = vec![fake_gen(1)];
    let mut ops = RealSys::sync_only();
    let err = decision_to_action(
        &mut ops,
        &cfg,
        &gens,
        &[],
        Decision::Boot {
            generation_index: 42,
            cmdline_override: None,
        },
    )
    .expect_err("out-of-range index must error, not panic");
    match err {
        NmblError::ConfigInvalid { context, reason } => {
            assert!(
                context.contains("emergency-retry"),
                "context must name the dispatcher: {context}"
            );
            assert!(
                reason.contains("42"),
                "reason must mention bad index: {reason}"
            );
        }
        other => panic!("expected ConfigInvalid, got {other:?}"),
    }
}
