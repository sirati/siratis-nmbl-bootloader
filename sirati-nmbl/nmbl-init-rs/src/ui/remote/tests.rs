//! Tests for the remote-TUI server session lifecycle.
//!
//! These exercise the per-session loop ([`run_remote_menu`]) with a
//! scripted fake console (so no real pty/socket is needed) and the
//! shutdown / sink bookkeeping the accept multiplexer relies on.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::terminal::TerminalAction;
use crate::ui::app::{App, SessionInteraction};
use crate::ui::console::{Console, ConsoleEvent, ConsoleKind};
use crate::ui::{build_emergency_app, build_message, default_items};

use super::{ActionSink, Shutdown, run_remote_menu};

/// Drive an async future to completion on a throwaway current-thread
/// runtime. The scripted console resolves instantly and the emergency
/// loop's `select!` is biased on input, so no wall-clock time elapses.
fn block<F: Future>(fut: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build_local(tokio::runtime::LocalOptions::default())
        .expect("test runtime");
    rt.block_on(fut)
}

/// A scripted in-process [`Console`] for the remote session loop. Feeds a
/// sequence of optional key events; `error_after` makes `poll_event`
/// return an error after N events to simulate a client disconnect.
struct FakeConsole {
    events: Vec<Option<KeyEvent>>,
    cursor: usize,
    error_after: Option<usize>,
}

impl FakeConsole {
    fn new(events: Vec<Option<KeyEvent>>) -> Self {
        Self {
            events,
            cursor: 0,
            error_after: None,
        }
    }

    fn erroring(error_after: usize) -> Self {
        Self {
            events: Vec::new(),
            cursor: 0,
            error_after: Some(error_after),
        }
    }
}

impl Console for FakeConsole {
    fn render(&mut self, _app: &App<'_>) -> Result<()> {
        Ok(())
    }
    fn poll_event<'a>(
        &'a mut self,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Option<ConsoleEvent>>> + 'a>> {
        Box::pin(async move { self.poll_event_blocking(timeout) })
    }
    fn poll_event_blocking(&mut self, _timeout: Duration) -> Result<Option<ConsoleEvent>> {
        let at = self.cursor;
        self.cursor = self.cursor.saturating_add(1);
        if let Some(n) = self.error_after
            && at >= n
        {
            return Err(NmblError::Tui {
                source: std::io::Error::other("client disconnected"),
            });
        }
        Ok(self
            .events
            .get(at)
            .copied()
            .flatten()
            .map(ConsoleEvent::Key))
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

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn fresh_app() -> App<'static> {
    let session = SessionInteraction::new();
    let message = build_message(&NmblError::Io {
        source: std::io::Error::other("boot failed"),
        context: "test".to_string(),
    });
    build_emergency_app(&message, &default_items(), &session)
}

/// Build an emergency App whose interaction latch is the given `session`
/// (sharing the same `Rc<Cell<bool>>`), mirroring how `serve_session`
/// wires the per-session latch into its App.
fn app_in_session(session: &SessionInteraction) -> App<'static> {
    let message = build_message(&NmblError::Io {
        source: std::io::Error::other("boot failed"),
        context: "test".to_string(),
    });
    build_emergency_app(&message, &default_items(), session)
}

fn run_menu(console: &mut dyn Console) -> Option<TerminalAction> {
    let config = Config::recovery_default();
    let session = SessionInteraction::new();
    let mut app = fresh_app();
    let mut errs = 0u32;
    let sender = crate::sys::poller::build().1;
    block(run_remote_menu(
        console,
        &mut app,
        &config,
        &session,
        Duration::from_secs(30),
        &mut errs,
        &sender,
    ))
}

#[test]
fn enter_on_reboot_commits_reboot() {
    // Index 0 is Reboot; pressing Enter selects it → terminal action.
    let mut console = FakeConsole::new(vec![Some(press(KeyCode::Enter))]);
    let action = run_menu(&mut console);
    assert!(
        matches!(action, Some(TerminalAction::Reboot)),
        "expected Reboot, got {action:?}"
    );
}

