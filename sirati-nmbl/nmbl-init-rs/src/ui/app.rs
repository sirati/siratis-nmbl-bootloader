//! TUI state machine. Pure logic: takes `crossterm::event::KeyEvent`s
//! and mutates [`App`]; the surrounding `ui::mod` is responsible for
//! actually polling input and rendering frames via [`crate::ui::view`].
//!
//! The state machine has six screens:
//! - [`Screen::List`]    — generation picker, default landing page.
//! - [`Screen::Editing`] — single-line kernel-cmdline editor.
//! - [`Screen::Passphrase`] — modal LUKS prompt driven by activation.rs.
//! - [`Screen::Emergency`] — boot-failed picker between Reboot and Shell.
//! - [`Screen::BootStatus`] — non-interactive progress + log view shown
//!   during early boot phases (before the selector / activation).
//! - [`Screen::KeyEcho`] — diagnostic test screen that echoes every key
//!   event and raw byte sequence to two panels. Inaccessible from
//!   normal boot; only reached when `nmbl.key_echo=1` appears on the
//!   kernel cmdline. Used to debug VNC/PS-2 → splash input plumbing.
//!
//! When the user makes a final decision the `decision` field is set
//! and [`App::on_key`] returns `true`, signalling the run loop to exit.
//! The passphrase modal is the exception: Enter on a passphrase screen
//! leaves the App alive (the caller — the supplier driving
//! [`crate::ui::passphrase_prompt_on_console`] — drains the buffer and
//! returns it without exiting the App), and only Esc on the passphrase
//! modal sets a [`Decision::Shell`] exit.

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use zeroize::Zeroizing;

use crate::generations::Generation;

/// Per-boot-session latch: set the first time the operator presses any
/// key, shared (cloned) across every App of one session so "already
/// attended" spans the selector, LUKS prompt, and emergency screen.
/// Independent between sessions (each remote TUI session gets its own).
#[derive(Clone, Default)]
pub struct SessionInteraction(Rc<Cell<bool>>);
impl SessionInteraction {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn get(&self) -> bool {
        self.0.get()
    }
    pub fn set(&self) {
        self.0.set(true);
    }
}

/// Page size (in rows) for [`Screen::Log`] PageUp/PageDown scrolling.
const LOG_PAGE: u16 = 20;

/// Maximum number of entries retained in each [`Screen::KeyEcho`] ring
/// buffer. Old entries are evicted from the front when full. ~20 keeps
/// the panels readable on an 80×24 console with room for header/footer.
pub const KEY_ECHO_RING_CAP: usize = 20;

/// Top-level user choice returned when the TUI exits.
#[derive(Debug)]
pub enum Decision {
    /// User chose to boot this generation. cmdline may have been
    /// edited in the TUI.
    Boot {
        generation_index: usize,
        cmdline_override: Option<String>,
    },
    /// User asked for the emergency shell.
    Shell,
    /// User asked to reboot the machine (not common but useful).
    Reboot,
}

/// Choice the operator can make on the emergency screen.
///
/// Kept separate from [`Decision`] because the boot-menu Decision
/// machinery is geared around generations + cmdline overrides, which
/// the emergency screen has no business expressing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmergencyChoice {
    /// Reboot the machine via `reboot(RB_AUTOBOOT)`.
    Reboot,
    /// Stay inside NMBL and host an emulated terminal inside the
    /// existing ratatui chrome. Unlike [`EmergencyChoice::RawShell`]
    /// this does NOT `execve(2)` — NMBL keeps PID 1 and pumps bytes
    /// between the operator's keystrokes and a forked shell on a PTY.
    /// When the shell exits, control returns to the emergency screen.
    ///
    /// Gated behind the `pretty-shell` feature because the terminal
    /// emulator reuses `alacritty_terminal`. The feature is default-on
    /// (and also pulled in by `image-splash`); a `--no-default-features`
    /// build has no way to render an ANSI grid and would either need a
    /// private terminal emulator (forbidden by the no-reimplement rule)
    /// or a degraded plain-text mode that defeats the purpose.
    ///
    /// Pretty Shell is the preferred default whenever the feature is
    /// compiled in; it sits at the top of the shell options on the
    /// emergency picker, with the raw busybox-on-tty fallback below it.
    #[cfg(feature = "pretty-shell")]
    PrettyShell,
    /// Drop to the configured emergency shell on a raw tty via the
    /// console picker + multiplexed busybox PTY relay. Kept available
    /// even when `pretty-shell` is on so operators can fall back when
    /// the terminal emulator misbehaves.
    RawShell,
    /// Re-run the full normal boot path (phases 3, 3b, 4, 5) from the
    /// emergency screen. Use case: a transient activation failure
    /// (network mount times out, USB key not seated) that the operator
    /// fixed at the shell and now wants to retry without rebooting.
    /// On success returns a [`crate::terminal::TerminalAction::Kexec`]
    /// (or `Reboot`); on failure shows a modal error and returns the
    /// operator to the emergency screen.
    RetryBoot,
    /// Skip phases 3 and 3b (assume the operator already mounted the
    /// system filesystem manually at the shell) and run only the
    /// generation scan + selector. Confirms with a yes/no modal before
    /// handing off to the boot dispatcher. Use case: kexec readiness
    /// check during disaster recovery, where the disk layout is
    /// non-standard and `run_all_activations` can't replicate it.
    VerifyKexecReadiness,
}

/// One row on the emergency screen.
pub struct EmergencyItem {
    pub label: &'static str,
    pub choice: EmergencyChoice,
}

/// Per-frame snapshot shown by [`Screen::BootStatus`].
///
/// Owned by the App so callers can mutate fields between frames via the
/// `set_*` / `tick_*` helpers on [`App`]; the renderer is purely a
/// consumer of this struct.
pub struct BootStatusData<'a> {
    /// Current phase label, e.g. "phase 3: storage activations" or
    /// "waiting for /dev/disk/by-uuid/X (12s/30s)".
    pub phase: std::borrow::Cow<'a, str>,
    /// Snapshot of the recent log lines (already gathered by caller).
    /// Most recent last; the renderer clips to the visible panel.
    pub log_lines: Vec<String>,
    /// Spinner phase. Caller increments via [`App::tick_boot_spinner`];
    /// renderer maps to a glyph by `spinner_frame % SPINNER_FRAMES`.
    pub spinner_frame: u8,
}

