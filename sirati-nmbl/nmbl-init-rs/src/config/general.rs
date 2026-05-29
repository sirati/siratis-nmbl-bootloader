use std::path::PathBuf;

use serde::Deserialize;

use crate::log::Verbosity;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct General {
    #[serde(default)]
    pub verbosity: Verbosity,

    /// Selector-TUI countdown before auto-booting the default entry, in
    /// milliseconds. A sub-second value (e.g. `500`) is supported; the
    /// display still rounds up so it never shows a misleading "0s".
    /// NixOS-generated configs derive this from `timeoutSeconds * 1000`.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u32,

    /// Optional override (seconds) for the emergency/error screen's
    /// auto-reboot countdown. When `Some(s)` it replaces the built-in
    /// 30 s default; absent keeps the historic 30 s budget so the boot
    /// UX does not silently change on upgrade.
    #[serde(default)]
    pub emergency_timeout_secs: Option<u64>,

    /// Per-device readiness budget (seconds) used while waiting for a
    /// `fileSystems[].device` to appear during mount, and while waiting
    /// for cryptsetup / LVM / mdraid activations to materialise their
    /// produced block devices. Honoured by `devices::wait_for` at every
    /// call site.
    #[serde(default = "default_device_timeout_secs")]
    pub device_timeout_secs: u64,

    #[serde(default = "default_panic_report_dir")]
    pub panic_report_dir: PathBuf,

    /// Legacy toggle: kept as a no-op field so existing configs that
    /// still set `serial_console = true/false` parse successfully under
    /// `deny_unknown_fields`. The TUI now renders through ratatui's
    /// crossterm backend on every console kind (framebuffer VT,
    /// `/dev/tty1`, serial UART) — vt100/xterm escapes round-trip over
    /// any modern serial terminal. No code path branches on this flag.
    #[serde(default, rename = "serial_console")]
    pub _serial_console_compat: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            verbosity: Verbosity::default(),
            timeout_ms: default_timeout_ms(),
            emergency_timeout_secs: None,
            device_timeout_secs: default_device_timeout_secs(),
            panic_report_dir: default_panic_report_dir(),
            _serial_console_compat: false,
        }
    }
}

pub(super) fn default_timeout_ms() -> u32 {
    5000
}

pub(super) fn default_device_timeout_secs() -> u64 {
    30
}

pub(super) fn default_panic_report_dir() -> PathBuf {
    PathBuf::from("/run")
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelModules {
    /// Modules loaded BEFORE the boot console is brought up. Reserved
    /// for graphics drivers (`virtio_gpu`, `simpledrm`, `i915`, …) so
    /// `/dev/dri/card*` exists when `open_console` tries to attach the
    /// splash backend. Loaded by `modules::load_modules(_,_,
    /// ModuleSet::Early)` during phase 2a, immediately before
    /// `open_console`.
    #[serde(default)]
    pub early: Vec<String>,

    /// Storage / filesystem / activation modules loaded in phase 2b,
    /// AFTER the boot console is up so the operator sees per-module
    /// progress.
    #[serde(default)]
    pub explicit: Vec<String>,

    #[serde(default)]
    pub blacklist: Vec<String>,

    #[serde(default = "default_modules_dir")]
    pub modules_dir: PathBuf,
}

pub(super) fn default_modules_dir() -> PathBuf {
    PathBuf::from("/lib/modules")
}