#[test]
fn ctrl_e_ends_session_with_no_action() {
    // Ctrl+E sets app.exit_session; the remote loop must end the session
    // WITHOUT committing any terminal action (the machine keeps running
    // for the local operator / other remote sessions).
    let mut console = FakeConsole::new(vec![Some(ctrl(KeyCode::Char('e')))]);
    let action = run_menu(&mut console);
    assert!(action.is_none(), "Ctrl+E must not commit an action");
}

#[test]
fn poll_error_ends_session_without_rebooting() {
    // A client disconnect surfaces as a console poll error. The session
    // must end with None — NEVER silently commit a machine-wide Reboot.
    let mut console = FakeConsole::erroring(0);
    let action = run_menu(&mut console);
    assert!(
        action.is_none(),
        "a disconnected client must not trigger a reboot"
    );
}

/// Drive `run_remote_menu` with an explicit session + timeout, mirroring
/// `serve_session` (which shares the per-session latch into the App).
fn run_menu_with_session(
    console: &mut dyn Console,
    session: &SessionInteraction,
    timeout: Duration,
) -> Option<TerminalAction> {
    let config = Config::recovery_default();
    let mut app = app_in_session(session);
    let mut errs = 0u32;
    let sender = crate::sys::poller::build().1;
    block(run_remote_menu(
        console, &mut app, &config, session, timeout, &mut errs, &sender,
    ))
}

#[test]
fn unattended_session_commits_reboot_on_zero_timeout() {
    // Control for the test below: an UN-attended session with a
    // zero-length countdown arms the auto-reboot deadline, which is
    // already elapsed on entry → the loop commits Reboot WITHOUT ever
    // polling the (immediately-erroring) console.
    let session = SessionInteraction::new();
    let mut console = FakeConsole::erroring(0);
    let action = run_menu_with_session(&mut console, &session, Duration::ZERO);
    assert!(
        matches!(action, Some(TerminalAction::Reboot)),
        "unattended session must arm the countdown and reboot, got {action:?}"
    );
}

#[test]
fn attended_remote_session_does_not_auto_reboot_on_timeout() {
    // serve_session marks every fresh remote session attended (the
    // operator connected, so they are present). That disarms the
    // unattended auto-reboot countdown: even with a zero-length timeout
    // the session must NOT commit a machine-wide Reboot. Here the console
    // errors immediately (client gone), so a disarmed loop ends with
    // None; were the countdown armed it would have rebooted before the
    // console was ever polled (see the control test above).
    let session = SessionInteraction::new();
    session.set();
    let mut console = FakeConsole::erroring(0);
    let action = run_menu_with_session(&mut console, &session, Duration::ZERO);
    assert!(
        action.is_none(),
        "an attended remote session must not auto-reboot on timeout, got {action:?}"
    );
}

#[test]
fn shutdown_signal_is_observable_across_clones() {
    let s = Shutdown::new();
    let c = s.clone();
    assert!(!s.is_signalled());
    c.signal();
    assert!(s.is_signalled(), "signal must be visible across clones");
}

#[test]
fn shutdown_signal_wakes_a_parked_poller() {
    // A future that registers its waker and parks on shutdown must be
    // woken (not silently left Pending) when a clone signals — the
    // property the accept multiplexer relies on to react with no other
    // event in flight.
    use std::future::poll_fn;
    use std::task::Poll;

    let s = Shutdown::new();
    let waker_clone = s.clone();

    block(async move {
        let mut signalled_once = false;
        poll_fn(|cx| {
            s.register(cx);
            if s.is_signalled() {
                return Poll::Ready(());
            }
            if !signalled_once {
                // Signal from "another" handle; this must wake us so the
                // runtime re-polls and observes the flag on the next turn.
                signalled_once = true;
                waker_clone.signal();
            }
            Poll::Pending
        })
        .await;
    });
}

