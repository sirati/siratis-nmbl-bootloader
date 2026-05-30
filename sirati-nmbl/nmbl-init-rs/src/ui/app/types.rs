use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Instant;

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

/// Per-boot-session "skip the generation selector" latch. Mirrors
/// [`SessionInteraction`]: a shared `Rc<Cell<bool>>` cloned across the
/// passphrase prompt and the post-phase selector dispatch (everything
/// runs on the single `LocalRuntime`, so a `Cell` is enough).
///
/// Default = `false` = "show the selector" — today's behaviour, which
/// is what non-LUKS boots (no passphrase prompt ever runs) and CHECKED
/// passphrase submits both keep. Set to `true` ONLY when the operator
/// submits the LUKS passphrase with the "Select NixOS Generation"
/// checkbox left UNCHECKED, instructing the dispatcher to boot the
/// default generation immediately without rendering the selector.
#[derive(Clone, Default)]
pub struct SkipSelector(Rc<Cell<bool>>);
impl SkipSelector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
    #[must_use]
    pub fn get(&self) -> bool {
        self.0.get()
    }
    /// Record the operator's checkbox state at passphrase submit:
    /// `true` ⇒ skip the selector, `false` ⇒ show it.
    pub fn set(&self, skip: bool) {
        self.0.set(skip);
    }
}

/// Page size (in rows) for [`Screen::Log`] PageUp/PageDown scrolling.
pub(crate) const LOG_PAGE: u16 = 20;

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
        /// "Select NixOS Generation" checkbox. Default `false`
        /// (unchecked): on submit the generation selector is SKIPPED and
        /// the default generation boots immediately. Toggled by Ctrl+G
        /// while this screen is active. `true` (checked) keeps today's
        /// behaviour — show the selector after unlock.
        select_generation: bool,
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
