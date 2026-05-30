//! Explicit kernel-module loader.
//!
//! Replaces the `for module in $explicit_modules; do modprobe ...` loop
//! in `scripts/mount-and-kernel.sh.nix`. Loads every module named in the
//! selected list, plus each module's transitive dependencies, via
//! `sys::module`. Blacklisted names are skipped (blacklist wins).
//!
//! The loader is split across two phases. Graphics drivers
//! (`virtio_gpu`, `simpledrm`, `i915`, …) must be available BEFORE
//! `open_console` so the splash backend can attach to
//! `/dev/dri/card*` — those go into [`ModuleSet::Early`] and are
//! loaded in phase 2a, before the console is brought up. Storage /
//! filesystem / activation drivers go into [`ModuleSet::Explicit`] and
//! load in phase 2b, after the console is up so per-module progress
//! is visible to the operator.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::sys::module::{self, LoadOutcome, ModuleEntry, canonical_module_name};
use crate::sys::ops::ModuleOps;
use crate::ui::BootReporter;
use crate::{nmbl_info, nmbl_verbose, nmbl_warn};

/// Which subset of `config.kernel_modules` a single load pass walks.
///
/// The orchestrator runs phase 2a with [`ModuleSet::Early`] before
/// `open_console`, and phase 2b with [`ModuleSet::Explicit`] after.
/// The blacklist is shared between both passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleSet {
    /// Pre-console drivers — graphics stack so the splash backend can
    /// attach to `/dev/dri/card*`. Reads `config.kernel_modules.early`.
    Early,
    /// Post-console drivers — storage, filesystem, activation. Reads
    /// `config.kernel_modules.explicit`.
    Explicit,
}

impl ModuleSet {
    /// Borrow the matching list out of the config.
    fn module_list(self, config: &Config) -> &[String] {
        match self {
            ModuleSet::Early => &config.kernel_modules.early,
            ModuleSet::Explicit => &config.kernel_modules.explicit,
        }
    }

    /// Human-readable label for log messages and the boot-status phase
    /// string. Matches the run_phases narration.
    fn phase_label(self) -> &'static str {
        match self {
            ModuleSet::Early => "phase 2a: loading early kernel modules",
            ModuleSet::Explicit => "phase 2b: loading kernel modules",
        }
    }

    /// Phase prefix for the per-module spinner label, e.g. `"phase 2a"`.
    fn modprobe_prefix(self) -> &'static str {
        match self {
            ModuleSet::Early => "phase 2a",
            ModuleSet::Explicit => "phase 2b",
        }
    }
}

/// Load the [`ModuleSet::Early`] subset — graphics drivers, etc. Called
/// in phase 2a before `open_console` so the splash backend has a DRM
/// card to attach to.
///
/// The pre-console reporter wraps a [`crate::ui::console::NoopConsole`];
/// status pushes do nothing visible, but the underlying log-ring is
/// still populated for the post-console reporter to surface.
pub fn load_early_modules(
    ops: &mut impl ModuleOps,
    config: &Config,
    reporter: &mut BootReporter<'_, '_>,
) -> Result<()> {
    ops.load_module_set(config, reporter, ModuleSet::Early)
}

/// Load the [`ModuleSet::Explicit`] subset — storage, filesystem,
/// activation drivers. Called in phase 2b after `open_console` so the
/// operator sees per-module progress on the live boot console.
pub fn load_explicit_modules(
    ops: &mut impl ModuleOps,
    config: &Config,
    reporter: &mut BootReporter<'_, '_>,
) -> Result<()> {
    ops.load_module_set(config, reporter, ModuleSet::Explicit)
}