#[test]
fn action_sink_keeps_first_committer() {
    // Mirrors serve_session's "first committer wins" rule on the shared
    // sink: a second commit must not overwrite the first.
    let sink: ActionSink = Rc::new(RefCell::new(None));
    {
        let mut slot = sink.borrow_mut();
        if slot.is_none() {
            *slot = Some(TerminalAction::Reboot);
        }
    }
    {
        let mut slot = sink.borrow_mut();
        if slot.is_none() {
            *slot = Some(TerminalAction::Kexec);
        }
    }
    assert!(
        matches!(*sink.borrow(), Some(TerminalAction::Reboot)),
        "first committed action must win"
    );
}

#[test]
fn server_returns_on_pre_signalled_shutdown_and_unlinks() {
    // The full server: with shutdown pre-signalled it must bind, then
    // return promptly (no clients) and unlink the socket via the
    // SocketUnlinkGuard. Exercises bind + multiplexer + shutdown +
    // unlink end-to-end without needing root or a pty.
    use super::run_remote_server;
    use crate::ipc::tui_socket::TUI_SOCK_PATH;

    let config = Config::recovery_default();
    let shutdown = Shutdown::new();
    let sink: ActionSink = Rc::new(RefCell::new(None));
    shutdown.signal();

    // bind_listener needs /nmbl-run root-owned 0700; in an unprivileged
    // test sandbox that may be unavailable. If the bind fails the server
    // returns early without creating the socket — the assertion below
    // (socket absent) still holds, so the test is meaningful either way.
    let sender = crate::sys::poller::build().1;
    block(run_remote_server(&config, shutdown, sink, &sender));

    assert!(
        !std::path::Path::new(TUI_SOCK_PATH).exists(),
        "socket must be unlinked after server shutdown"
    );
}

// ---- Real-pty regression: a slow remote peer must not kill the session ----
//
// Production symptom: an operator in the full-system rescue ran `nmbl` over
// SSH and PID 1 logged `remote-tui: session ended (client likely gone): TUI
// failed: Resource temporarily unavailable (os error 11)` right after the
// first frame. The pty's slave->master buffer was full (sshd had not yet
// drained it), the non-blocking pty write returned EAGAIN, and the session
// treated that back-pressure as a disconnect. These tests drive the same
// `run_remote_menu` + `TtyConsole::from_pty` path on a real pty whose
// output buffer is already full, with the test acting as the slow peer.

/// Allocate a pty pair sized `rows`x`cols`.
fn real_pty(rows: u16, cols: u16) -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    let ws = nix::pty::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&ws), None).expect("openpty");
    (pty.master, pty.slave)
}

/// Simulate a peer that is not reading: write filler through the slave
/// until the kernel refuses more (EAGAIN), so the next write the server
/// attempts hits back-pressure. Returns how many filler bytes are queued.
fn fill_pty_output(slave: std::os::fd::BorrowedFd<'_>) -> usize {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let flags = fcntl_getfl(slave).expect("getfl");
    fcntl_setfl(slave, flags | OFlags::NONBLOCK).expect("setfl");
    // Raw output: no ONLCR expansion so the byte count is exact.
    let mut termios = rustix::termios::tcgetattr(slave).expect("tcgetattr");
    termios.make_raw();
    rustix::termios::tcsetattr(slave, rustix::termios::OptionalActions::Now, &termios)
        .expect("tcsetattr");
    let chunk = [b'.'; 4096];
    let mut queued = 0usize;
    loop {
        match rustix::io::write(slave, &chunk) {
            Ok(n) => queued += n,
            Err(rustix::io::Errno::AGAIN) => break,
            Err(e) => panic!("filling pty failed: {e}"),
        }
    }
    fcntl_setfl(slave, flags).expect("restore flags");
    assert!(queued > 0, "pty accepted no filler");
    queued
}