/// Which screen the App is currently presenting.
pub enum Screen<'a> {
    List,
    /// Full boot-transcript viewer, opened with Ctrl+L from any screen
    /// and popped back via Esc / Ctrl+L. `lines` is the snapshot
    /// (oldest first) and `offset` is the scroll position; the renderer
    /// clamps `offset` so over-scroll is harmless.
    Log {
        lines: Vec<String>,
        offset: u16,
    },
    Editing {
        /// Index into the generations slice.
        generation_index: usize,
        /// Working buffer + cursor for the cmdline. Shares its editing
        /// semantics with the passphrase prompt via
        /// [`crate::ui::editline`].
        line: crate::ui::editline::EditableLine,
    },
    Passphrase {
        prompt_label: String,
        buffer: Zeroizing<String>,
        /// Byte index into `buffer`; always lies on a char boundary.
        /// The displayed characters are masked (dots) but the cursor
        /// tracks the real position in the secret so mid-string edits
        /// land where the operator expects.
        cursor: usize,
        /// `true` once the operator has submitted the buffer and the
        /// activation runner is verifying it (cryptsetup is running).
        /// Renderer overlays a spinner so the operator sees the boot
        /// is alive rather than hung. Cleared back to `false` if the
        /// activation reports a wrong-password retry and the prompt
        /// re-opens for another attempt.
        verifying: bool,
        /// Spinner phase for the verifying overlay, cycled via
        /// [`App::tick_passphrase_spinner`]. Indexes
        /// [`SPINNER_GLYPHS`] modulo [`SPINNER_FRAMES`].
        spinner_frame: u8,
    },
    /// Boot has failed. Show the error and let the operator pick
    /// between Reboot and Shell. Defaults are owned by the caller —
    /// the screen just runs the picker.
    Emergency {
        /// Human-readable explanation (already formatted error chain).
        message: String,
        /// Items to display, in order. The screen renders them as a
        /// list and lets the operator pick one.
        items: Vec<EmergencyItem>,
        /// Selected row index, clamped to `items.len() - 1` on render.
        selected: usize,
        /// Final choice the operator committed to; `None` until Enter.
        chosen: Option<EmergencyChoice>,
    },
    /// Non-interactive progress view shown during early boot. The
    /// caller drives the phase label, log snapshot, and spinner tick;
    /// key events are absorbed but never produce a [`Decision`].
    BootStatus(BootStatusData<'a>),
    /// Diagnostic test screen — gated behind `nmbl.key_echo=1` on the
    /// kernel cmdline so it is unreachable in normal boots. Renders
    /// two ring buffers side-by-side: parsed `KeyEvent`s on the left,
    /// raw byte sequences on the right. The driver loop pushes a new
    /// entry per keypress and the renderer in [`crate::ui::view`]
    /// shows the most-recent at the bottom. The loop exits on Ctrl+C.
    KeyEcho {
        /// Human-readable rendering of each parsed `KeyEvent`, most
        /// recent last. Bounded at [`KEY_ECHO_RING_CAP`].
        events: VecDeque<String>,
        /// Raw bytes captured from the underlying input reader,
        /// hex-printed (e.g. `"1b 5b 41"` for arrow-up CSI). Most
        /// recent last. Bounded at [`KEY_ECHO_RING_CAP`].
        byte_log: VecDeque<String>,
    },
}

/// One overlay drawn on top of [`App::screen`].
///
/// The underlying screen keeps rendering behind the modal so the
/// operator can see "where they were" — closing the modal returns to
/// exactly the same screen state (same selection, same scroll). Owned
/// `String` fields keep the App `'static`-friendly so a future caller
/// doesn't have to thread a borrow through the modal lifetime.
#[derive(Debug, Clone)]
pub enum ModalKind {
    /// Two-button yes/no confirmation overlay. Driven by
    /// [`crate::ui::show_modal_confirm`].
    Confirm {
        title: String,
        message: String,
        yes_label: String,
        no_label: String,
        yes_selected: bool,
        hint: String,
    },
    /// Read-only error overlay. Driven by
    /// [`crate::ui::show_modal_error`].
    Error {
        title: String,
        message: String,
        hint: String,
    },
    /// N-button overlay. Driven by
    /// [`crate::ui::show_wrong_password_modal`].
    Buttons {
        title: String,
        message: String,
        labels: Vec<String>,
        selected: usize,
        hint: String,
    },
    /// Animated progress overlay shown by [`crate::ui::BootReporter`]
    /// when an emergency-action wants the menu visible behind. The
    /// terminal-boot path stays on [`Screen::BootStatus`].
    Status {
        phase: String,
        log_lines: Vec<String>,
        spinner_frame: u8,
    },
}

/// Top-level TUI app state.
pub struct App<'a> {
    pub generations: &'a [Generation],
    pub selected_index: usize,
    pub screen: Screen<'a>,
    pub show_kernel_params: bool,
    pub countdown_remaining_secs: Option<u64>,
    pub decision: Option<Decision>,
    /// When `Some`, painted on top of `screen` by the renderer. The
    /// underlying screen keeps rendering behind so closing the modal
    /// returns to the same selection / scroll state.
    pub modal: Option<ModalKind>,
    /// Latch for the emergency-screen auto-reboot countdown. Set on
    /// the FIRST entry to the emergency (error) screen and never reset
    /// — re-entries after dismissing a modal find the deadline already
    /// present so the timer doesn't restart. Once elapsed, the next
    /// visit to the emergency screen reboots immediately.
    pub error_countdown_deadline: Option<Instant>,
    /// Scroll viewport offset for the modal text region. Only
    /// meaningful when the modal layout returns `scrollable = true`
    /// (stage H4 in `modal_layout`). Cleared by every modal-open and
    /// modal-close path so a re-entry never inherits the previous
    /// modal's scroll position. Ctrl+Shift+Up/Down advance by 1;
    /// Ctrl+Shift+PageUp/PageDown advance by visible_lines - 1.
    pub modal_scroll_offset: u16,
    /// Per-boot-session interaction latch shared across every App of a
    /// session. Set on the first keypress; read by the emergency screen
    /// to decide whether to arm the auto-reboot countdown.
    pub interaction: SessionInteraction,
    /// Set when the operator presses Ctrl+E asking to leave the current
    /// (remote) session. Local run loops ignore it today; a future
    /// phase makes the remote loops honour it.
    pub exit_session: bool,
    /// Screen stashed while the log viewer ([`Screen::Log`]) is open, so
    /// Esc / Ctrl+L can pop back to exactly where the operator was.
    pub return_screen: Option<Box<Screen<'a>>>,
    /// Live Caps-Lock state, polled each render tick by the passphrase
    /// prompt loop. `true` paints the (reserved, non-resizing) warning
    /// row on the passphrase modal; `false` leaves it blank. Only the
    /// passphrase screen reads it. Defaults to `false` (off / unknown)
    /// so backends that can't query the keyboard LED — serial lines,
    /// the mock harness — simply never show the warning.
    pub caps_lock_warning: bool,
}

/// Number of frames in the boot-status spinner cycle.
///
/// We deliberately use the 4-frame ASCII rotor `|/-\` rather than the
/// 10-frame braille systemd uses. The splash glyph cache (see
/// `src/splash/glyph_cache.rs`) only rasterises ASCII printable plus
/// the box-drawing subset ratatui uses for borders; Unicode braille
/// (U+2800 block) is not in the cache, so `cache.get(c, _)` would
/// return `None` and the splash compositor would draw nothing. On a
/// crossterm terminal the braille would render fine, but the boot
/// screen needs to look identical on both backends — pick ASCII for
/// guaranteed coverage.
pub const SPINNER_FRAMES: u8 = 4;

/// The ASCII spinner glyph sequence. Indexed by `spinner_frame % SPINNER_FRAMES`.
pub const SPINNER_GLYPHS: [char; SPINNER_FRAMES as usize] = ['|', '/', '-', '\\'];

impl<'a> App<'a> {
    pub fn new(generations: &'a [Generation]) -> Self {
        Self {
            generations,
            selected_index: 0,
            screen: Screen::List,
            show_kernel_params: false,
            countdown_remaining_secs: None,
            decision: None,
            modal: None,
            error_countdown_deadline: None,
            modal_scroll_offset: 0,
            interaction: SessionInteraction::new(),
            exit_session: false,
            return_screen: None,
            caps_lock_warning: false,
        }
    }

    /// Same as [`App::new`] but joins an existing session so the
    /// interaction latch is shared with the other Apps of this boot.
    pub fn new_in_session(generations: &'a [Generation], session: &SessionInteraction) -> Self {
        let mut app = Self::new(generations);
        app.interaction = session.clone();
        app
    }