/// Walk the chosen [`ModuleSet`] list, loading each entry + its
/// transitive deps via `sys::module`. Blacklisted module names
/// (top-level or transitive) are skipped with a log line; a blacklisted
/// dep is a config inconsistency and gets a warning.
///
/// `reporter` carries either the live boot console (phase 2b) or the
/// pre-console `NoopConsole` (phase 2a); the call sequence is identical
/// in both phases — only the visible side-effect differs.
pub fn load_module_set(
    config: &Config,
    reporter: &mut BootReporter<'_, '_>,
    which: ModuleSet,
) -> Result<()> {
    let _ = reporter.set_phase(which.phase_label());
    let module_list = which.module_list(config);
    // Cheap fast path: skip the modules.dep parse when the list is empty.
    // This also matters in phase 2a, where the early list is often empty
    // on platforms with built-in graphics drivers (KVM with simpledrm-only,
    // bare-metal with i915 built into the kernel, etc.).
    if module_list.is_empty() {
        nmbl_verbose!("no modules requested for {:?}; skipping", which);
        return Ok(());
    }

    load_modules_inner(
        &config.kernel_modules.modules_dir,
        module_list,
        &config.kernel_modules.blacklist,
        Some((reporter, which.modprobe_prefix())),
    )?;
    Ok(())
}

/// Lower-level loader used by the bootstrap stage (Phase 0.5), which
/// only has a tiny explicit list and no blacklist. Reporter-free so the
/// bootstrap path can call it before the live console is open and
/// before the full [`Config`] is even loaded.
pub fn load_modules(modules_dir: &Path, explicit: &[String], blacklist: &[String]) -> Result<()> {
    load_modules_inner(modules_dir, explicit, blacklist, None)
}

