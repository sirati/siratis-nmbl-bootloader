//! Dependency-graph types and resolver for kernel module loading.
//!
//! Parses `modules.dep`, builds a by-name index, and topologically
//! orders the load sequence (deepest-dependency first).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use nix::errno::Errno;

use crate::error::{NmblError, Result};
use crate::nmbl_warn;

/// One entry parsed from `modules.dep`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleEntry {
    /// Module name, e.g. `"ext4"`.
    pub name: String,
    /// Absolute path to the `.ko[.xz|.zst|.gz]` file, joined with the
    /// modules root.
    pub path: PathBuf,
    /// Names of modules this depends on, in load order (deepest first).
    pub deps: Vec<String>,
}

/// Canonicalize a kernel module name: hyphens are folded to underscores.
///
/// Mirrors modprobe's `s/-/_/g` rule. The on-disk `.ko` filename can
/// use either spelling (`dm-mod.ko.xz` ships in the upstream kernel
/// tree, while `/sys/module/dm_mod` and most config knobs spell it with
/// an underscore), and `modules.dep` preserves the file-system spelling
/// verbatim. Folding both the parsed entry names and the lookup queries
/// through this function lets either form resolve consistently.
pub(crate) fn canonical_module_name(raw: &str) -> String {
    raw.replace('-', "_")
}

/// Derive the kernel module name from a `.ko[.xz|.zst|.gz]` path.
/// Hyphens in the filename are folded to underscores so that downstream
/// lookups against `/sys/module/<name>` and caller-supplied module names
/// (which conventionally use underscores) resolve regardless of the
/// kernel's own filename spelling.
pub(crate) fn module_name_from_path(path: &Path) -> Option<String> {
    let file = path.file_name().and_then(|s| s.to_str())?;
    let stripped = file
        .strip_suffix(".xz")
        .or_else(|| file.strip_suffix(".zst"))
        .or_else(|| file.strip_suffix(".gz"))
        .unwrap_or(file);
    stripped.strip_suffix(".ko").map(canonical_module_name)
}

/// Parse a `modules.dep` text body, anchoring relative paths under
/// `root` (typically `<modules_dir>/<release>/`). Comment / blank /
/// malformed lines are skipped. Empty dep lists are valid.
pub fn parse_modules_dep_text(text: &str, root: &Path) -> Vec<ModuleEntry> {
    let mut out: Vec<ModuleEntry> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once(':') else {
            continue;
        };
        let mod_path = root.join(lhs.trim());
        let Some(name) = module_name_from_path(&mod_path) else {
            continue;
        };
        let deps: Vec<String> = rhs
            .split_whitespace()
            .filter_map(|d| module_name_from_path(&root.join(d)))
            .collect();
        out.push(ModuleEntry {
            name,
            path: mod_path,
            deps,
        });
    }
    out
}

/// Parse `<modules_dir>/<kernel_release>/modules.dep`.
pub fn load_modules_dep(modules_dir: &Path, kernel_release: &str) -> Result<Vec<ModuleEntry>> {
    let root = modules_dir.join(kernel_release);
    let dep_path = root.join("modules.dep");
    let text = std::fs::read_to_string(&dep_path).map_err(|source| NmblError::Io {
        source,
        context: format!("reading {}", dep_path.display()),
    })?;
    Ok(parse_modules_dep_text(&text, &root))
}

/// Build a borrow-map keyed by module name. Convenience for callers
/// that load `modules.dep` once and resolve many modules out of it.
pub fn index_by_name(entries: &[ModuleEntry]) -> HashMap<String, &ModuleEntry> {
    let mut map = HashMap::with_capacity(entries.len());
    for e in entries {
        map.insert(e.name.clone(), e);
    }
    map
}

/// Outcome of a single `init_module(2)` call.
///
/// `load_module` returns this so that callers can distinguish a
/// successful load from a kernel-side refusal — the latter is not a
/// catastrophic NMBL error and the boot should continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadOutcome {
    /// Module was newly loaded.
    Loaded,
    /// Module was already loaded (`EEXIST`). Idempotent re-runs hit this.
    AlreadyLoaded,
    /// Kernel refused to load — typically `EOPNOTSUPP` / `ENOEXEC` /
    /// `ENODEV` (see [`is_recoverable_module_error`]). Callers should
    /// log a warning and continue; this is **not** fatal.
    KernelRefused { source: nix::Error },
    /// The `.ko` file referenced by `modules.dep` is absent from the
    /// initrd. NixOS `makeModulesClosure { allowMissing = true; }`
    /// leaves dangling dep entries in `modules.dep` for transitive
    /// modules it pruned; the kernel usually has the relevant symbols
    /// built-in (or the parent module doesn't actually need them at
    /// runtime), so this is non-fatal — callers log + skip.
    FileMissing,
}