    /// Construct an App parked on the [`Screen::BootStatus`] view with
    /// the given phase label, an empty log buffer, and spinner_frame=0.
    ///
    /// `generations` is empty because the boot-status screen runs
    /// before the selector has anything to show. A future caller can
    /// transition out of the boot-status screen by replacing
    /// `self.screen` directly.
    pub fn boot_status(phase: impl Into<std::borrow::Cow<'a, str>>) -> App<'a> {
        App {
            generations: &[],
            selected_index: 0,
            screen: Screen::BootStatus(BootStatusData {
                phase: phase.into(),
                log_lines: Vec::new(),
                spinner_frame: 0,
            }),
            show_kernel_params: false,
            countdown_remaining_secs: None,
            decision: None,
            modal: None,
            error_countdown_deadline: None,
            modal_scroll_offset: 0,
            interaction: SessionInteraction::new(),
            exit_session: false,
            return_screen: None,
            caps_lock_warning: false,
        }
    }

    /// Construct an App parked on [`Screen::KeyEcho`] with empty ring
    /// buffers. The diagnostic loop in [`crate::ui::key_echo`] drives
    /// further mutations via [`App::push_key_echo_event`] and
    /// [`App::push_key_echo_bytes`].
    pub fn key_echo() -> App<'a> {
        App {
            generations: &[],
            selected_index: 0,
            screen: Screen::KeyEcho {
                events: VecDeque::new(),
                byte_log: VecDeque::new(),
            },
            show_kernel_params: false,
            countdown_remaining_secs: None,
            decision: None,
            modal: None,
            error_countdown_deadline: None,
            modal_scroll_offset: 0,
            interaction: SessionInteraction::new(),
            exit_session: false,
            return_screen: None,
            caps_lock_warning: false,
        }
    }

    /// Scroll the modal text viewport up by `n` rows (towards the top
    /// of the buffer). Saturates at 0.
    pub fn modal_scroll_up(&mut self, n: u16) {
        self.modal_scroll_offset = self.modal_scroll_offset.saturating_sub(n);
    }

    /// Scroll the modal text viewport down by `n` rows, clamped at
    /// `total - visible`. Saturates so the last visible row never
    /// scrolls past the buffer's last row.
    pub fn modal_scroll_down(&mut self, n: u16, total: u16, visible: u16) {
        let max_off = total.saturating_sub(visible);
        let new_off = self.modal_scroll_offset.saturating_add(n);
        self.modal_scroll_offset = new_off.min(max_off);
    }

    /// Reset the modal scroll offset to 0. Called every modal open/close
    /// path so a re-entry never inherits the previous modal's offset.
    pub fn modal_scroll_reset(&mut self) {
        self.modal_scroll_offset = 0;
    }

    /// Latch the auto-reboot deadline for the error (emergency) screen.
    ///
    /// Sets `error_countdown_deadline` only when it is currently
    /// `None` — re-entries (after dismissing a modal and returning to
    /// the error screen) find the deadline already present so the
    /// timer never restarts. If the deadline already elapsed during
    /// time spent on another screen, the next visit will observe
    /// `now >= deadline` and the loop driver reboots immediately.
    pub fn latch_error_countdown(&mut self, auto_reboot_in: std::time::Duration) {
        if self.error_countdown_deadline.is_none() {
            let now = Instant::now();
            self.error_countdown_deadline = Some(now.checked_add(auto_reboot_in).unwrap_or(now));
        }
    }

    /// Replace the error text shown on the emergency screen so the
    /// operator always sees the *latest* failure, not the first one
    /// the session ever hit. Called whenever a new error is surfaced
    /// (an emergency action failing, a re-entry after a sub-flow) so
    /// the menu's "error" box tracks the most recent diagnostic
    /// instead of latching the original boot error forever. No-op when
    /// the App is on any other screen.
    pub fn set_emergency_message(&mut self, new_message: impl Into<String>) {
        if let Screen::Emergency { message, .. } = &mut self.screen {
            *message = new_message.into();
        } else {
            debug_assert!(
                false,
                "set_emergency_message called on non-Emergency screen"
            );
        }
    }

    /// Append a human-readable parsed-event string to the key-echo
    /// events ring, evicting the oldest entry when full. No-op when
    /// the App is on any other screen.
    pub fn push_key_echo_event(&mut self, line: impl Into<String>) {
        if let Screen::KeyEcho { events, .. } = &mut self.screen {
            if events.len() >= KEY_ECHO_RING_CAP {
                events.pop_front();
            }
            events.push_back(line.into());
        } else {
            debug_assert!(false, "push_key_echo_event called on non-KeyEcho screen");
        }
    }

    /// Append a hex-printed raw-byte string to the key-echo byte-log
    /// ring, evicting the oldest entry when full. No-op when the App
    /// is on any other screen.
    pub fn push_key_echo_bytes(&mut self, line: impl Into<String>) {
        if let Screen::KeyEcho { byte_log, .. } = &mut self.screen {
            if byte_log.len() >= KEY_ECHO_RING_CAP {
                byte_log.pop_front();
            }
            byte_log.push_back(line.into());
        } else {
            debug_assert!(false, "push_key_echo_bytes called on non-KeyEcho screen");
        }
    }

    /// Replace the phase label of the boot-status screen. No-op when
    /// the App is on any other screen so a stray phase update from a
    /// late-firing supervisor task can't crash production.
    pub fn set_boot_phase(&mut self, phase: impl Into<std::borrow::Cow<'a, str>>) {
        if let Screen::BootStatus(data) = &mut self.screen {
            data.phase = phase.into();
        } else {
            debug_assert!(false, "set_boot_phase called on non-BootStatus screen");
        }
    }

    /// Replace the log-line snapshot. The caller (typically holding a
    /// log-ring snapshot via `crate::log::snapshot`) is responsible for
    /// ordering: most recent last.
    pub fn set_boot_log_lines(&mut self, lines: Vec<String>) {
        if let Screen::BootStatus(data) = &mut self.screen {
            data.log_lines = lines;
        } else {
            debug_assert!(false, "set_boot_log_lines called on non-BootStatus screen");
        }
    }

    /// Advance the spinner one frame. Wraps modulo [`SPINNER_FRAMES`]
    /// so callers can tick on any interval without checking the count.
    pub fn tick_boot_spinner(&mut self) {
        if let Screen::BootStatus(data) = &mut self.screen {
            data.spinner_frame = data.spinner_frame.wrapping_add(1) % SPINNER_FRAMES;
        } else {
            debug_assert!(false, "tick_boot_spinner called on non-BootStatus screen");
        }
    }

    /// Flip the passphrase modal into "verifying" mode (cryptsetup is
    /// running). The renderer paints a spinner overlay so the operator
    /// sees the boot is alive — closes the visual gap between Enter and
    /// the LUKS-unlock result. No-op when the App is on another screen.
    ///
    /// Setting `verifying = false` also resets `spinner_frame` to 0 so
    /// a subsequent re-verify starts from a known phase rather than
    /// inheriting the last frame from the previous attempt.
    pub fn set_passphrase_verifying(&mut self, verifying: bool) {
        if let Screen::Passphrase {
            verifying: v,
            spinner_frame,
            ..
        } = &mut self.screen
        {
            *v = verifying;
            if !verifying {
                *spinner_frame = 0;
            }
        } else {
            debug_assert!(
                false,
                "set_passphrase_verifying called on non-Passphrase screen"
            );
        }
    }