/// Shared core for both [`load_module_set`] (post-console, with
/// reporter) and [`load_modules`] (pre-console, no reporter). When
/// `reporter_ctx` is `Some`, each top-level module pushes a
/// `"<prefix>: modprobe <name>"` status frame so the operator sees
/// per-module progress.
fn load_modules_inner(
    modules_dir: &Path,
    explicit: &[String],
    blacklist: &[String],
    mut reporter_ctx: Option<(&mut BootReporter<'_, '_>, &'static str)>,
) -> Result<()> {
    let release = crate::sys::uname::kernel_release()?;
    let entries = module::load_modules_dep(modules_dir, &release)?;
    let by_name: HashMap<String, &ModuleEntry> = module::index_by_name(&entries);
    // Canonicalize the blacklist so a config entry `dm-crypt` and a
    // request for `dm_crypt` (or vice versa) match consistently.
    let blacklist_canonical: HashSet<String> =
        blacklist.iter().map(|b| canonical_module_name(b)).collect();
    let blacklist: HashSet<&str> = blacklist_canonical.iter().map(String::as_str).collect();

    let mut loaded: usize = 0;
    for name in explicit {
        if let Some((reporter, prefix)) = reporter_ctx.as_mut() {
            let _ = reporter.set_phase(format!("{prefix}: modprobe {name}"));
        }
        // Operator-supplied names may use hyphens (`dm-crypt`), but the
        // modules tree is keyed in canonical underscore form
        // (`dm_crypt`); both the blacklist and the by-name index live in
        // the canonical namespace, so fold the lookup key before either
        // membership check or the load order resolves a stale negative.
        let canonical = canonical_module_name(name);
        if blacklist.contains(canonical.as_str()) {
            nmbl_verbose!("skipping blacklisted module {}", name);
            continue;
        }
        // Skip entries that don't exist in the modules tree. Modern
        // kernels fold many former crypto modules (ecb, xts, sha256_generic,
        // and friends) into the built-in libcrypto, so the .ko file is
        // simply absent. A missing top-level entry is not an error — if
        // a downstream phase genuinely needs the module the kernel will
        // surface that with a clearer failure.
        if !by_name.contains_key(canonical.as_str()) {
            nmbl_verbose!("module {} not in modules tree; assuming built-in", name);
            continue;
        }
        let order = module::resolve_load_order(name, &by_name)?;
        let (to_load, skipped) = filter_blacklisted(order, &blacklist);
        for skipped_name in skipped {
            nmbl_warn!(
                "module {} requested but its dependency {} is blacklisted",
                name,
                skipped_name
            );
        }
        for entry in to_load {
            match module::load_module(entry)? {
                LoadOutcome::Loaded | LoadOutcome::AlreadyLoaded => {
                    loaded += 1;
                }
                LoadOutcome::KernelRefused { source } => {
                    nmbl_warn!(
                        "module {} could not be loaded by the kernel ({}); \
                         continuing — if a downstream phase needs it the \
                         error will be clearer there",
                        entry.name,
                        source
                    );
                }
                LoadOutcome::FileMissing => {
                    nmbl_warn!(
                        "module {} listed in modules.dep but {} is not in the \
                         initrd; skipping (closure was likely shrunk with \
                         allowMissing=true)",
                        entry.name,
                        entry.path.display()
                    );
                }
            }
        }
    }

    nmbl_info!("loaded {} modules", loaded);
    Ok(())
}

/// Split a resolved load order into (to_load, skipped_names). Entries
/// whose name is in `blacklist` are removed; their names are returned
/// so the caller can log them.
pub(crate) fn filter_blacklisted<'a>(
    load_order: Vec<&'a ModuleEntry>,
    blacklist: &HashSet<&str>,
) -> (Vec<&'a ModuleEntry>, Vec<&'a str>) {
    let mut to_load: Vec<&'a ModuleEntry> = Vec::with_capacity(load_order.len());
    let mut skipped: Vec<&'a str> = Vec::new();
    for entry in load_order {
        if blacklist.contains(entry.name.as_str()) {
            skipped.push(entry.name.as_str());
        } else {
            to_load.push(entry);
        }
    }
    (to_load, skipped)
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn entry(name: &str) -> ModuleEntry {
        ModuleEntry {
            name: name.to_owned(),
            path: PathBuf::from(format!("/m/{name}.ko")),
            deps: Vec::new(),
        }
    }

    #[test]
    fn filter_passes_everything_through_empty_blacklist() {
        let a = entry("a");
        let b = entry("b");
        let order = vec![&a, &b];
        let blacklist: HashSet<&str> = HashSet::new();
        let (to_load, skipped) = filter_blacklisted(order, &blacklist);
        assert_eq!(to_load.len(), 2);
        assert!(skipped.is_empty());
    }

    #[test]
    fn filter_drops_blacklisted_and_reports_names() {
        let a = entry("a");
        let b = entry("b");
        let c = entry("c");
        let order = vec![&a, &b, &c];
        let mut blacklist: HashSet<&str> = HashSet::new();
        blacklist.insert("b");
        let (to_load, skipped) = filter_blacklisted(order, &blacklist);
        let names: Vec<&str> = to_load.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a", "c"]);
        assert_eq!(skipped, vec!["b"]);
    }

    #[test]
    fn filter_drops_all_if_all_blacklisted() {
        let a = entry("a");
        let b = entry("b");
        let order = vec![&a, &b];
        let mut blacklist: HashSet<&str> = HashSet::new();
        blacklist.insert("a");
        blacklist.insert("b");
        let (to_load, skipped) = filter_blacklisted(order, &blacklist);
        assert!(to_load.is_empty());
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn module_set_early_reads_early_list() {
        let mut config = Config::recovery_default();
        config.kernel_modules.early = vec!["fake_a".to_owned()];
        config.kernel_modules.explicit = vec!["fake_b".to_owned()];
        let early_list = ModuleSet::Early.module_list(&config);
        assert_eq!(early_list, &["fake_a".to_owned()]);
    }

    #[test]
    fn module_set_explicit_reads_explicit_list() {
        let mut config = Config::recovery_default();
        config.kernel_modules.early = vec!["fake_a".to_owned()];
        config.kernel_modules.explicit = vec!["fake_b".to_owned()];
        let explicit_list = ModuleSet::Explicit.module_list(&config);
        assert_eq!(explicit_list, &["fake_b".to_owned()]);
    }

    #[test]
    fn module_set_early_and_explicit_are_disjoint_in_picker() {
        // Synthetic config — confirm `load_early_modules` selects the
        // early list and `load_explicit_modules` selects the explicit
        // list, and the two lists do not bleed into each other through
        // the dispatcher. Mirrors the production split: phase 2a loads
        // virtio_gpu/virtio_pci; phase 2b loads ext4/nvme.
        let mut config = Config::recovery_default();
        config.kernel_modules.early = vec!["virtio_pci".to_owned(), "virtio_gpu".to_owned()];
        config.kernel_modules.explicit = vec!["ext4".to_owned(), "nvme".to_owned()];

        let early = ModuleSet::Early.module_list(&config);
        let explicit = ModuleSet::Explicit.module_list(&config);

        assert_eq!(early, &["virtio_pci".to_owned(), "virtio_gpu".to_owned()]);
        assert_eq!(explicit, &["ext4".to_owned(), "nvme".to_owned()]);

        // Each list contains exactly the modules the operator asked
        // for in that phase; nothing crosses the divide.
        for early_name in early {
            assert!(
                !explicit.iter().any(|m| m == early_name),
                "early module {early_name} must not leak into explicit list",
            );
        }
        for explicit_name in explicit {
            assert!(
                !early.iter().any(|m| m == explicit_name),
                "explicit module {explicit_name} must not leak into early list",
            );
        }
    }

    #[test]
    fn module_set_labels_are_distinct() {
        // Phase labels surface to the operator via the boot-status
        // screen; mixing them would confuse "what is the boot doing".
        assert_ne!(
            ModuleSet::Early.phase_label(),
            ModuleSet::Explicit.phase_label(),
        );
        assert!(ModuleSet::Early.phase_label().contains("2a"));
        assert!(ModuleSet::Explicit.phase_label().contains("2b"));
        assert_eq!(ModuleSet::Early.modprobe_prefix(), "phase 2a");
        assert_eq!(ModuleSet::Explicit.modprobe_prefix(), "phase 2b");
    }

    /// Mirror of the `match module::load_module(entry)?` arm used by
    /// `load_explicit_modules`. The point of this test is to lock in
    /// the contract: a `LoadOutcome::KernelRefused` value MUST flow
    /// through the routing logic without producing an `Err`, otherwise
    /// the boot will abort the way it did pre-fix for `dax.ko.xz`.
    ///
    /// We can't drive `module::load_module` directly without an actual
    /// `.ko` file and a kernel that refuses it, so this test exercises
    /// the dispatch shape on synthetic outcomes — the same shape that
    /// `load_explicit_modules` uses.
    #[test]
    fn kernel_refused_outcome_does_not_abort_dispatch() {
        use nix::errno::Errno;

        let outcomes = vec![
            crate::sys::module::LoadOutcome::Loaded,
            crate::sys::module::LoadOutcome::AlreadyLoaded,
            crate::sys::module::LoadOutcome::KernelRefused {
                source: Errno::EOPNOTSUPP,
            },
            crate::sys::module::LoadOutcome::FileMissing,
            crate::sys::module::LoadOutcome::Loaded,
        ];

        let mut loaded: usize = 0;
        let mut refused: usize = 0;
        let mut missing: usize = 0;
        // This match must stay in lock-step with `load_explicit_modules`.
        for outcome in outcomes {
            match outcome {
                crate::sys::module::LoadOutcome::Loaded
                | crate::sys::module::LoadOutcome::AlreadyLoaded => {
                    loaded += 1;
                }
                crate::sys::module::LoadOutcome::KernelRefused { source: _ } => {
                    refused += 1;
                }
                crate::sys::module::LoadOutcome::FileMissing => {
                    missing += 1;
                }
            }
        }
        assert_eq!(loaded, 3);
        assert_eq!(refused, 1);
        assert_eq!(missing, 1);
    }
}