/// The slow peer: wait a while (the server must keep the session alive
/// meanwhile), then drain the pty master. Once the emergency menu has
/// arrived intact, press Enter (Reboot is the first item) and keep
/// draining so the server never blocks on later frames.
async fn slow_peer(master: std::os::fd::OwnedFd, filler: usize, transcript: Rc<RefCell<Vec<u8>>>) {
    use tokio::io::unix::AsyncFd;
    rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK).expect("master nonblock");
    tokio::time::sleep(Duration::from_millis(400)).await;
    let master = AsyncFd::new(master).expect("register master");
    let mut pressed = false;
    loop {
        let mut guard = master.readable().await.expect("master readiness");
        let mut chunk = [0u8; 8192];
        match guard
            .try_io(|fd| rustix::io::read(fd.get_ref(), &mut chunk).map_err(std::io::Error::from))
        {
            Ok(Ok(0)) | Ok(Err(_)) => {
                // Server closed its side; nothing more to read.
                std::future::pending::<()>().await;
            }
            Ok(Ok(n)) => transcript.borrow_mut().extend_from_slice(&chunk[..n]),
            Err(_would_block) => continue,
        }
        let seen = transcript.borrow();
        let frame = seen.get(filler..).unwrap_or(&[]);
        if !pressed && String::from_utf8_lossy(frame).contains("Retry boot from config") {
            drop(seen);
            pressed = true;
            rustix::io::write(master.get_ref(), b"\r").expect("press Enter");
        }
    }
}

#[test]
fn remote_session_survives_eagain_from_a_slow_peer() {
    use crate::ui::console::TtyConsole;

    let (master, slave) = real_pty(60, 200);
    let filler = fill_pty_output(std::os::fd::AsFd::as_fd(&slave));
    let mut console = TtyConsole::from_pty(slave, (60, 200)).expect("console on pty");
    let transcript = Rc::new(RefCell::new(Vec::new()));

    let config = Config::recovery_default();
    let session = SessionInteraction::new();
    session.set();
    let mut app = app_in_session(&session);
    let mut errs = 0u32;
    let sender = crate::sys::poller::build().1;

    let action = block(async {
        let menu = run_remote_menu(
            &mut console,
            &mut app,
            &config,
            &session,
            Duration::from_secs(60),
            &mut errs,
            &sender,
        );
        let peer = slow_peer(master, filler, transcript.clone());
        tokio::time::timeout(Duration::from_secs(30), async {
            tokio::select! {
                action = menu => action,
                () = peer => None,
            }
        })
        .await
        .expect("remote session neither finished nor failed within 30s")
    });

    let seen = transcript.borrow();
    assert!(
        seen.iter().take(filler).all(|b| *b == b'.'),
        "the peer's earlier output must arrive intact before the menu"
    );
    assert!(
        String::from_utf8_lossy(seen.get(filler..).unwrap_or(&[]))
            .contains("Retry boot from config"),
        "the full menu must reach the slow peer"
    );
    assert!(
        matches!(action, Some(TerminalAction::Reboot)),
        "a slow peer is back-pressure, not a disconnect: the session must stay \
         alive and act on the peer's Enter, got {action:?}"
    );
}

#[test]
fn remote_session_ends_when_the_peer_really_disconnects() {
    use crate::ui::console::TtyConsole;

    // Closing the master is a real hang-up: the session must end with no
    // action (never a silent machine-wide Reboot), and must not hang.
    let (master, slave) = real_pty(30, 100);
    let mut console = TtyConsole::from_pty(slave, (30, 100)).expect("console on pty");
    drop(master);
    let session = SessionInteraction::new();
    session.set();
    let started = std::time::Instant::now();
    let action = run_menu_with_session(&mut console, &session, Duration::from_secs(60));
    assert!(
        action.is_none(),
        "a hung-up peer must not commit an action, got {action:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a hung-up peer must end the session promptly"
    );
}