    /// Advance the passphrase verifying-spinner one frame. Wraps modulo
    /// [`SPINNER_FRAMES`]. No-op when the App is on another screen.
    pub fn tick_passphrase_spinner(&mut self) {
        if let Screen::Passphrase { spinner_frame, .. } = &mut self.screen {
            *spinner_frame = spinner_frame.wrapping_add(1) % SPINNER_FRAMES;
        } else {
            debug_assert!(
                false,
                "tick_passphrase_spinner called on non-Passphrase screen"
            );
        }
    }

    /// Clear the passphrase buffer (zeroizing it) and reset spinner /
    /// verifying flags. Used by the wrong-password retry path so a
    /// re-prompt starts from a clean slate. No-op when the App is on
    /// another screen.
    pub fn clear_passphrase_buffer(&mut self) {
        if let Screen::Passphrase {
            buffer,
            cursor,
            verifying,
            spinner_frame,
            ..
        } = &mut self.screen
        {
            buffer.clear();
            *cursor = 0;
            *verifying = false;
            *spinner_frame = 0;
        } else {
            debug_assert!(
                false,
                "clear_passphrase_buffer called on non-Passphrase screen"
            );
        }
    }

    /// Reduce a crossterm KeyEvent into a state mutation. Returns
    /// `true` if the App wants to exit (decision is Some).
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        // Ignore Release/Repeat so a held key doesn't fire repeatedly
        // and a key-up after the decisive Press doesn't re-trigger.
        if key.kind != KeyEventKind::Press {
            return self.decision.is_some();
        }

        // Record that the operator is present. Per-session latch
        // consulted by the emergency screen to suppress the auto-reboot
        // countdown once any key has been pressed.
        self.interaction.set();

        // Any keypress cancels the countdown — even one we ignore later.
        self.countdown_remaining_secs = None;

