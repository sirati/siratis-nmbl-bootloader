//! Persistent-default and one-shot boot selection files.
//!
//! `nmblctl default` / `nmblctl reboot-into` write two small text files in
//! NMBL's state dir that the boot-time selector honours:
//!
//! * `boot-default` — a persistent default (`latest`, or `generation <N>`);
//! * `boot-once` — a one-shot (`generation <N>`) NMBL boots exactly once and
//!   then deletes, like `grub-reboot`.
//!
//! This module is the SINGLE definition of those formats and of the boot-time
//! read/consume logic, used by both `nmbl-init` (to honour them) and `nmblctl`
//! (to write them). Keeping the format here means the writer and the reader
//! cannot drift.
//!
//! # Signature safety
//!
//! These files only ever adjust which generation the selector DEFAULTS to
//! among the ones it already discovered; they never bypass a signature gate.
//! On signed-EROFS hosts the boot selection is the verified `active` symlink,
//! and `nmblctl` refuses to write these files there (it routes through the
//! verified `nmbl-erofsctl` path instead), so the boot-time consumer here only
//! ever runs on profile/stateful hosts where the profile symlink is unsigned.

use std::path::{Path, PathBuf};

/// File basenames under the state dir. Shared with `nmblctl`.
pub const ONE_SHOT_BASENAME: &str = "boot-once";
pub const DEFAULT_BASENAME: &str = "boot-default";

/// A one-shot "boot this generation next" selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OneShotSelection {
    /// The generation number to boot next.
    pub generation: u32,
}

/// A persistent default: the newest generation, or a fixed number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSelection {
    /// Always the newest generation available at boot time.
    Latest,
    /// A specific generation number.
    Generation(u32),
}

impl OneShotSelection {
    /// Render the file body: `generation <N>\n`.
    #[must_use]
    pub fn render(self) -> String {
        format!("generation {}\n", self.generation)
    }

    /// Parse a one-shot body; `None` for anything but one `generation <N>`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let line = text.lines().find(|l| !l.trim().is_empty())?;
        let n = line.trim().strip_prefix("generation ")?.trim();
        Some(Self {
            generation: n.parse().ok()?,
        })
    }
}

impl DefaultSelection {
    /// Render the file body: `latest\n` or `generation <N>\n`.
    #[must_use]
    pub fn render(self) -> String {
        match self {
            DefaultSelection::Latest => "latest\n".to_string(),
            DefaultSelection::Generation(n) => format!("generation {n}\n"),
        }
    }

    /// Parse a default body; `None` for an unrecognised body.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
        if line == "latest" {
            return Some(DefaultSelection::Latest);
        }
        let n = line.strip_prefix("generation ")?.trim();
        Some(DefaultSelection::Generation(n.parse().ok()?))
    }
}

impl std::fmt::Display for DefaultSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DefaultSelection::Latest => write!(f, "latest (newest generation)"),
            DefaultSelection::Generation(n) => write!(f, "generation {n}"),
        }
    }
}

/// Read the one-shot selection from `state_dir`, if present and parseable.
/// Does NOT consume it — call [`consume_one_shot`] once the boot commits to it.
#[must_use]
pub fn read_one_shot(state_dir: &Path) -> Option<OneShotSelection> {
    let text = std::fs::read_to_string(one_shot_path(state_dir)).ok()?;
    OneShotSelection::parse(&text)
}

/// Read the persistent default from `state_dir`, if present and parseable.
#[must_use]
pub fn read_default(state_dir: &Path) -> Option<DefaultSelection> {
    let text = std::fs::read_to_string(default_path(state_dir)).ok()?;
    DefaultSelection::parse(&text)
}

