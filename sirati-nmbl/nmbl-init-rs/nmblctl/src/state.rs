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

    /// Explicit mounted-system discovery for authenticated rescue operators.
    pub fn discover_for(cli: &nmblctl::Cli) -> Result<Self, String> {
        let mut sys = Self::discover()?;
        if let Some(path) = &cli.config {
            validate_operator_path(path, false)?;
            sys.config =
                Some(Config::load(path).map_err(|e| format!("loading operator config: {e}"))?);
            sys.config_source = Some(path.clone());
            sys.state_dir = discover_state_dir(sys.config.as_ref());
        }
        // The sidecar describes PID 1's pre-kexec namespace. On the running
        // installed system those mounts are rooted at / rather than /mnt/system.
        // Rescue operators explicitly supply their mounted namespace below.
        if cli.config.is_none()
            && cli.system_root.is_none()
            && let Some(config) = sys.config.as_mut()
        {
            config.paths.system_root = PathBuf::from("/");
            config.paths.nix_profiles_dir = PathBuf::from("/nix/var/nix/profiles");
        }
        if let Some(path) = &cli.system_root {
            validate_operator_path(path, true)?;
            let config = sys
                .config
                .as_mut()
                .ok_or("--system-root requires a readable config")?;
            config.paths.system_root = path.clone();
            config.paths.nix_profiles_dir = path.join("nix/var/nix/profiles");
        }
        if let Some(path) = &cli.profiles_dir {
            validate_operator_path(path, true)?;
            let config = sys
                .config
                .as_mut()
                .ok_or("--profiles-dir requires a readable config")?;
            config.paths.nix_profiles_dir = path.clone();
        }
        if let Some(path) = &cli.state_dir {
            validate_operator_path(path, true)?;
            sys.state_dir = path.clone();
        }
        if let Some(config) = sys.config.as_mut() {
            config.runtime_boot_mountpoint = if cli.config.is_some() {
                sys.config_source
                    .as_deref()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .map(Path::to_path_buf)
            } else {
                sys.state_dir.parent().map(Path::to_path_buf)
            };
        }
        (sys.generations, sys.active_generation) = scan_profiles(sys.config.as_ref());
        sys.rescue_mode = sys.config.as_ref().map(rescue_mode_str);
        Ok(sys)
    }

    /// Resolve actual bootable closure and verify signatures before creating
    /// recovery authorization; boot re-verifies pinned artifacts before load.
    pub fn validate_retry_target(&self, number: u32) -> Result<(), String> {
        self.validate_generation(number)?;
        let config = self
            .config
            .as_ref()
            .ok_or("retry requires a readable NMBL config")?;
        if config.stateful.is_none() {
            return Err("retry requires enabled persistent stateful recovery; use reboot-into on non-stateful hosts".into());
        }
        nmbl_init::state::read(&self.state_dir.join("state.bin"))
            .map_err(|e| format!("reading retry state: {e}"))?
            .ok_or("retry requires supported persistent boot state")?;
        let mut console = nmbl_init::ui::console::NoopConsole::new();
        let mut reporter =
            nmbl_init::ui::BootReporter::new(&mut console, "validating operator retry");
        let generations = nmbl_init::generations::scan_generations(config, &mut reporter)
            .map_err(|e| format!("scanning retry closures: {e}"))?;
        let target = generations
            .iter()
            .find(|g| g.number == number)
            .ok_or("retry generation closure is not bootable")?;
        let store = fs::canonicalize(config.paths.system_root.join("nix/store"))
            .map_err(|e| format!("resolving installed store: {e}"))?;
        let toplevel = fs::canonicalize(&target.toplevel)
            .map_err(|e| format!("resolving retry closure: {e}"))?;
        if toplevel.parent() != Some(store.as_path()) {
            return Err("retry profile must resolve to the installed Nix store".into());
        }
        if config.signing.enable {
            nmbl_init::sig::verify_generation_pinned(config, target)
                .map_err(|e| format!("retry generation signature verification failed: {e}"))?;
        }
        Ok(())
    }

    /// Stable public policy state for operator diagnostics and recovery proof.
    pub fn status_json(&self) -> Result<String, String> {
        let state = nmbl_init::state::read(&self.state_dir.join("state.bin"))
            .map_err(|e| format!("reading persistent boot state: {e}"))?
            .ok_or("persistent boot state absent or unsupported")?;
        let maximum = self
            .config
            .as_ref()
            .and_then(|c| c.stateful.as_ref())
            .map(|s| s.max_recovery_attempts);
        let retry = std::fs::read_to_string(
            self.state_dir
                .join(nmbl_init::boot_selection::RETRY_BASENAME),
        )
        .ok()
        .and_then(|s| nmbl_init::boot_selection::OneShotSelection::parse(&s));
        let mut decision_state = state.clone();
        let exhaustion = maximum
            .map(|budget| -> Result<bool, String> {
                let config = self
                    .config
                    .as_ref()
                    .ok_or("stateful policy config unavailable")?;
                let mut console = nmbl_init::ui::console::NoopConsole::new();
                let mut reporter =
                    nmbl_init::ui::BootReporter::new(&mut console, "inspect bootable profiles");
                let generations = nmbl_init::generations::scan_generations(config, &mut reporter)
                    .map_err(|e| {
                    format!("cannot inspect bootable profiles for recovery policy: {e}")
                })?;
                let active = self
                    .active_generation
                    .and_then(|n| generations.iter().position(|g| g.number == n))
                    .unwrap_or(0);
                Ok(matches!(
                    nmbl_init::state::decide(&mut decision_state, &generations, active, budget),
                    nmbl_init::state::StatefulDecision::Exhausted
                ))
            })
            .transpose()?;
        serde_json::to_string(&serde_json::json!({
            "version": 1, "state_format_version": state.state_format_version,
            "last_attempted_generation": state.last_attempted_generation.map(|n| n.get()),
            "last_boot_succeeded": state.last_boot_succeeded, "recovery_attempt": state.recovery_attempt,
            "known_good_generations": state.known_good_generations.map(|n| n.map(|v| v.get())),
            "max_recovery_attempts": maximum, "automatic_recovery_exhausted": exhaustion,
            "pending_retry_generation": retry.map(|r| r.generation), "installed_generations": self.generations,
            "signing_enabled": self.config.as_ref().map(|c| c.signing.enable),
            "signing_enforced": self.config.as_ref().map(|c| c.signing.enforce),
            "signed_erofs": self.is_signed_erofs(), "active_generation": self.active_generation
        })).map_err(|e| format!("serializing policy state: {e}"))
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

/// Explicit recovery paths must be canonical, root-owned and not writable by
/// other users. Ancestor ownership prevents redirecting an authenticated write.
pub fn validate_operator_path(path: &Path, directory: bool) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let canonical = fs::canonicalize(path).map_err(|e| format!("resolving operator path: {e}"))?;
    if !path.is_absolute() || canonical != path {
        return Err("operator path must be absolute without symlinks or traversal".into());
    }
    for ancestor in path.ancestors() {
        let metadata =
            fs::symlink_metadata(ancestor).map_err(|e| format!("checking operator path: {e}"))?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(
                "operator path and ancestors must be root-owned and not writable by other users"
                    .into(),
            );
        }
    }
    let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        return Err("operator path has wrong file type".into());
    }
    Ok(())
}