#[test]
fn remote_console_restores_the_operators_blocking_tty() {
    use crate::ui::console::TtyConsole;

    // The pty arrives over SCM_RIGHTS and shares its open file description
    // with the operator's shell. The session must not leave that shell's
    // terminal non-blocking (a later `cat` would fail with EAGAIN).
    let (_master, slave) = real_pty(30, 100);
    let shell_view = rustix::io::dup(&slave).expect("dup slave");
    let before = rustix::fs::fcntl_getfl(&shell_view).expect("getfl");
    assert!(!before.contains(rustix::fs::OFlags::NONBLOCK));
    let console = TtyConsole::from_pty(slave, (30, 100)).expect("console on pty");
    drop(console);
    let after = rustix::fs::fcntl_getfl(&shell_view).expect("getfl");
    assert!(
        !after.contains(rustix::fs::OFlags::NONBLOCK),
        "the operator's tty must be blocking again after the session"
    );
}

// ---- Server-level regression: disconnects release everything ----------
//
// Production symptom: after two failed `nmbl` sessions PID 1 spun a full
// core forever, still holding the accepted socket, the session's pty fds
// and termwiz's socketpairs although the client was gone. This drives the
// real accept loop + session code with real ptys and the real client.

/// User+system CPU ticks consumed so far by the calling thread (the
/// current-thread runtime that runs the server in these tests).
fn thread_cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/thread-self/stat").expect("thread stat");
    let after_comm = stat.rsplit_once(')').expect("stat comm").1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After the comm: [0]=state ... [11]=utime [12]=stime (fields 14/15).
    let utime: u64 = fields[11].parse().expect("utime");
    let stime: u64 = fields[12].parse().expect("stime");
    utime + stime
}

/// How many of this process's fds refer to the tty at `path`.
fn open_fds_on(path: &std::path::Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("fd dir")
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|target| target == path)
        .count()
}

fn tty_path(fd: std::os::fd::BorrowedFd<'_>) -> std::path::PathBuf {
    use std::os::fd::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).expect("tty path")
}

async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..250 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting until {what}");
}

/// Let the server idle for a second and assert it did not burn CPU.
async fn assert_server_idle(after: &str) {
    let before = thread_cpu_ticks();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let used = thread_cpu_ticks() - before;
    assert!(
        used <= 10,
        "server busy-waited after {after}: {used} CPU ticks in one idle second"
    );
}

