//! Recovery orchestration: drive the local emergency menu and — with
//! `remote-tui` — the remote attach server concurrently on the single
//! `LocalRuntime`.

use crate::config::Config;
#[cfg(feature = "remote-tui")]
use crate::error::NmblError;
use crate::terminal::TerminalAction;
use crate::ui::app::App;
use crate::ui::console::Console;
use crate::ui::{SessionInteraction, run_emergency_screen_with_app};
#[cfg(feature = "remote-tui")]
use crate::ui::{build_emergency_app, build_message, default_items};

use super::dispatch_emergency_choice;

/// Drive the local emergency menu — and, with `remote-tui`, the remote
/// attach server CONCURRENTLY on the same `LocalRuntime`. Returns the
/// [`TerminalAction`] the dispatcher in `main` performs after the stack
/// unwinds.
///
/// Without `remote-tui` this is just the local re-entrant picker loop
/// (today's behaviour, unchanged). With `remote-tui` the local loop and
/// [`crate::ui::remote::run_remote_server`] run together: whichever
/// produces a terminal action first wins, the other is torn down, and
/// the socket is unlinked.
#[cfg(not(feature = "remote-tui"))]
pub(super) async fn drive_recovery(
    console: &mut dyn Console,
    app: App<'static>,
    config: &Config,
    session: &SessionInteraction,
    emergency_timeout: std::time::Duration,
) -> TerminalAction {
    run_local_menu(console, app, config, session, emergency_timeout).await
}

/// `remote-tui` build: run the local menu and the remote server together.
#[cfg(feature = "remote-tui")]
pub(super) async fn drive_recovery(
    console: &mut dyn Console,
    app: App<'static>,
    config: &Config,
    session: &SessionInteraction,
    emergency_timeout: std::time::Duration,
) -> TerminalAction {
    use crate::ui::remote::{ActionSink, Shutdown, run_remote_server};

    let shutdown = Shutdown::new();
    let sink: ActionSink = std::rc::Rc::new(std::cell::RefCell::new(None));

    let local = run_local_menu(console, app, config, session, emergency_timeout);
    let server = run_remote_server(config, shutdown.clone(), sink.clone());

    tokio::select! {
        biased;
        // Local operator acted: signal the server to unlink + stop. The
        // server future is dropped by `select!`; its `SocketUnlinkGuard`
        // still unlinks on drop, and dropping in-flight remote sessions
        // EOFs their clients.
        action = local => {
            shutdown.signal();
            action
        }
        // The server only returns once a remote session committed an
        // action and signalled shutdown (or `bind_listener` failed, in
        // which case the sink is empty and we keep serving the local
        // menu). Read whichever action the first committer stored.
        () = server => {
            // Take the committed action out of the sink in a scoped
            // borrow so the `RefCell` reference is never held across the
            // fallback `.await` below.
            let committed = sink.borrow_mut().take();
            match committed {
                Some(action) => action,
                // Server exited without an action (bind failure): fall
                // back to the local menu alone so the local operator
                // still has a working recovery UI.
                None => {
                    run_local_menu(
                        console,
                        app_fallback(session),
                        config,
                        session,
                        emergency_timeout,
                    )
                    .await
                }
            }
        }
    }
}

/// Build a fresh emergency App for the local-menu fallback taken when the
/// remote server exits early (bind failure) before the local menu has
/// produced an action. The original `app` was moved into the now-dropped
/// local future, so we rebuild a parked-on-Emergency App.
#[cfg(feature = "remote-tui")]
fn app_fallback(session: &SessionInteraction) -> App<'static> {
    let err = NmblError::Io {
        source: std::io::Error::other("recovery menu"),
        context: "emergency".to_string(),
    };
    let message = build_message(&err);
    build_emergency_app(&message, &default_items(), session)
}

/// The local re-entrant emergency picker loop. Extracted so the
/// `remote-tui` wiring can race it against the remote server.
///
/// Re-entrant picker: the Raw Shell, Pretty Shell, Retry boot, and
/// Verify kexec readiness branches all return control to this loop on
/// exit. The Reboot branch — and the success arms of Retry/Verify —
/// diverge into a [`TerminalAction`].
async fn run_local_menu(
    console: &mut dyn Console,
    mut app: App<'static>,
    config: &Config,
    session: &SessionInteraction,
    emergency_timeout: std::time::Duration,
) -> TerminalAction {
    // Count of distinct failures surfaced this session. Used so the
    // persistent emergency-screen "error" box can show the LATEST
    // failure along with how many have been seen — see
    // `update_latest_error`.
    let mut error_count: u32 = 0;
    loop {
        // Modal state from the prior iteration (if any) must be
        // cleared before re-entering the picker; otherwise a stale
        // overlay would obscure the menu.
        app.modal = None;
        let choice = run_emergency_screen_with_app(console, &mut app, emergency_timeout).await;

        if let Some(action) =
            dispatch_emergency_choice(choice, console, &mut app, &mut error_count, config, session)
                .await
        {
            return action;
        }
    }
}