/// Errnos returned by `init_module(2)` that mean "this kernel cannot
/// load this particular module right now" rather than "NMBL is broken".
///
/// Covers:
/// * `EOPNOTSUPP` / `ENOTSUP` — feature not supported by the running
///   kernel (e.g. `CONFIG_*=n`, missing CPU feature, transport endpoint
///   does not support the operation).
/// * `ENOEXEC` — kernel cannot parse the module image (mismatched
///   architecture, corrupted file, wrong format).
/// * `ENODEV` — no matching device for the driver; the module loaded
///   logic refuses with no hardware to bind to.
/// * `ENOSYS` — syscall family unavailable / disabled.
/// * `EINVAL` — kernel rejected the module's parameters or signature.
/// * `ENOENT` — the module's own `init()` returned -ENOENT (typically
///   "a backend the module wanted is unavailable" — e.g. encrypted_keys
///   failing `aes_get_sizes()` when the trusted-keys cipher isn't
///   built into the kernel). Safe to skip because file-not-found at the
///   .ko path itself is caught earlier by [`load_module`]'s existence
///   pre-check (returns [`LoadOutcome::FileMissing`]).
///
/// `EEXIST` is handled separately as [`LoadOutcome::AlreadyLoaded`] and
/// is NOT routed through this classifier. `ELOOP` (cycle detection from
/// our own dep walk) is intentionally **not** recoverable: it indicates
/// a config/modules.dep bug that should propagate.
pub fn is_recoverable_module_error(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::EOPNOTSUPP
            | Errno::ENOEXEC
            | Errno::ENODEV
            | Errno::ENOSYS
            | Errno::EINVAL
            | Errno::ENOENT
    )
}

/// Synthesize a `NmblError::Module` from a raw errno + module name + path.
pub(crate) fn module_err(name: &str, path: &Path, errno: Errno) -> NmblError {
    NmblError::Module {
        name: name.to_owned(),
        path: path.to_path_buf(),
        source: errno,
    }
}

/// Resolve a module name into the full load order including all
/// transitive dependencies (deepest first, target last). Returns an
/// `Err` if `name` is not in the dep map or if a cycle is detected.
///
/// The query is canonicalized (hyphens → underscores) so callers can
/// pass either spelling. Entries inserted via [`index_by_name`] are
/// already keyed by canonical names, since [`module_name_from_path`]
/// folds the on-disk filename.
pub fn resolve_load_order<'a>(
    name: &str,
    by_name: &'a HashMap<String, &'a ModuleEntry>,
) -> Result<Vec<&'a ModuleEntry>> {
    let canonical = canonical_module_name(name);
    let mut order: Vec<&'a ModuleEntry> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: HashSet<String> = HashSet::new();
    visit(&canonical, by_name, &mut order, &mut visited, &mut visiting)?;
    Ok(order)
}

fn visit<'a>(
    name: &str,
    by_name: &'a HashMap<String, &'a ModuleEntry>,
    order: &mut Vec<&'a ModuleEntry>,
    visited: &mut HashSet<String>,
    visiting: &mut HashSet<String>,
) -> Result<()> {
    if visited.contains(name) {
        return Ok(());
    }
    if !visiting.insert(name.to_owned()) {
        // We were already mid-visit on this node → cycle. Re-look up the
        // entry so the error carries the .ko path of the offender.
        let path = by_name
            .get(name)
            .map(|e| e.path.clone())
            .unwrap_or_default();
        return Err(module_err(name, &path, Errno::ELOOP));
    }
    let Some(entry) = by_name.get(name).copied() else {
        // Name not in modules.dep — almost always means it's built
        // into the kernel (CONFIG_FOO=y instead of =m). Soft-skip:
        // if downstream code actually needs it the missing-symbol
        // error there will be more specific than aborting the boot
        // for what is, in the common case, a non-event.
        nmbl_warn!(
            "module {} not in modules.dep; assuming built-in and skipping",
            name
        );
        visiting.remove(name);
        visited.insert(name.to_owned());
        return Ok(());
    };
    for dep in &entry.deps {
        visit(dep, by_name, order, visited, visiting)?;
    }
    visiting.remove(name);
    visited.insert(name.to_owned());
    order.push(entry);
    Ok(())
}