/// Remove the one-shot file (best-effort) and fsync the directory so the
/// consumption is durable — a one-shot must fire exactly once even across a
/// crash right after this boot commits. A missing file is success.
pub fn consume_one_shot(state_dir: &Path) {
    let path = one_shot_path(state_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            crate::nmbl_warn!(
                "boot-selection: could not remove one-shot {}: {e}; it may re-fire next boot",
                path.display()
            );
            return;
        }
    }
    if let Ok(dir) = std::fs::File::open(state_dir) {
        let _ = rustix::fs::fsync(&dir);
    }
}

/// Resolve the effective default generation NUMBER for this boot from the
/// selection files, given the generations available (as numbers) and the
/// fallback the caller would otherwise use (the active-profile number).
///
/// Precedence: a valid one-shot wins; else the persistent default (`latest`
/// → the max available number; a specific number if it is still installed);
/// else `fallback`. A selection naming a generation that is no longer
/// installed is ignored (the file is stale) so the boot never dead-ends on a
/// GC'd target. This resolves a NUMBER; the caller maps it to an index and the
/// consumer deletes the one-shot after committing.
#[must_use]
pub fn resolve_default_number(state_dir: &Path, available: &[u32], fallback: u32) -> u32 {
    if let Some(one) = read_one_shot(state_dir)
        && available.contains(&one.generation)
    {
        return one.generation;
    }
    match read_default(state_dir) {
        Some(DefaultSelection::Latest) => available.iter().copied().max().unwrap_or(fallback),
        Some(DefaultSelection::Generation(n)) if available.contains(&n) => n,
        _ => fallback,
    }
}

fn one_shot_path(state_dir: &Path) -> PathBuf {
    state_dir.join(ONE_SHOT_BASENAME)
}

fn default_path(state_dir: &Path) -> PathBuf {
    state_dir.join(DEFAULT_BASENAME)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;

    #[test]
    fn one_shot_round_trips() {
        let sel = OneShotSelection { generation: 42 };
        assert_eq!(OneShotSelection::parse(&sel.render()), Some(sel));
    }

    #[test]
    fn default_round_trips() {
        for sel in [DefaultSelection::Latest, DefaultSelection::Generation(3)] {
            assert_eq!(DefaultSelection::parse(&sel.render()), Some(sel));
        }
    }

    #[test]
    fn parsing_rejects_garbage() {
        assert_eq!(OneShotSelection::parse("boot 7"), None);
        assert_eq!(DefaultSelection::parse("newest"), None);
        assert_eq!(DefaultSelection::parse(""), None);
    }

    #[test]
    fn one_shot_wins_over_default() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(DEFAULT_BASENAME), "generation 1\n").expect("w");
        std::fs::write(dir.path().join(ONE_SHOT_BASENAME), "generation 3\n").expect("w");
        assert_eq!(resolve_default_number(dir.path(), &[1, 2, 3], 2), 3);
    }

    #[test]
    fn latest_resolves_to_max_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(DEFAULT_BASENAME), "latest\n").expect("w");
        assert_eq!(resolve_default_number(dir.path(), &[5, 9, 2], 5), 9);
    }

    #[test]
    fn stale_selection_falls_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A default naming gen 99, which is not installed → fall back.
        std::fs::write(dir.path().join(DEFAULT_BASENAME), "generation 99\n").expect("w");
        assert_eq!(resolve_default_number(dir.path(), &[1, 2, 3], 2), 2);
        // A one-shot naming gen 99 (not installed) is also ignored.
        std::fs::write(dir.path().join(ONE_SHOT_BASENAME), "generation 99\n").expect("w");
        assert_eq!(resolve_default_number(dir.path(), &[1, 2, 3], 2), 2);
    }

    #[test]
    fn no_files_returns_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(resolve_default_number(dir.path(), &[1, 2, 3], 2), 2);
    }

    #[test]
    fn consume_removes_one_shot_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(ONE_SHOT_BASENAME), "generation 3\n").expect("w");
        assert!(read_one_shot(dir.path()).is_some());
        consume_one_shot(dir.path());
        assert!(read_one_shot(dir.path()).is_none());
        // Second consume is a no-op, not an error.
        consume_one_shot(dir.path());
    }
}