        // Global Ctrl shortcuts, handled before the per-screen dispatch
        // so they work from every screen. Plain `e` / `l` keep their
        // per-screen meanings because these require the CONTROL modifier.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('e') => {
                    // Ask to leave this (remote) session. Local loops
                    // ignore the flag today; just record it.
                    self.exit_session = true;
                    return false;
                }
                KeyCode::Char('l') => {
                    if matches!(self.screen, Screen::Log { .. }) {
                        // Toggle closed: pop back to the stashed screen.
                        if let Some(prev) = self.return_screen.take() {
                            self.screen = *prev;
                        }
                    } else {
                        // Stash the current screen and open the log viewer.
                        self.return_screen = Some(Box::new(std::mem::replace(
                            &mut self.screen,
                            Screen::Log {
                                lines: crate::log::snapshot_full(),
                                offset: 0,
                            },
                        )));
                    }
                    return false;
                }
                _ => {}
            }
        }

        match &mut self.screen {
            Screen::Log { offset, .. } => {
                // Esc closes the viewer (Ctrl+L is handled above). Other
                // keys scroll; the renderer clamps the offset so
                // over-scroll here is harmless. No Decision is produced.
                match key.code {
                    KeyCode::Esc => {
                        if let Some(prev) = self.return_screen.take() {
                            self.screen = *prev;
                        }
                    }
                    KeyCode::Up => *offset = offset.saturating_sub(1),
                    KeyCode::Down => *offset = offset.saturating_add(1),
                    KeyCode::PageUp => *offset = offset.saturating_sub(LOG_PAGE),
                    KeyCode::PageDown => *offset = offset.saturating_add(LOG_PAGE),
                    KeyCode::Home => *offset = 0,
                    KeyCode::End => *offset = u16::MAX,
                    _ => {}
                }
                false
            }
            Screen::List => Self::handle_list_key(
                key.code,
                &mut self.selected_index,
                self.generations,
                &mut self.screen,
                &mut self.show_kernel_params,
                &mut self.decision,
            ),
            Screen::Editing { .. } => {
                Self::handle_editing_key(key, &mut self.screen, &mut self.decision)
            }
            Screen::Passphrase { .. } => {
                Self::handle_passphrase_key(key, &mut self.screen, &mut self.decision)
            }
            Screen::Emergency { .. } => Self::handle_emergency_key(key.code, &mut self.screen),
            // BootStatus absorbs keypresses without producing a Decision.
            // The boot-status screen is non-interactive: it shows progress
            // until the caller flips the App to a different screen.
            Screen::BootStatus(_) => false,
            // KeyEcho is driven directly by the diagnostic loop in
            // `crate::ui::key_echo`, which appends to the ring buffers
            // *before* invoking `on_key` for any state mutations. We
            // intentionally never produce a [`Decision`] from this
            // screen: the loop exits on Ctrl+C / Ctrl+Esc detected at
            // the loop level, not via `Decision`.
            Screen::KeyEcho { .. } => false,
        }
    }

    fn handle_emergency_key(code: KeyCode, screen: &mut Screen) -> bool {
        let Screen::Emergency {
            items,
            selected,
            chosen,
            ..
        } = screen
        else {
            return false;
        };

        let last_idx = items.len().saturating_sub(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected = selected.saturating_sub(1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected < last_idx {
                    *selected = selected.saturating_add(1);
                }
                false
            }
            KeyCode::Enter => {
                if let Some(item) = items.get(*selected) {
                    *chosen = Some(item.choice);
                    true
                } else {
                    false
                }
            }
            // Hotkeys: 'r' for reboot, 'p' for Pretty Shell (when
            // compiled in), 's' for the raw shell, 't' for reTry boot,
            // 'v' for Verify kexec readiness. Operators in a boot-
            // failure scenario tend to be muscle-memory typing one of
            // these letters; we commit straight away on the first key.
            KeyCode::Char('r') => {
                *chosen = Some(EmergencyChoice::Reboot);
                true
            }
            #[cfg(feature = "pretty-shell")]
            KeyCode::Char('p') => {
                *chosen = Some(EmergencyChoice::PrettyShell);
                true
            }
            KeyCode::Char('s') => {
                *chosen = Some(EmergencyChoice::RawShell);
                true
            }
            KeyCode::Char('t') => {
                *chosen = Some(EmergencyChoice::RetryBoot);
                true
            }
            KeyCode::Char('v') => {
                *chosen = Some(EmergencyChoice::VerifyKexecReadiness);
                true
            }
            KeyCode::Esc => {
                // Esc is a no-op: it preserves the prior selection so a
                // stray keypress doesn't commit. The caller can decide
                // separately to fall through to the default on timeout.
                false
            }
            _ => false,
        }
    }

    fn handle_list_key(
        code: KeyCode,
        selected_index: &mut usize,
        generations: &[Generation],
        screen: &mut Screen,
        show_kernel_params: &mut bool,
        decision: &mut Option<Decision>,
    ) -> bool {
        let last_idx = generations.len().saturating_sub(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                *selected_index = selected_index.saturating_sub(1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if *selected_index < last_idx {
                    *selected_index = selected_index.saturating_add(1);
                }
                false
            }
            KeyCode::Enter => {
                // Guard against an empty list: emitting a Boot
                // decision with index 0 would crash the caller as
                // soon as it tried to look up the generation.
                if generations.is_empty() {
                    return false;
                }
                *decision = Some(Decision::Boot {
                    generation_index: *selected_index,
                    cmdline_override: None,
                });
                true
            }
            KeyCode::Char('e') => {
                let buffer = generations
                    .get(*selected_index)
                    .map(|g| g.kernel_params.join(" "))
                    .unwrap_or_default();
                *screen = Screen::Editing {
                    generation_index: *selected_index,
                    line: crate::ui::editline::EditableLine::with_text(buffer),
                };
                false
            }
            KeyCode::Char('p') => {
                *show_kernel_params = !*show_kernel_params;
                false
            }
            KeyCode::Char('s') => {
                *decision = Some(Decision::Shell);
                true
            }
            KeyCode::Char('q') => {
                *decision = Some(Decision::Reboot);
                true
            }
            _ => false,
        }
    }

    fn handle_editing_key(
        key: KeyEvent,
        screen: &mut Screen,
        decision: &mut Option<Decision>,
    ) -> bool {
        let Screen::Editing {
            generation_index,
            line,
        } = screen
        else {
            return false;
        };

        // Enter / Esc are owned by the editor screen, not the line.
        match key.code {
            KeyCode::Enter => {
                *decision = Some(Decision::Boot {
                    generation_index: *generation_index,
                    cmdline_override: Some(line.text().to_owned()),
                });
                return true;
            }
            KeyCode::Esc => {
                *screen = Screen::List;
                return false;
            }
            _ => {}
        }
        // Everything else (insert, Backspace/Delete, cursor motion,
        // Ctrl+A/E/D, word-wise motion) goes through the shared
        // editable-line helper so the cmdline editor and the passphrase
        // prompt behave identically.
        line.handle_key(key);
        false
    }

    fn handle_passphrase_key(
        key: KeyEvent,
        screen: &mut Screen,
        decision: &mut Option<Decision>,
    ) -> bool {
        let Screen::Passphrase { buffer, cursor, .. } = screen else {
            return false;
        };

        match key.code {
            KeyCode::Enter => {
                // Caller (the passphrase prompt loop) detects the buffer
                // is ready by polling — we do NOT exit the App here.
                // Signal "consumed" with `true` so the supplier's
                // dispatch loop can return cleanly.
                true
            }
            KeyCode::Esc => {
                *decision = Some(Decision::Shell);
                true
            }
            _ => {
                // Drive the same shared editable-line logic as the
                // cmdline editor. The secret stays in the Zeroizing
                // buffer (which derefs to &mut String); only the
                // renderer masks it. The cursor tracks the real index.
                let (new_cursor, _handled) =
                    crate::ui::editline::handle_key_on(buffer, *cursor, key);
                *cursor = new_cursor;
                false
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert with panics on contract failure"
)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use std::path::PathBuf;

    fn fake_gen(number: u32, params: &[&str]) -> Generation {
        Generation {
            number,
            profile_link: PathBuf::from(format!("/p/system-{number}-link")),
            kernel: PathBuf::from("/p/kernel"),
            initrd: PathBuf::from("/p/initrd"),
            init_path: PathBuf::from(format!("/p/system-{number}-link/init")),
            kernel_params: params.iter().map(|s| (*s).to_string()).collect(),
            label: String::new(),
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn list_arrow_keys_move_selection_within_bounds() {
        let gens = vec![fake_gen(3, &[]), fake_gen(2, &[]), fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert_eq!(app.selected_index, 0);

        // Up at index 0 stays at 0.
        assert!(!app.on_key(press(KeyCode::Up)));
        assert_eq!(app.selected_index, 0);

        // Down moves through the list.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(app.selected_index, 1);
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(app.selected_index, 2);

        // Down at end stays at end.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(app.selected_index, 2);

        // vi-keys also work.
        assert!(!app.on_key(press(KeyCode::Char('k'))));
        assert_eq!(app.selected_index, 1);
        assert!(!app.on_key(press(KeyCode::Char('j'))));
        assert_eq!(app.selected_index, 2);
    }

    #[test]
    fn list_e_transitions_to_editing_with_joined_params() {
        let gens = vec![fake_gen(42, &["init=/sbin/init", "quiet", "loglevel=4"])];
        let mut app = App::new(&gens);

        assert!(!app.on_key(press(KeyCode::Char('e'))));
        match &app.screen {
            Screen::Editing {
                generation_index,
                line,
            } => {
                assert_eq!(*generation_index, 0);
                assert_eq!(line.text(), "init=/sbin/init quiet loglevel=4");
                assert_eq!(line.cursor(), line.text().len(), "cursor must land at end");
            }
            _ => panic!("expected Editing screen"),
        }
    }

    #[test]
    fn list_s_sets_shell_decision_and_returns_true() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert!(app.on_key(press(KeyCode::Char('s'))));
        assert!(matches!(app.decision, Some(Decision::Shell)));
    }

    #[test]
    fn list_q_sets_reboot_decision_and_returns_true() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert!(app.on_key(press(KeyCode::Char('q'))));
        assert!(matches!(app.decision, Some(Decision::Reboot)));
    }

    #[test]
    fn list_enter_sets_boot_decision_with_no_override() {
        let gens = vec![fake_gen(7, &[]), fake_gen(6, &[])];
        let mut app = App::new(&gens);
        app.selected_index = 1;
        assert!(app.on_key(press(KeyCode::Enter)));
        match &app.decision {
            Some(Decision::Boot {
                generation_index,
                cmdline_override,
            }) => {
                assert_eq!(*generation_index, 1);
                assert!(cmdline_override.is_none());
            }
            other => panic!("expected Boot decision, got {other:?}"),
        }
    }

    #[test]
    fn list_enter_with_empty_generations_does_not_decide() {
        // Defence-in-depth: if the selector ever ran with zero
        // generations, Enter would otherwise emit Boot{0,..} and
        // main.rs would index out of bounds. Make Enter a no-op.
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        assert!(!app.on_key(press(KeyCode::Enter)));
        assert!(app.decision.is_none(), "decision must stay None");
    }

    #[test]
    fn list_p_toggles_show_kernel_params() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert!(!app.show_kernel_params);
        app.on_key(press(KeyCode::Char('p')));
        assert!(app.show_kernel_params);
        app.on_key(press(KeyCode::Char('p')));
        assert!(!app.show_kernel_params);
    }

    #[test]
    fn any_keypress_in_list_cancels_countdown() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        app.countdown_remaining_secs = Some(4);
        // 'p' is a no-op-ish toggle, but should still clear the countdown.
        app.on_key(press(KeyCode::Char('p')));
        assert!(app.countdown_remaining_secs.is_none());
    }

    #[test]
    fn any_keypress_sets_user_interacted_latch() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert!(!app.interaction.get());
        app.on_key(press(KeyCode::Char('p')));
        assert!(app.interaction.get());
    }

    #[test]
    fn editing_typing_appends_and_backspace_removes() {
        let gens = vec![fake_gen(1, &["foo"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));

        // Append " bar".
        for c in " bar".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        match &app.screen {
            Screen::Editing { line, .. } => {
                assert_eq!(line.text(), "foo bar");
                assert_eq!(line.cursor(), line.text().len());
            }
            _ => panic!("expected Editing"),
        }

        // Backspace once removes 'r'.
        app.on_key(press(KeyCode::Backspace));
        match &app.screen {
            Screen::Editing { line, .. } => assert_eq!(line.text(), "foo ba"),
            _ => panic!("expected Editing"),
        }
    }

    #[test]
    fn editing_enter_sets_boot_with_cmdline_override() {
        let gens = vec![fake_gen(5, &["root=/dev/sda1"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));
        for c in " quiet".chars() {
            app.on_key(press(KeyCode::Char(c)));
        }
        assert!(app.on_key(press(KeyCode::Enter)));
        match &app.decision {
            Some(Decision::Boot {
                generation_index,
                cmdline_override,
            }) => {
                assert_eq!(*generation_index, 0);
                assert_eq!(cmdline_override.as_deref(), Some("root=/dev/sda1 quiet"));
            }
            other => panic!("expected Boot{{..}}, got {other:?}"),
        }
    }

    #[test]
    fn editing_esc_returns_to_list_without_decision() {
        let gens = vec![fake_gen(5, &["foo"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));
        assert!(matches!(app.screen, Screen::Editing { .. }));
        assert!(!app.on_key(press(KeyCode::Esc)));
        assert!(matches!(app.screen, Screen::List));
        assert!(app.decision.is_none());
    }

    #[test]
    fn editing_home_end_left_right_navigation() {
        let gens = vec![fake_gen(1, &["abcd"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));

        // Cursor starts at end. Home jumps to 0.
        app.on_key(press(KeyCode::Home));
        match &app.screen {
            Screen::Editing { line, .. } => assert_eq!(line.cursor(), 0),
            _ => panic!(),
        }
        // Right advances one byte.
        app.on_key(press(KeyCode::Right));
        match &app.screen {
            Screen::Editing { line, .. } => assert_eq!(line.cursor(), 1),
            _ => panic!(),
        }
        // End jumps to the end.
        app.on_key(press(KeyCode::End));
        match &app.screen {
            Screen::Editing { line, .. } => assert_eq!(line.cursor(), line.text().len()),
            _ => panic!(),
        }
        // Left walks back one byte.
        app.on_key(press(KeyCode::Left));
        match &app.screen {
            Screen::Editing { line, .. } => {
                assert_eq!(line.cursor(), line.text().len().saturating_sub(1));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn editing_handles_multibyte_backspace_without_panic() {
        // Backspacing across a multi-byte char boundary must not panic
        // even though clippy's indexing_slicing lint applies to prod code.
        let gens = vec![fake_gen(1, &["héllo"])];
        let mut app = App::new(&gens);
        app.on_key(press(KeyCode::Char('e')));
        app.on_key(press(KeyCode::Backspace));
        match &app.screen {
            Screen::Editing { line, .. } => {
                assert_eq!(line.text(), "héll");
                assert_eq!(line.cursor(), line.text().len());
            }
            _ => panic!("expected Editing"),
        }
    }

    #[test]
    fn passphrase_screen_collects_chars_and_pops() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.screen = Screen::Passphrase {
            prompt_label: "Unlock".to_string(),
            buffer: Zeroizing::new(String::new()),
            cursor: 0,
            verifying: false,
            spinner_frame: 0,
        };
        for c in "hi".chars() {
            assert!(!app.on_key(press(KeyCode::Char(c))));
        }
        match &app.screen {
            Screen::Passphrase { buffer, .. } => assert_eq!(&**buffer, "hi"),
            _ => panic!(),
        }
        app.on_key(press(KeyCode::Backspace));
        match &app.screen {
            Screen::Passphrase { buffer, .. } => assert_eq!(&**buffer, "h"),
            _ => panic!(),
        }
    }

    #[test]
    fn passphrase_esc_drops_to_shell() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.screen = Screen::Passphrase {
            prompt_label: "Unlock".to_string(),
            buffer: Zeroizing::new(String::new()),
            cursor: 0,
            verifying: false,
            spinner_frame: 0,
        };
        assert!(app.on_key(press(KeyCode::Esc)));
        assert!(matches!(app.decision, Some(Decision::Shell)));
    }

    #[test]
    fn passphrase_enter_signals_consumed_without_decision() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.screen = Screen::Passphrase {
            prompt_label: "Unlock".to_string(),
            buffer: Zeroizing::new("secret".to_string()),
            cursor: 0,
            verifying: false,
            spinner_frame: 0,
        };
        assert!(app.on_key(press(KeyCode::Enter)));
        assert!(app.decision.is_none(), "Enter must not set a Decision");
    }

    #[test]
    fn passphrase_set_verifying_toggles_flag_and_resets_spinner_on_clear() {
        // The verifying flag drives the overlay; clearing it must also
        // reset the spinner frame so a re-verify starts from glyph 0.
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.screen = Screen::Passphrase {
            prompt_label: "Unlock".to_string(),
            buffer: Zeroizing::new(String::new()),
            cursor: 0,
            verifying: false,
            spinner_frame: 0,
        };
        app.set_passphrase_verifying(true);
        app.tick_passphrase_spinner();
        app.tick_passphrase_spinner();
        match &app.screen {
            Screen::Passphrase {
                verifying,
                spinner_frame,
                ..
            } => {
                assert!(*verifying, "verifying must be set");
                assert_eq!(*spinner_frame, 2, "two ticks land on frame 2");
            }
            _ => panic!("expected Passphrase"),
        }
        app.set_passphrase_verifying(false);
        match &app.screen {
            Screen::Passphrase {
                verifying,
                spinner_frame,
                ..
            } => {
                assert!(!*verifying, "verifying cleared");
                assert_eq!(*spinner_frame, 0, "spinner reset on clear");
            }
            _ => panic!("expected Passphrase"),
        }
    }

    #[test]
    fn passphrase_tick_spinner_wraps_modulo_frame_count() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.screen = Screen::Passphrase {
            prompt_label: "Unlock".to_string(),
            buffer: Zeroizing::new(String::new()),
            cursor: 0,
            verifying: true,
            spinner_frame: 0,
        };
        for _ in 0..SPINNER_FRAMES {
            app.tick_passphrase_spinner();
        }
        match &app.screen {
            Screen::Passphrase { spinner_frame, .. } => {
                assert_eq!(*spinner_frame, 0, "SPINNER_FRAMES ticks wrap to 0");
            }
            _ => panic!("expected Passphrase"),
        }
    }

    #[test]
    fn passphrase_clear_buffer_resets_state() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.screen = Screen::Passphrase {
            prompt_label: "Unlock".to_string(),
            buffer: Zeroizing::new("typed".to_string()),
            cursor: 0,
            verifying: true,
            spinner_frame: 3,
        };
        app.clear_passphrase_buffer();
        match &app.screen {
            Screen::Passphrase {
                buffer,
                verifying,
                spinner_frame,
                ..
            } => {
                assert!(buffer.is_empty());
                assert!(!*verifying);
                assert_eq!(*spinner_frame, 0);
            }
            _ => panic!("expected Passphrase"),
        }
    }

    fn emergency_app() -> App<'static> {
        let mut app = App::new(&[]);
        app.screen = Screen::Emergency {
            message: "boot failed: test".to_string(),
            items: vec![
                EmergencyItem {
                    label: "Reboot",
                    choice: EmergencyChoice::Reboot,
                },
                EmergencyItem {
                    label: "Raw Shell",
                    choice: EmergencyChoice::RawShell,
                },
            ],
            selected: 0,
            chosen: None,
        };
        app
    }

    fn emergency_state(app: &App<'_>) -> (usize, Option<EmergencyChoice>) {
        match &app.screen {
            Screen::Emergency {
                selected, chosen, ..
            } => (*selected, *chosen),
            _ => panic!("expected Emergency screen"),
        }
    }

    #[test]
    fn emergency_arrow_keys_move_selection_within_bounds() {
        let mut app = emergency_app();
        assert_eq!(emergency_state(&app).0, 0);

        // Up at index 0 stays at 0.
        assert!(!app.on_key(press(KeyCode::Up)));
        assert_eq!(emergency_state(&app).0, 0);

        // Down advances.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(emergency_state(&app).0, 1);

        // Down at end stays at end.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(emergency_state(&app).0, 1);

        // Up walks back.
        assert!(!app.on_key(press(KeyCode::Up)));
        assert_eq!(emergency_state(&app).0, 0);

        // vi-keys also work.
        assert!(!app.on_key(press(KeyCode::Char('j'))));
        assert_eq!(emergency_state(&app).0, 1);
        assert!(!app.on_key(press(KeyCode::Char('k'))));
        assert_eq!(emergency_state(&app).0, 0);
    }

    #[test]
    fn emergency_enter_returns_selected_variant() {
        // selected=0 -> Reboot.
        let mut app = emergency_app();
        assert!(app.on_key(press(KeyCode::Enter)));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::Reboot));

        // selected=1 -> RawShell.
        let mut app = emergency_app();
        assert!(!app.on_key(press(KeyCode::Down)));
        assert!(app.on_key(press(KeyCode::Enter)));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::RawShell));
    }

    #[test]
    fn set_emergency_message_replaces_displayed_error() {
        // Regression: the emergency screen used to latch the first
        // error it was built with. set_emergency_message must overwrite
        // the displayed text so the operator always sees the LATEST
        // failure (e.g. a failed Raw Shell), not the original boot one.
        let mut app = emergency_app();
        match &app.screen {
            Screen::Emergency { message, .. } => {
                assert_eq!(message, "boot failed: test");
            }
            _ => panic!("expected Emergency screen"),
        }

        app.set_emergency_message("Latest error (#1): Raw Shell failed\n\nEACCES");
        match &app.screen {
            Screen::Emergency { message, .. } => {
                assert!(
                    message.contains("Latest error (#1)"),
                    "message not updated: {message}"
                );
                assert!(
                    message.contains("Raw Shell failed"),
                    "missing title: {message}"
                );
                assert!(
                    !message.contains("boot failed: test"),
                    "stale first error retained"
                );
            }
            _ => panic!("expected Emergency screen"),
        }

        // A second update wins again (most-recent-wins, no latch).
        app.set_emergency_message("Latest error (#2): Retry failed\n\nENOENT");
        match &app.screen {
            Screen::Emergency { message, .. } => {
                assert!(
                    message.contains("Latest error (#2)"),
                    "second update lost: {message}"
                );
                assert!(!message.contains("(#1)"), "stale #1 retained: {message}");
            }
            _ => panic!("expected Emergency screen"),
        }

        // Selection / items are untouched by a message-only update.
        assert_eq!(emergency_state(&app).0, 0);
    }

    #[test]
    fn emergency_esc_preserves_selection_without_committing() {
        let mut app = emergency_app();
        // Move to Shell.
        assert!(!app.on_key(press(KeyCode::Down)));
        assert_eq!(emergency_state(&app).0, 1);

        // Esc must not commit and must not move.
        assert!(!app.on_key(press(KeyCode::Esc)));
        let (sel, chosen) = emergency_state(&app);
        assert_eq!(sel, 1, "selection must be preserved across Esc");
        assert!(chosen.is_none(), "Esc must not commit a choice");
    }

    #[test]
    fn emergency_hotkeys_r_and_s_commit_directly() {
        let mut app = emergency_app();
        assert!(app.on_key(press(KeyCode::Char('r'))));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::Reboot));

        let mut app = emergency_app();
        assert!(app.on_key(press(KeyCode::Char('s'))));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::RawShell));
    }

    #[cfg(feature = "pretty-shell")]
    #[test]
    fn emergency_hotkey_p_commits_pretty_shell() {
        let mut app = emergency_app();
        assert!(app.on_key(press(KeyCode::Char('p'))));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::PrettyShell));
    }

    #[test]
    fn emergency_hotkeys_t_and_v_commit_retry_and_verify() {
        let mut app = emergency_app();
        assert!(app.on_key(press(KeyCode::Char('t'))));
        assert_eq!(emergency_state(&app).1, Some(EmergencyChoice::RetryBoot));

        let mut app = emergency_app();
        assert!(app.on_key(press(KeyCode::Char('v'))));
        assert_eq!(
            emergency_state(&app).1,
            Some(EmergencyChoice::VerifyKexecReadiness)
        );
    }

    #[test]
    fn boot_status_constructor_parks_app_on_boot_screen() {
        let app = App::boot_status("phase 0: kernel handoff");
        assert!(app.decision.is_none());
        match &app.screen {
            Screen::BootStatus(data) => {
                assert_eq!(&*data.phase, "phase 0: kernel handoff");
                assert!(data.log_lines.is_empty());
                assert_eq!(data.spinner_frame, 0);
            }
            _ => panic!("expected BootStatus screen"),
        }
    }

    #[test]
    fn boot_status_setters_mutate_in_place() {
        let mut app = App::boot_status("initial");
        app.set_boot_phase("phase 2");
        app.set_boot_log_lines(vec!["one".into(), "two".into()]);
        match &app.screen {
            Screen::BootStatus(data) => {
                assert_eq!(&*data.phase, "phase 2");
                assert_eq!(data.log_lines, vec!["one", "two"]);
            }
            _ => panic!("expected BootStatus screen"),
        }
    }

    #[test]
    fn boot_status_spinner_tick_wraps_modulo_frame_count() {
        let mut app = App::boot_status("waiting");
        for _ in 0..SPINNER_FRAMES {
            app.tick_boot_spinner();
        }
        // SPINNER_FRAMES ticks must wrap back to 0.
        match &app.screen {
            Screen::BootStatus(data) => assert_eq!(data.spinner_frame, 0),
            _ => panic!("expected BootStatus screen"),
        }
        // One more tick lands on frame 1.
        app.tick_boot_spinner();
        match &app.screen {
            Screen::BootStatus(data) => assert_eq!(data.spinner_frame, 1),
            _ => panic!("expected BootStatus screen"),
        }
    }

    #[test]
    fn boot_status_on_key_does_not_produce_decision() {
        let mut app = App::boot_status("phase X");
        // Any keypress is absorbed; no decision is emitted.
        for code in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Char('s'),
            KeyCode::Char('q'),
        ] {
            assert!(!app.on_key(press(code)), "{code:?} must not exit");
            assert!(app.decision.is_none(), "{code:?} must not set decision");
        }
    }

    // The boot-status setters use `debug_assert!(false, ...)` on the
    // wrong-screen branch, so behaviour differs between profiles:
    //   - debug builds: each setter panics with the assertion text.
    //   - release builds: each setter is a silent no-op.
    // We pin both profiles so a future edit that breaks either path
    // (e.g. flipping `debug_assert!` to `assert!`, or swapping the
    // branch to a state mutation) is caught by `cargo test`.

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "set_boot_phase called on non-BootStatus screen")]
    fn boot_status_set_phase_panics_on_wrong_screen_in_debug() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens); // Screen::List
        app.set_boot_phase("ignored");
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "set_boot_log_lines called on non-BootStatus screen")]
    fn boot_status_set_log_lines_panics_on_wrong_screen_in_debug() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens); // Screen::List
        app.set_boot_log_lines(vec!["ignored".into()]);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "tick_boot_spinner called on non-BootStatus screen")]
    fn boot_status_tick_spinner_panics_on_wrong_screen_in_debug() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens); // Screen::List
        app.tick_boot_spinner();
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn boot_status_setters_are_noop_on_wrong_screen_in_release() {
        // debug_assert is stripped, so each setter must leave the App
        // unchanged when invoked on a non-BootStatus screen.
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens); // Screen::List
        app.set_boot_phase("ignored");
        app.set_boot_log_lines(vec!["ignored".into()]);
        app.tick_boot_spinner();
        assert!(matches!(app.screen, Screen::List));
    }

    // ---- Error-screen countdown latch -----------------------------

    #[test]
    fn latch_error_countdown_sets_deadline_on_first_call() {
        // First invocation must transition deadline from None → Some.
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        assert!(app.error_countdown_deadline.is_none());
        app.latch_error_countdown(std::time::Duration::from_secs(30));
        assert!(app.error_countdown_deadline.is_some());
    }

    #[test]
    fn latch_error_countdown_is_idempotent_across_reentries() {
        // Re-entry to the error screen (operator dismissed a modal,
        // navigated back) MUST NOT restart the timer. The deadline
        // captured on the first call must survive every subsequent
        // call regardless of duration.
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.latch_error_countdown(std::time::Duration::from_secs(30));
        let deadline_a = app.error_countdown_deadline;
        // Re-enter twice with different (smaller / larger) durations.
        app.latch_error_countdown(std::time::Duration::from_secs(5));
        let deadline_b = app.error_countdown_deadline;
        app.latch_error_countdown(std::time::Duration::from_secs(99));
        let deadline_c = app.error_countdown_deadline;
        assert_eq!(deadline_a, deadline_b);
        assert_eq!(deadline_a, deadline_c);
    }

    #[test]
    fn latch_error_countdown_preserves_elapsed_deadline() {
        // If the deadline already elapsed during time spent on
        // another screen, the latch must keep the elapsed deadline
        // — the loop driver observes `now >= deadline` and reboots
        // immediately. We test this by pre-setting a deadline in
        // the past and confirming the latch leaves it alone.
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        let past = std::time::Instant::now() - std::time::Duration::from_secs(10);
        app.error_countdown_deadline = Some(past);
        app.latch_error_countdown(std::time::Duration::from_secs(30));
        assert_eq!(
            app.error_countdown_deadline,
            Some(past),
            "latch must not refresh an already-elapsed deadline"
        );
    }

    // ---- Modal overlay state --------------------------------------

    #[test]
    fn modal_field_defaults_to_none_on_construction() {
        // App::new() must start with no modal so a fresh boot doesn't
        // accidentally render a stale overlay.
        let gens: Vec<Generation> = vec![];
        let app = App::new(&gens);
        assert!(app.modal.is_none());
    }

    #[test]
    fn modal_scroll_offset_defaults_to_zero_on_construction() {
        let gens: Vec<Generation> = vec![];
        let app = App::new(&gens);
        assert_eq!(app.modal_scroll_offset, 0);
    }

    #[test]
    fn modal_scroll_down_clamps_at_total_minus_visible() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        // 10 total rows, viewport of 4 → max offset is 6.
        app.modal_scroll_down(1, 10, 4);
        assert_eq!(app.modal_scroll_offset, 1);
        app.modal_scroll_down(10, 10, 4);
        assert_eq!(app.modal_scroll_offset, 6, "clamped at total - visible");
        // Down past max stays at max.
        app.modal_scroll_down(99, 10, 4);
        assert_eq!(app.modal_scroll_offset, 6);
    }

    #[test]
    fn modal_scroll_up_saturates_at_zero() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.modal_scroll_offset = 3;
        app.modal_scroll_up(2);
        assert_eq!(app.modal_scroll_offset, 1);
        // Past zero stays at zero.
        app.modal_scroll_up(99);
        assert_eq!(app.modal_scroll_offset, 0);
    }

    #[test]
    fn modal_scroll_reset_clears_offset() {
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.modal_scroll_offset = 5;
        app.modal_scroll_reset();
        assert_eq!(app.modal_scroll_offset, 0);
    }

    #[test]
    fn modal_field_carries_status_overlay_payload() {
        // ModalKind::Status round-trips its payload exactly. The
        // BootReporter writes into this variant when an emergency
        // action wants the menu visible behind a progress dialog.
        let gens: Vec<Generation> = vec![];
        let mut app = App::new(&gens);
        app.modal = Some(ModalKind::Status {
            phase: "phase X".into(),
            log_lines: vec!["one".into()],
            spinner_frame: 2,
        });
        match &app.modal {
            Some(ModalKind::Status {
                phase,
                log_lines,
                spinner_frame,
            }) => {
                assert_eq!(phase, "phase X");
                assert_eq!(log_lines, &vec!["one".to_string()]);
                assert_eq!(*spinner_frame, 2);
            }
            other => panic!("expected ModalKind::Status, got {other:?}"),
        }
    }

    #[test]
    fn release_events_are_ignored() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert!(!app.on_key(release));
        assert!(app.decision.is_none());
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_e_sets_exit_session() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert!(!app.exit_session);
        // Ctrl+E sets the flag, produces no Decision, and does not exit.
        assert!(!app.on_key(ctrl(KeyCode::Char('e'))));
        assert!(app.exit_session);
        assert!(app.decision.is_none());
        // Plain 'e' from the list still opens the editor — proving the
        // global handler only fires with CONTROL held.
        let mut app2 = App::new(&gens);
        app2.on_key(press(KeyCode::Char('e')));
        assert!(matches!(app2.screen, Screen::Editing { .. }));
        assert!(!app2.exit_session);
    }

    #[test]
    fn ctrl_l_opens_log_and_esc_returns() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        assert!(matches!(app.screen, Screen::List));

        // Ctrl+L opens the log viewer.
        assert!(!app.on_key(ctrl(KeyCode::Char('l'))));
        assert!(matches!(app.screen, Screen::Log { .. }));

        // Esc pops back to the List.
        assert!(!app.on_key(press(KeyCode::Esc)));
        assert!(matches!(app.screen, Screen::List));

        // Re-open then close via a second Ctrl+L.
        app.on_key(ctrl(KeyCode::Char('l')));
        assert!(matches!(app.screen, Screen::Log { .. }));
        app.on_key(ctrl(KeyCode::Char('l')));
        assert!(matches!(app.screen, Screen::List));
    }

    #[test]
    fn log_scroll_offset_moves_and_saturates() {
        let gens = vec![fake_gen(1, &[])];
        let mut app = App::new(&gens);
        app.screen = Screen::Log {
            lines: vec!["a".into(), "b".into(), "c".into()],
            offset: 0,
        };

        // Up at 0 saturates at 0.
        app.on_key(press(KeyCode::Up));
        assert!(matches!(app.screen, Screen::Log { offset: 0, .. }));
        // Down advances by 1.
        app.on_key(press(KeyCode::Down));
        assert!(matches!(app.screen, Screen::Log { offset: 1, .. }));
        // PageDown advances by a page.
        app.on_key(press(KeyCode::PageDown));
        assert!(matches!(app.screen, Screen::Log { offset, .. } if offset == 1 + LOG_PAGE));
        // End jumps to u16::MAX (renderer clamps for display).
        app.on_key(press(KeyCode::End));
        assert!(matches!(
            app.screen,
            Screen::Log {
                offset: u16::MAX,
                ..
            }
        ));
        // Home returns to 0.
        app.on_key(press(KeyCode::Home));
        assert!(matches!(app.screen, Screen::Log { offset: 0, .. }));
    }

    #[test]
    fn shared_session_latch_spans_two_apps() {
        // A keypress on one App built from a session must be visible to
        // a second App built via new_in_session — proving the emergency
        // screen sees interaction from the selector / passphrase prompt.
        let session = SessionInteraction::new();
        let gens = vec![fake_gen(1, &[])];
        let mut first = App::new_in_session(&gens, &session);
        assert!(!session.get());
        first.on_key(press(KeyCode::Char('p')));
        assert!(session.get());

        let second = App::new_in_session(&[], &session);
        assert!(
            second.interaction.get(),
            "second App must observe the shared latch"
        );
    }
}
