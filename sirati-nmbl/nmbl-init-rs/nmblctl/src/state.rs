//! System discovery: read NMBL's runtime config and state from the booted
//! system so the views and actions have one place that touches the filesystem.
//!
//! `nmblctl` runs on the live NixOS after NMBL kexec'd into it, so the runtime
//! config.toml (embedded or external), the generation profiles, the stateful
//! `state.bin`, the signed-EROFS selectors, and `/proc/cmdline` are all
//! readable. [`System::discover`] gathers them once; the views render from the
//! result and the actions validate against it.

use std::fs;
use std::path::{Path, PathBuf};

use nmbl_init::config::Config;

/// The discovered NMBL system state.
pub struct System {
    /// The parsed runtime config, when one could be located and parsed.
    pub config: Option<Config>,
    /// Where the config was read from (for the chain view).
    pub config_source: Option<PathBuf>,
    /// `rescue.mode` string from the config (`embedded`/`external`/`none`).
    pub rescue_mode: Option<String>,
    /// The state dir NMBL uses for its flag files (default `/boot/nmbl`).
    pub state_dir: PathBuf,
    /// `/proc/cmdline`, verbatim.
    pub cmdline: String,
    /// Discovered generation numbers (newest first), from the Nix profiles.
    pub generations: Vec<u32>,
    /// The active generation number (the `system` profile target), if known.
    pub active_generation: Option<u32>,
}

impl System {
    /// Discover the live NMBL state. Never fails hard on a missing optional
    /// input — a partial view is better than no view — but returns `Err` only
    /// when nothing at all about NMBL can be found (not an NMBL system).
    pub fn discover() -> Result<Self, String> {
        let (config, config_source) = load_runtime_config();
        let rescue_mode = config.as_ref().map(rescue_mode_str);
        let state_dir = discover_state_dir(config.as_ref());
        let cmdline = fs::read_to_string("/proc/cmdline").unwrap_or_default();
        let (generations, active_generation) = scan_profiles(config.as_ref());

        if config.is_none() && generations.is_empty() && cmdline.is_empty() {
            return Err(
                "no NMBL config, generations, or /proc/cmdline found — is this an NMBL system?"
                    .to_string(),
            );
        }
        Ok(Self {
            config,
            config_source,
            rescue_mode,
            state_dir,
            cmdline,
            generations,
            active_generation,
        })
    }

    /// The rescue sentinel path NMBL checks. Prefers the secure-boot config's
    /// `sentinel_path`, else the pinned default.
    #[must_use]
    pub fn sentinel_path(&self) -> PathBuf {
        self.config
            .as_ref()
            .map(|c| c.secure_boot.sentinel_path.clone())
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| PathBuf::from(nmbl_init::security_consts::SENTINEL_PATH))
    }

    /// Whether this host selects generations through the signed-EROFS `active`
    /// symlink (signature-gated), rather than the Nix `system` profile symlink.
    #[must_use]
    pub fn is_signed_erofs(&self) -> bool {
        self.config
            .as_ref()
            .and_then(|c| c.generation_image.as_ref())
            .is_some_and(|g| g.enable)
    }

    /// Validate that `generation` is one nmblctl can target. On profile hosts
    /// it must be a discovered generation number. On signed-EROFS hosts the
    /// selection is the verified `active` symlink, so a raw generation-number
    /// override is refused with a pointer to the verified tooling (see
    /// `nmblctl::flags` for the signature-safety rationale).
    pub fn validate_generation(&self, generation: u32) -> Result<(), String> {
        if self.is_signed_erofs() {
            return Err(format!(
                "this host boots signed EROFS generations selected by the verified `active` \
                 symlink; nmblctl will not write a raw generation-{generation} override that \
                 would bypass signature verification. Use `nmbl-erofsctl activate` / `rollback` \
                 (which re-verify the target's signature) instead"
            ));
        }
        if self.generations.is_empty() {
            // No profile scan available (e.g. running off-host): accept the
            // number rather than block, since the boot-time selector validates.
            return Ok(());
        }
        if self.generations.contains(&generation) {
            Ok(())
        } else {
            Err(format!(
                "generation {generation} is not among the installed generations {:?}",
                self.generations
            ))
        }
    }

    /// Read the persistent default selection, if set.
    #[must_use]
    pub fn read_default(&self) -> Option<nmblctl::flags::DefaultSelection> {
        let path = self.state_dir.join(nmblctl::flags::DEFAULT_BASENAME);
        let text = fs::read_to_string(path).ok()?;
        nmblctl::flags::DefaultSelection::parse(&text)
    }

    /// Read the one-shot selection, if set.
    #[must_use]
    pub fn read_one_shot(&self) -> Option<nmblctl::flags::OneShotSelection> {
        let path = self.state_dir.join(nmblctl::flags::ONE_SHOT_BASENAME);
        let text = fs::read_to_string(path).ok()?;
        nmblctl::flags::OneShotSelection::parse(&text)
    }
}