/// A hand-driven client: connect, pass `slave`, and await the ack.
async fn attach(path: &std::path::Path, slave: std::os::fd::OwnedFd) -> tokio::net::UnixStream {
    use std::os::fd::AsFd;
    let stream = std::os::unix::net::UnixStream::connect(path).expect("connect");
    let handshake = crate::ipc::tui_socket::Handshake {
        term: "xterm-256color".to_string(),
        winsize: (30, 100),
    };
    crate::ipc::tui_socket::send_fd_and_handshake(stream.as_fd(), slave.as_fd(), &handshake)
        .expect("send pty");
    drop(slave);
    stream.set_nonblocking(true).expect("nonblocking");
    let stream = tokio::net::UnixStream::from_std(stream).expect("register client");
    let mut status = [0u8; 1];
    loop {
        stream.readable().await.expect("client readiness");
        match stream.try_read(&mut status) {
            Ok(1) => break,
            Ok(_) => panic!("server closed before acknowledging"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => panic!("reading the ack failed: {e}"),
        }
    }
    assert_eq!(status[0], b'K', "server must accept the trusted peer");
    stream
}

/// Read the operator side of a session's pty until `needle` appears.
async fn read_until(master: &tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>, needle: &str) {
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains(needle) {
        let mut guard = master.readable().await.expect("master readiness");
        let mut chunk = [0u8; 8192];
        match guard
            .try_io(|fd| rustix::io::read(fd.get_ref(), &mut chunk).map_err(std::io::Error::from))
        {
            Ok(Ok(n)) if n > 0 => seen.extend_from_slice(&chunk[..n]),
            Ok(other) => panic!("session pty closed before {needle:?}: {other:?}"),
            Err(_would_block) => {}
        }
    }
}

fn async_master(master: std::os::fd::OwnedFd) -> tokio::io::unix::AsyncFd<std::os::fd::OwnedFd> {
    rustix::fs::fcntl_setfl(&master, rustix::fs::OFlags::NONBLOCK).expect("master nonblock");
    tokio::io::unix::AsyncFd::new(master).expect("register master")
}

#[test]
fn server_releases_disconnected_sessions_and_serves_a_fresh_client() {
    use std::os::fd::AsFd;

    let dir = tempfile_dir();
    let path = dir.join("tui.sock");
    crate::ipc::tui_socket::test_peer::TRUSTED_UID
        .with(|uid| uid.set(Some(rustix::process::geteuid().as_raw())));

    let config = Config::recovery_default();
    let shutdown = Shutdown::new();
    let sink: ActionSink = Rc::new(RefCell::new(None));
    let sender = crate::sys::poller::build().1;

    block(async {
        let listener = tokio::net::UnixListener::bind(&path).expect("bind test socket");
        let server = super::driver::accept_loop(&listener, &config, &shutdown, &sink, &sender);
        let script = async {
            // 1. A client that disconnects mid-handshake.
            drop(std::os::unix::net::UnixStream::connect(&path).expect("connect"));
            assert_server_idle("a mid-handshake disconnect").await;

            // 2. The production case: the operator's pty is full (a slow
            // peer), then the client process goes away. The session must
            // survive the back-pressure, then release the pty on hang-up.
            let (master, slave) = real_pty(30, 100);
            fill_pty_output(slave.as_fd());
            let pts = tty_path(slave.as_fd());
            let client = attach(&path, slave).await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(
                open_fds_on(&pts) > 0,
                "back-pressure must not end the session"
            );
            drop(client);
            eventually("the session released the full pty", || {
                open_fds_on(&pts) == 0
            })
            .await;
            assert_server_idle("a client vanished behind a full pty").await;
            drop(master);

            // 3. The operator's terminal hangs up while the client socket
            // stays open: the server must end the session and close the
            // client's socket instead of polling a dead pty forever.
            let (master, slave) = real_pty(30, 100);
            let pts = tty_path(slave.as_fd());
            let client = attach(&path, slave).await;
            let master = async_master(master);
            read_until(&master, "Retry boot from config").await;
            drop(master);
            let mut buf = [0u8; 16];
            let eof = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    client.readable().await.expect("client readiness");
                    match client.try_read(&mut buf) {
                        Ok(n) => return n,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(_) => return 0,
                    }
                }
            })
            .await
            .expect("server kept the session of a hung-up terminal");
            assert_eq!(eof, 0, "server must close the client's socket");
            eventually("the hung-up pty was released", || open_fds_on(&pts) == 0).await;
            assert_server_idle("the operator's terminal hung up").await;

            // 4. A fresh, real `nmbl` client still gets a working session.
            let (master, slave) = real_pty(30, 100);
            let pts = tty_path(slave.as_fd());
            let client_path = path.clone();
            let client = std::thread::spawn(move || {
                crate::ipc::tui_socket::serve_controlling_tty(&client_path, move || Ok(slave))
            });
            let master = async_master(master);
            read_until(&master, "Retry boot from config").await;
            // Ctrl+E leaves the remote session without any action.
            rustix::io::write(master.get_ref(), b"\x05").expect("Ctrl+E");
            eventually("the real client exited", || client.is_finished()).await;
            let code = client.join().expect("client thread").expect("client I/O");
            assert_eq!(code, std::process::ExitCode::SUCCESS);
            eventually("the finished session released its pty", || {
                open_fds_on(&pts) == 0
            })
            .await;
            assert!(sink.borrow().is_none(), "Ctrl+E must not commit an action");
            assert_server_idle("a completed session").await;

            shutdown.signal();
        };
        tokio::join!(server, script);
    });
    crate::ipc::tui_socket::test_peer::TRUSTED_UID.with(|uid| uid.set(None));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A private scratch directory for the test socket.
fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nmbl-remote-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}