/// Write `data` to `path` durably: a temp file in the same dir, fsync'd,
/// renamed over the target, then the directory fsync'd. This is the same
/// atomic-rename discipline NMBL's own state writers use, so a crash leaves
/// either the old or the new flag file, never a torn one.
pub fn write_durable(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let basename = path.file_name().and_then(|s| s.to_str()).unwrap_or("flag");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_nanos();
    let mut temporary = None;
    // Stale crash files and concurrent invocations cannot block a later retry,
    // and create_new never follows/truncates an attacker-supplied symlink.
    for collision in 0..32 {
        let tmp = parent.join(format!(
            ".{basename}.nmblctl.tmp.{}.{nonce}.{collision}",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .mode(0o600)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => {
                temporary = Some((tmp, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let (tmp, mut file) = temporary.ok_or_else(|| {
        std::io::Error::other("could not allocate a unique durable temporary file")
    })?;
    let result = (|| {
        file.write_all(data)?;
        file.flush()?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        // VFAT rename creates a new directory slot and dirties the moved file
        // inode. Persist its size/cluster at that new slot before syncing the
        // parent: pre-rename file fsync alone can leave an empty file on reset.
        file.sync_all()?;
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
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
    #[test]
    fn json_status_reads_real_persistent_state_without_blessing_or_mutation() {
        let dir = tempfile::tempdir().expect("state");
        let path = dir.path().join("state.bin");
        let original = nmbl_init::state::State {
            recovery_attempt: 5,
            last_boot_succeeded: false,
            ..nmbl_init::state::State::default()
        };
        nmbl_init::state::write_padded(&path, &original).expect("write");
        let sys = System {
            config: None,
            config_source: None,
            rescue_mode: None,
            state_dir: dir.path().to_path_buf(),
            cmdline: String::new(),
            generations: vec![42],
            active_generation: Some(42),
        };
        let json: serde_json::Value =
            serde_json::from_str(&sys.status_json().expect("status")).expect("json");
        assert_eq!(json.get("version").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(
            json.get("recovery_attempt").and_then(|v| v.as_u64()),
            Some(5)
        );
        assert_eq!(
            json.get("last_boot_succeeded").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(nmbl_init::state::read(&path).expect("read"), Some(original));
    }
    #[test]
    fn stale_temporary_file_or_symlink_does_not_block_or_get_truncated() {
        let dir = tempfile::tempdir().expect("state");
        let stale = dir.path().join(format!(
            ".retry-generation.nmblctl.tmp.{}",
            std::process::id()
        ));
        fs::write(&stale, b"original stale intent").expect("stale");
        let target = dir.path().join("retry-generation");
        write_durable(&target, b"generation 42\n").expect("retry");
        assert_eq!(
            fs::read(&stale).expect("preserved"),
            b"original stale intent"
        );
        fs::remove_file(&stale).expect("remove");
        std::os::unix::fs::symlink(&target, &stale).expect("stale symlink");
        write_durable(&target, b"generation 43\n").expect("retry");
        assert!(
            fs::symlink_metadata(&stale)
                .expect("preserved symlink")
                .is_symlink()
        );
        assert_eq!(fs::read(&target).expect("target"), b"generation 43\n");
    }
    #[test]
    fn json_status_refuses_exhaustion_classification_when_scan_is_unavailable() {
        let dir = tempfile::tempdir().expect("state");
        let original = nmbl_init::state::State {
            recovery_attempt: 5,
            last_boot_succeeded: false,
            ..nmbl_init::state::State::default()
        };
        nmbl_init::state::write_padded(&dir.path().join("state.bin"), &original).expect("state");
        let mut config = Config::recovery_default();
        config.stateful = Some(nmbl_init::config::StatefulConfig {
            max_recovery_attempts: 5,
            success_target: "multi-user.target".into(),
        });
        config.paths.system_root = dir.path().join("installed");
        config.paths.nix_profiles_dir = dir.path().join("missing-profiles");
        let mut sys = System {
            config: Some(config),
            config_source: None,
            rescue_mode: None,
            state_dir: dir.path().to_path_buf(),
            cmdline: String::new(),
            generations: vec![42],
            active_generation: Some(42),
        };
        assert!(
            sys.status_json()
                .expect_err("missing scan")
                .contains("cannot inspect bootable profiles")
        );
        fs::write(dir.path().join("malformed-profiles"), b"not a directory").expect("malformed");
        sys.config.as_mut().expect("config").paths.nix_profiles_dir =
            dir.path().join("malformed-profiles");
        assert!(
            sys.status_json()
                .expect_err("malformed scan")
                .contains("cannot inspect bootable profiles")
        );
        assert_eq!(
            nmbl_init::state::read(&dir.path().join("state.bin")).expect("read"),
            Some(original)
        );
    }
}
