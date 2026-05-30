//! Generation scanning and active-generation resolution.

use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::nmbl_warn;
use crate::ui::BootReporter;

use super::Generation;
use super::readiness::classify_scan_failure_live;
use super::resolve::{
    mount_aware_resolve, read_kernel_params, read_label, resolve_init_path, resolve_kernel_initrd,
};

/// Parse `system-<N>-link` filenames into `N`. Returns `None` for anything
/// that doesn't match exactly — that directory hosts other entries too.
pub(super) fn parse_generation_number(name: &str) -> Option<u32> {
    name.strip_prefix("system-")?
        .strip_suffix("-link")?
        .parse::<u32>()
        .ok()
}

/// Scan `config.paths.nix_profiles_dir` for `system-*-link` entries and return
/// the matching generations sorted by `number` DESCENDING (newest first).
///
/// When the scan finds nothing, the failure is classified (see
/// [`super::readiness::classify_scan_failure_live`]) into the most
/// specific cause:
///   - [`crate::error::NmblError::SystemRootNotMounted`] — nothing is
///     mounted at the system-root mountpoint;
///   - [`crate::error::NmblError::ProfilesDirMissing`] — a filesystem is
///     mounted but it lacks the `nix/var/nix/profiles` directory (wrong
///     fs / wrong mountpoint, e.g. a bad hand-mount);
///   - [`crate::error::NmblError::NoGenerations`] — the directory exists
///     and was read but holds no `system-N-link` entries.
///
/// `reporter` carries the live boot console; we surface the scan path
/// as the boot-status phase label so the operator sees what's being
/// inspected.
pub fn scan_generations(
    config: &Config,
    reporter: &mut BootReporter<'_, '_>,
) -> Result<Vec<Generation>> {
    let dir = config.paths.nix_profiles_dir.clone();
    let _ = reporter.set_phase(format!(
        "phase 4: scanning generations in {}",
        dir.display()
    ));
    let mount_prefix = config.paths.system_root.as_path();
    let entries = match std::fs::read_dir(&dir) {
        Ok(it) => it,
        Err(err) => {
            nmbl_warn!("cannot read {}: {err}", dir.display());
            return Err(classify_scan_failure_live(&dir, mount_prefix));
        }
    };

    let mut generations: Vec<Generation> = Vec::new();
    for entry in entries.flatten() {
        let file_name_os = entry.file_name();
        let Some(name) = file_name_os.to_str() else {
            continue;
        };
        let Some(number) = parse_generation_number(name) else {
            continue;
        };

        let profile_link = entry.path();
        let toplevel = match mount_aware_resolve(&profile_link, mount_prefix) {
            Ok(p) => p,
            Err(err) => {
                nmbl_warn!(
                    "skipping generation {number} at {}: resolving profile link: {err}",
                    profile_link.display()
                );
                continue;
            }
        };
        let (kernel, initrd) = match resolve_kernel_initrd(&toplevel, mount_prefix) {
            Ok(pair) => pair,
            Err(err) => {
                nmbl_warn!(
                    "skipping generation {number} at {}: {err}",
                    profile_link.display()
                );
                continue;
            }
        };
        let init_path = match resolve_init_path(&profile_link, &toplevel) {
            Ok(p) => p,
            Err(err) => {
                nmbl_warn!(
                    "skipping generation {number} at {} (no init): {err}",
                    profile_link.display()
                );
                continue;
            }
        };

        generations.push(Generation {
            number,
            kernel_params: read_kernel_params(&toplevel),
            label: read_label(&toplevel),
            profile_link,
            kernel,
            initrd,
            init_path,
        });
    }

    if generations.is_empty() {
        // The dir read fine but held no usable `system-N-link` entries;
        // classification keeps this as NoGenerations (dir present) while
        // still routing the missing-dir / not-mounted cases above.
        return Err(classify_scan_failure_live(&dir, mount_prefix));
    }

    // Newest first; the active-profile lookup below maps the operator's
    // currently-selected generation onto the correct slot in this Vec.
    generations.sort_by_key(|g| std::cmp::Reverse(g.number));
    Ok(generations)
}

/// Resolve the index of the generation that `<profiles_dir>/system`
/// currently points at, or `0` (highest-numbered, the historical
/// default) when that pointer cannot be honoured.
///
/// The `system` symlink is what `nixos-rebuild --rollback` flips: the
/// `system-N-link` entries on disk are append-only history, but the
/// `system` pointer marks which of them is active. Selecting purely
/// by max generation number would silently boot the entry the operator
/// just rolled away from.
pub fn active_generation_index(generations: &[Generation], profiles_dir: &Path) -> usize {
    let link = profiles_dir.join("system");
    let target = match std::fs::read_link(&link) {
        Ok(t) => t,
        Err(err) => {
            nmbl_warn!(
                "active generation symlink {} unreadable, falling back to newest: {err}",
                link.display()
            );
            return 0;
        }
    };
    let name = match target.file_name().and_then(|s| s.to_str()) {
        Some(n) => n,
        None => {
            nmbl_warn!(
                "active generation symlink {} target {:?} has no usable filename, falling back to newest",
                link.display(),
                target,
            );
            return 0;
        }
    };
    let Some(number) = parse_generation_number(name) else {
        nmbl_warn!(
            "active generation symlink {} target {:?} does not match system-N-link, falling back to newest",
            link.display(),
            target,
        );
        return 0;
    };
    match generations.iter().position(|g| g.number == number) {
        Some(idx) => idx,
        None => {
            nmbl_warn!(
                "active generation {number} not present in scan (likely filtered for missing init), falling back to newest"
            );
            0
        }
    }
}
