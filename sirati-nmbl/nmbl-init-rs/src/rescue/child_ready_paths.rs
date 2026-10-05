//! Parent-owned anchors survive the rescue mount plan covering /mnt.
use crate::config::Config;
use crate::error::{NmblError, Result};
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

struct Anchor {
    directory: File,
    name: OsString,
}
impl Anchor {
    fn capture(path: &Path) -> std::io::Result<Option<Self>> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("persistent path has no parent"))?;
        let name = path
            .file_name()
            .ok_or_else(|| std::io::Error::other("persistent path has no filename"))?
            .to_os_string();
        let directory = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(parent)
        {
            Ok(directory) => directory,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(Self { directory, name }))
    }
    fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.directory.as_raw_fd())).join(&self.name)
    }
}

pub(super) struct ReadyPaths {
    #[cfg(feature = "stateful")]
    state_required: bool,
    #[cfg(feature = "stateful")]
    state: Option<Anchor>,
    sentinel: Option<Anchor>,
}
impl ReadyPaths {
    pub(super) fn capture(config: &Config) -> std::io::Result<Self> {
        #[cfg(feature = "stateful")]
        let state = if config.stateful.is_some() {
            match config
                .runtime_state_mountpoint
                .as_ref()
                .or(config.runtime_boot_mountpoint.as_ref())
            {
                Some(mount) => Anchor::capture(&mount.join("nmbl/state.bin"))?,
                None => None,
            }
        } else {
            None
        };
        let sentinel = match crate::policy::sentinel::resolve_sentinel_path(config) {
            Some(path) => Anchor::capture(&path)?,
            None => None,
        };
        Ok(Self {
            #[cfg(feature = "stateful")]
            state_required: config.stateful.is_some(),
            #[cfg(feature = "stateful")]
            state,
            sentinel,
        })
    }
    pub(super) fn record(&self) -> Result<()> {
        #[cfg(feature = "stateful")]
        if self.state_required {
            let state = self.state.as_ref().ok_or_else(unavailable_state)?;
            if !crate::state::record_rescue_booted(&state.path())? {
                return Err(unavailable_state());
            }
        }
        if let Some(sentinel) = &self.sentinel {
            crate::policy::sentinel::consume_pinned_sentinel(&sentinel.path()).map_err(
                |source| NmblError::Io {
                    source,
                    context: "consuming pinned rescue request".into(),
                },
            )?;
        }
        Ok(())
    }
}
#[cfg(feature = "stateful")]
fn unavailable_state() -> NmblError {
    NmblError::ConfigInvalid {
        reason: "supported persistent boot state is unavailable; keeping rescue request".into(),
        context: "rescue readiness".into(),
    }
}

#[cfg(all(test, feature = "stateful"))]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "tests assert pinned state namespace contracts"
)]
mod tests {
    use super::*;
    #[test]
    fn replacing_runtime_mount_paths_cannot_redirect_readiness_write_or_sentinel_clear() {
        let dir = tempfile::tempdir().unwrap();
        let boot = dir.path().join("boot");
        std::fs::create_dir_all(boot.join("nmbl")).unwrap();
        let mut config = Config::recovery_default();
        config.stateful = Some(crate::config::StatefulConfig {
            max_recovery_attempts: 2,
            success_target: "boot-complete.target".into(),
        });
        config.runtime_boot_mountpoint = Some(boot.clone());
        config.runtime_state_mountpoint = Some(boot.clone());
        let original = crate::state::State {
            last_boot_succeeded: false,
            last_attempted_generation: nonmax::NonMaxU32::new(42),
            recovery_attempt: 2,
            ..Default::default()
        };
        crate::state::write_padded(&boot.join("nmbl/state.bin"), &original).unwrap();
        crate::policy::write_sentinel(&config);
        let paths = ReadyPaths::capture(&config).unwrap();
        let saved = dir.path().join("preserved");
        std::fs::rename(&boot, &saved).unwrap();
        std::fs::create_dir_all(boot.join("nmbl")).unwrap();
        let decoy = crate::state::State::default();
        crate::state::write_padded(&boot.join("nmbl/state.bin"), &decoy).unwrap();
        crate::policy::write_sentinel(&config);
        paths.record().unwrap();
        let actual = crate::state::read(&saved.join("nmbl/state.bin"))
            .unwrap()
            .unwrap();
        assert_eq!(
            actual.rescue_booted_generation,
            original.last_attempted_generation
        );
        assert_eq!(actual.recovery_attempt, original.recovery_attempt);
        assert_eq!(
            actual.known_good_generations,
            original.known_good_generations
        );
        assert!(!saved.join("nmbl/rescue").exists());
        assert_eq!(
            crate::state::read(&boot.join("nmbl/state.bin"))
                .unwrap()
                .unwrap(),
            decoy
        );
        assert!(boot.join("nmbl/rescue").exists());
    }
    #[test]
    fn unsupported_state_never_consumes_pinned_rescue_request() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::recovery_default();
        config.stateful = Some(crate::config::StatefulConfig {
            max_recovery_attempts: 2,
            success_target: "boot-complete.target".into(),
        });
        config.runtime_boot_mountpoint = Some(dir.path().to_path_buf());
        config.runtime_state_mountpoint = Some(dir.path().to_path_buf());
        crate::policy::write_sentinel(&config);
        let paths = ReadyPaths::capture(&config).unwrap();
        assert!(paths.record().is_err());
        assert!(crate::policy::sentinel_present(&config));
    }
}