/// Try the embedded config first (`/etc/nmbl/config.toml`), then the external
/// staging locations under `/boot`.
fn load_runtime_config() -> (Option<Config>, Option<PathBuf>) {
    const CANDIDATES: [&str; 4] = [
        "/etc/nmbl/config.toml",
        "/boot/nmbl/config.toml",
        "/boot/nmbl-generations/active/config.toml",
        "/persistent/nmbl-generations/active/config.toml",
    ];
    for cand in CANDIDATES {
        let path = Path::new(cand);
        if path.is_file()
            && let Ok(cfg) = Config::load(path)
        {
            return (Some(cfg), Some(path.to_path_buf()));
        }
    }
    (None, None)
}

fn rescue_mode_str(config: &Config) -> String {
    use nmbl_init::rescue::RescueMode;
    match config.rescue.mode {
        RescueMode::Embedded => "embedded",
        RescueMode::External => "external",
        RescueMode::None => "none",
    }
    .to_string()
}

fn discover_state_dir(config: Option<&Config>) -> PathBuf {
    // For signed-EROFS hosts the generation state root is the right home for
    // one-shot/default flags; otherwise /boot/nmbl (the stateful stateDir).
    if let Some(cfg) = config
        && let Some(gi) = cfg.generation_image.as_ref()
        && gi.enable
    {
        return gi.state_root.clone();
    }
    PathBuf::from("/boot/nmbl")
}

/// Scan the Nix profiles directory for `system-N-link` generations. Returns
/// `(numbers_newest_first, active_number)`.
fn scan_profiles(config: Option<&Config>) -> (Vec<u32>, Option<u32>) {
    let dir = config
        .map(|c| c.paths.nix_profiles_dir.clone())
        .unwrap_or_else(|| PathBuf::from("/nix/var/nix/profiles"));
    let mut numbers: Vec<u32> = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && let Some(n) = name
                    .strip_prefix("system-")
                    .and_then(|s| s.strip_suffix("-link"))
                    .and_then(|s| s.parse::<u32>().ok())
            {
                numbers.push(n);
            }
        }
    }
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    let active = fs::read_link(dir.join("system")).ok().and_then(|t| {
        t.file_name()
            .and_then(|s| s.to_str())
            .and_then(|s| s.strip_prefix("system-"))
            .and_then(|s| s.strip_suffix("-link"))
            .and_then(|s| s.parse::<u32>().ok())
    });
    (numbers, active)
}

/// Write `data` to `path` durably: a temp file in the same dir, fsync'd,
/// renamed over the target, then the directory fsync'd. This is the same
/// atomic-rename discipline NMBL's own state writers use, so a crash leaves
/// either the old or the new flag file, never a torn one.
pub fn write_durable(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.nmblctl.tmp.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("flag"),
        std::process::id()
    ));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(data)?;
        f.flush()?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // fsync the directory so the rename is durable.
    if let Ok(dir) = fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used, reason = "tests assert")]
mod tests {
    use super::*;

    #[test]
    fn write_durable_creates_and_replaces() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nmbl").join("boot-once");
        write_durable(&path, b"generation 3\n").expect("write");
        assert_eq!(fs::read_to_string(&path).expect("read"), "generation 3\n");
        // Overwrite atomically.
        write_durable(&path, b"generation 5\n").expect("rewrite");
        assert_eq!(fs::read_to_string(&path).expect("read"), "generation 5\n");
        // No temp file left behind.
        let leftovers: Vec<_> = fs::read_dir(path.parent().expect("parent"))
            .expect("dir")
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("nmblctl.tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away");
    }
}
