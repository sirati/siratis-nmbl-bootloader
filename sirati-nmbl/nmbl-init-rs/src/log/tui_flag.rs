use std::sync::atomic::{AtomicUsize, Ordering};

/// Set when an interactive console (splash framebuffer or raw-mode tty)
/// owns the screen. While true, the `nmbl_*!` macros suppress their
/// stderr branch — the TUI already surfaces phase/log output through its
/// own render loop, and writing through stderr races the ratatui
/// re-paint and produces visible smear (especially on serial, where
/// stderr→/dev/console and the kernel's printk echo to the same UART
/// produce duplicated lines like "phase 3" appearing back-to-back with a
/// `[ 1.234] phase 3` printk variant).
///
/// `/dev/kmsg` writes are still performed so the kernel ring buffer (and
/// any console the operator picked up via `console=` cmdline) keeps a
/// timestamped record — only the userspace stderr duplicate is silenced.
/// On `suspend` / handover to kexec/execve the count is decremented so the
/// post-handover path sees normal eprintln output again.
///
/// A refcount rather than a bool so nested/overlapping console owners
/// (e.g. a screen suspending to spawn a sub-console, or two paired
/// open/drop scopes) compose correctly: stderr stays suppressed as long
/// as *any* owner holds the console, and resumes only when the last one
/// releases it.
static TUI_CONSOLE_REFCOUNT: AtomicUsize = AtomicUsize::new(0);

/// Mark the console as TUI-owned: the `nmbl_*!` macros stop writing to
/// stderr until the matching [`clear_tui_active`] runs. Each call
/// increments a refcount, so every `set` must be paired with exactly one
/// `clear`. Cheap; safe to call from any code path that brings up a
/// [`crate::ui::console::Console`].
pub fn set_tui_active() {
    TUI_CONSOLE_REFCOUNT.fetch_add(1, Ordering::SeqCst);
}

/// Inverse of [`set_tui_active`]. Called when the TUI hands the screen
/// back to the kernel/foreign userspace (suspend, kexec handoff,
/// emergency-shell relay, drop on scope exit). Decrements the refcount;
/// stderr output resumes once it reaches zero.
pub fn clear_tui_active() {
    // saturating_sub semantics via fetch_update so an unpaired clear can
    // never wrap the count around to a huge value (which would wedge
    // stderr off forever).
    let _ = TUI_CONSOLE_REFCOUNT.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
        Some(n.saturating_sub(1))
    });
}

/// Internal helper for the `nmbl_*!` macros so the macro body stays
/// short and the gating logic has a single home.
#[doc(hidden)]
pub fn tui_active() -> bool {
    TUI_CONSOLE_REFCOUNT.load(Ordering::SeqCst) > 0
}
