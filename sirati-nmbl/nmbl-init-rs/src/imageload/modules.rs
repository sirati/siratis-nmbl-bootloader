//! Load a driver image's modules (#23, step 4) — REUSES the module loader.
//!
//! The squashfs is a `makeModulesClosure`-shaped tree: its kernel modules live
//! at `<mountpoint>/lib/modules/<release>/…` with a `modules.dep`, exactly the
//! layout [`crate::modules::load_modules`] already drives. So this module does
//! NOT reimplement any module loading — it points the EXISTING reporter-free
//! loader at the image's modules tree and hands it the spec's ordered module
//! list + per-image blacklist.
//!
//! The blacklist is the operator's list of in-tree modules to keep out of the
//! way (e.g. `nouveau` before an NVIDIA image); `load_modules` already skips
//! blacklisted names (top-level and transitive), so the conflict-avoidance
//! semantics come for free from the shared loader.

use std::path::Path;

use crate::config::{Config, DriverImageSpec};
use crate::error::{NmblError, Result};

/// Subdirectory of the mounted image that holds the kernel modules tree, i.e.
/// `<mountpoint>/lib/modules` (so `<…>/lib/modules/<release>/modules.dep`
/// resolves the way `load_modules_dep` expects).
const IMAGE_MODULES_SUBDIR: &str = "lib/modules";

/// Load `spec.modules` (in order) from the mounted driver image at
/// `mountpoint`, honouring `spec.blacklist`.
///
/// Delegates straight to [`crate::modules::load_modules`] against
/// `<mountpoint>/lib/modules`; no module-loading logic is duplicated here. The
/// declared modules are built against the running kernel release, so the
/// loader's internal `<modules_dir>/<release>/modules.dep` lookup resolves
/// against the image's own tree.
///
/// # Errors
/// [`NmblError::DriverImage`] (`stage = "load"`) wrapping the loader error
/// (e.g. a missing `modules.dep`, an unresolvable dependency). Per-module
/// kernel refusals / already-loaded / file-missing are handled non-fatally
/// INSIDE the shared loader, so they do not surface here.
pub(super) fn load_image_modules(
    config: &Config,
    spec: &DriverImageSpec,
    mountpoint: &Path,
) -> Result<()> {
    if spec.modules.is_empty() {
        // A signed image with no declared modules is valid but inert (e.g. it
        // ships firmware only); nothing to load.
        return Ok(());
    }

    let modules_dir = mountpoint.join(IMAGE_MODULES_SUBDIR);

    // The per-image blacklist plus any global blacklist: a driver image's
    // blacklist names in-tree modules to keep out of the way, and the global
    // boot blacklist still applies. Concatenate so the shared loader sees both.
    let mut blacklist: Vec<String> = config.kernel_modules.blacklist.clone();
    blacklist.extend(spec.blacklist.iter().cloned());

    crate::modules::load_modules(&modules_dir, &spec.modules, &blacklist).map_err(|source| {
        NmblError::DriverImage {
            stage: "load",
            source: Box::new(source),
        }
    })
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
    use std::path::PathBuf;

    fn spec(modules: &[&str], blacklist: &[&str]) -> DriverImageSpec {
        DriverImageSpec {
            path: PathBuf::from("nmbl/d.sfs"),
            sig_path: PathBuf::from("nmbl/d.sfs.sig"),
            modules: modules.iter().map(|s| s.to_string()).collect(),
            blacklist: blacklist.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn empty_modules_list_is_a_noop_ok() {
        // A firmware-only image declares no modules; load is a clean no-op even
        // with a bogus mountpoint (the loader is never invoked).
        let cfg = Config::recovery_default();
        load_image_modules(&cfg, &spec(&[], &[]), Path::new("/nonexistent"))
            .expect("empty modules list loads nothing");
    }

    #[test]
    fn missing_modules_dep_is_a_load_stage_error() {
        // A non-empty module list against a mountpoint with no modules.dep must
        // surface as a `load`-stage DriverImage error (the loader's read of
        // <dir>/<release>/modules.dep fails).
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = Config::recovery_default();
        let err = load_image_modules(&cfg, &spec(&["nvidia"], &["nouveau"]), dir.path())
            .expect_err("missing modules.dep must error");
        match err {
            NmblError::DriverImage { stage, .. } => assert_eq!(stage, "load"),
            other => panic!("expected DriverImage(load), got {other:?}"),
        }
    }
}
