//! Kernel module loading via `finit_module(2)`, with a dep-graph
//! resolver fed by `<modules_dir>/<release>/modules.dep`.
//!
//! This is the moral replacement of the `modprobe` invocation in the
//! bash `mount-and-kernel.sh.nix`. It deliberately does not shell out.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use rustix::fs::{Mode, OFlags};

use crate::error::{NmblError, Result};
use crate::nmbl_warn;

// `<linux/module.h>` flag bits for `finit_module(2)`. `nix` does not
// expose the syscall or the flags, so we hard-code them here.
#[allow(dead_code, reason = "exposed for callers / future use")]
const MODULE_INIT_IGNORE_MODVERSIONS: u32 = 1;
#[allow(dead_code, reason = "exposed for callers / future use")]
const MODULE_INIT_IGNORE_VERMAGIC: u32 = 2;
const MODULE_INIT_COMPRESSED_FILE: u32 = 4;

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
fn canonical_module_name(raw: &str) -> String {
    raw.replace('-', "_")
}

/// Derive the kernel module name from a `.ko[.xz|.zst|.gz]` path.
/// Hyphens in the filename are folded to underscores so that downstream
/// lookups against `/sys/module/<name>` and caller-supplied module names
/// (which conventionally use underscores) resolve regardless of the
/// kernel's own filename spelling.
fn module_name_from_path(path: &Path) -> Option<String> {
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

/// Outcome of a single `finit_module(2)` call.
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
}

/// Errnos returned by `finit_module(2)` that mean "this kernel cannot
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
///
/// `EEXIST` is handled separately as [`LoadOutcome::AlreadyLoaded`] and
/// is NOT routed through this classifier. `ELOOP` (cycle detection from
/// our own dep walk) is intentionally **not** recoverable: it indicates
/// a config/modules.dep bug that should propagate.
pub fn is_recoverable_module_error(errno: Errno) -> bool {
    matches!(
        errno,
        Errno::EOPNOTSUPP | Errno::ENOEXEC | Errno::ENODEV | Errno::ENOSYS | Errno::EINVAL
    )
}

/// Synthesize a `NmblError::Module` from a raw errno + module name + path.
fn module_err(name: &str, path: &Path, errno: Errno) -> NmblError {
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
    let entry = by_name
        .get(name)
        .copied()
        .ok_or_else(|| module_err(name, Path::new(""), Errno::ENOENT))?;
    for dep in &entry.deps {
        visit(dep, by_name, order, visited, visiting)?;
    }
    visiting.remove(name);
    visited.insert(name.to_owned());
    order.push(entry);
    Ok(())
}

/// Pick the right `finit_module` flag bits for a `.ko*` file based on
/// its extension. Compressed variants get `MODULE_INIT_COMPRESSED_FILE`
/// so the kernel decompresses in-place (Linux >= 5.17).
fn flags_for_path(path: &Path) -> u32 {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return 0;
    };
    if name.ends_with(".ko.xz") || name.ends_with(".ko.zst") || name.ends_with(".ko.gz") {
        MODULE_INIT_COMPRESSED_FILE
    } else {
        0
    }
}

/// Open the module file as an owning fd. `rustix::fs::open` returns an
/// `OwnedFd` natively, so we avoid the `from_raw_fd` unsafe wrap that
/// `nix::fcntl::open` would have forced.
fn open_module_fd(path: &Path, name: &str) -> Result<OwnedFd> {
    rustix::fs::open(path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| module_err(name, path, Errno::from_raw(e.raw_os_error())))
}

/// Call `finit_module(fd, params, flags)` directly.
///
/// Returns:
/// * `Ok(LoadOutcome::Loaded)` on success.
/// * `Ok(LoadOutcome::AlreadyLoaded)` when the kernel reports `EEXIST`.
/// * `Ok(LoadOutcome::KernelRefused { source })` for errnos classified
///   as recoverable by [`is_recoverable_module_error`].
/// * `Err(NmblError::Module)` for every other errno — those are real
///   load failures the caller should propagate.
fn finit_module(
    fd: &OwnedFd,
    params: &CString,
    flags: u32,
    name: &str,
    path: &Path,
) -> Result<LoadOutcome> {
    // SAFETY: Unavoidable raw syscall.
    //   * Why no safe wrapper: no Rust crate wraps `finit_module(2)` —
    //     `nix` (0.29) exposes neither the syscall nor its flag bits;
    //     `rustix` 0.38 has no covering API (the `rustix` issue tracker
    //     has no open ticket for it either); `kmod`/`libkmod` is a
    //     C-API binding that would re-introduce a dynamic-library
    //     dependency we explicitly do not want in PID 1.
    //   * Why this is safe: `fd` is a live `OwnedFd` borrowed by
    //     reference for the duration of the call, `params` is a valid
    //     NUL-terminated C string owned by the caller, and `flags` is
    //     a plain integer. The syscall reads, never writes, our
    //     pointers; failure is reported via the return value + errno.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_finit_module,
            fd.as_raw_fd(),
            params.as_ptr(),
            flags,
        )
    };
    if rc == 0 {
        return Ok(LoadOutcome::Loaded);
    }
    let errno = Errno::last();
    if errno == Errno::EEXIST {
        // Module is already in the kernel — treat as success.
        return Ok(LoadOutcome::AlreadyLoaded);
    }
    if is_recoverable_module_error(errno) {
        return Ok(LoadOutcome::KernelRefused { source: errno });
    }
    Err(module_err(name, path, errno))
}

/// Open the module's file at `entry.path` and load it via
/// `finit_module(2)`. Compressed `.ko.xz` / `.ko.zst` / `.ko.gz` are
/// passed with `MODULE_INIT_COMPRESSED_FILE` so the kernel handles
/// decompression.
///
/// File-IO errors (open) and unexpected errnos surface as
/// `NmblError::Module`. Idempotent re-loads return
/// `Ok(LoadOutcome::AlreadyLoaded)`; kernel-side refusals return
/// `Ok(LoadOutcome::KernelRefused { … })` and must be logged + skipped
/// by the caller rather than aborting the boot.
pub fn load_module(entry: &ModuleEntry) -> Result<LoadOutcome> {
    let fd = open_module_fd(&entry.path, &entry.name)?;
    let flags = flags_for_path(&entry.path);
    // `modules.dep` carries no parameters and the bash bootloader never
    // set them — pass an empty string and move on.
    let params = CString::default();
    finit_module(&fd, &params, flags, &entry.name, &entry.path)
}

/// Resolve `name` against `by_name`, then load every entry in load
/// order. Idempotent because individual `load_module` calls report
/// `EEXIST` as [`LoadOutcome::AlreadyLoaded`].
///
/// A kernel-refused dep is logged as a warning and skipped — the parent
/// module will most likely also be refused on the next iteration and
/// the operator will see the cascade. If a downstream phase actually
/// needs the missing module, it will fail with its own (more pointed)
/// error there.
pub fn load_with_deps(name: &str, by_name: &HashMap<String, &ModuleEntry>) -> Result<()> {
    for entry in resolve_load_order(name, by_name)? {
        match load_module(entry)? {
            LoadOutcome::Loaded | LoadOutcome::AlreadyLoaded => {}
            LoadOutcome::KernelRefused { source } => {
                nmbl_warn!(
                    "module {} could not be loaded by the kernel ({}); \
                     continuing — if a downstream phase needs it the \
                     error will be clearer there",
                    entry.name,
                    source
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    fn by(entries: &[ModuleEntry], name: &str) -> ModuleEntry {
        entries
            .iter()
            .find(|e| e.name == name)
            .cloned()
            .expect("entry present")
    }

    #[test]
    fn parses_names_and_deps() {
        let text = "\
kernel/fs/ext4/ext4.ko.xz: kernel/fs/jbd2/jbd2.ko.xz kernel/lib/crc16.ko.xz
kernel/fs/jbd2/jbd2.ko.xz: kernel/lib/crc32c_generic.ko.xz
kernel/lib/crc16.ko.xz:
kernel/lib/crc32c_generic.ko.xz:
";
        let root = PathBuf::from("/lib/modules/6.6.71");
        let entries = parse_modules_dep_text(text, &root);
        assert_eq!(entries.len(), 4);
        let ext4 = by(&entries, "ext4");
        assert_eq!(ext4.path, root.join("kernel/fs/ext4/ext4.ko.xz"));
        assert_eq!(ext4.deps, vec!["jbd2".to_owned(), "crc16".to_owned()]);
        assert!(by(&entries, "crc16").deps.is_empty());
    }

    #[test]
    fn topological_order_is_deepest_first() {
        let text = "\
a.ko: b.ko
b.ko: c.ko
c.ko:
";
        let root = PathBuf::from("/m");
        let entries = parse_modules_dep_text(text, &root);
        let idx = index_by_name(&entries);
        let order = resolve_load_order("a", &idx).expect("resolve failed");
        let names: Vec<&str> = order.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["c", "b", "a"]);
    }

    #[test]
    fn missing_module_errors() {
        let entries: Vec<ModuleEntry> = Vec::new();
        let idx = index_by_name(&entries);
        let err = resolve_load_order("ghost", &idx).expect_err("must error");
        match err {
            NmblError::Module { name, .. } => assert_eq!(name, "ghost"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn cycle_detection_errors() {
        let text = "\
a.ko: b.ko
b.ko: a.ko
";
        let root = PathBuf::from("/m");
        let entries = parse_modules_dep_text(text, &root);
        let idx = index_by_name(&entries);
        let err = resolve_load_order("a", &idx).expect_err("must error");
        match err {
            NmblError::Module { source, .. } => {
                assert_eq!(source, nix::Error::from(Errno::ELOOP));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn parses_hyphenated_filename_as_underscored_name() {
        // The upstream kernel ships `kernel/drivers/md/dm-mod.ko.xz` and
        // `modules.dep` preserves that filename. The parser must fold
        // the hyphen to an underscore so callers asking for `dm_mod`
        // resolve against the same entry.
        let text = "kernel/drivers/md/dm-mod.ko.xz:\n";
        let root = PathBuf::from("/lib/modules/6.6.71");
        let entries = parse_modules_dep_text(text, &root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "dm_mod");
        assert_eq!(entries[0].path, root.join("kernel/drivers/md/dm-mod.ko.xz"));
    }

    #[test]
    fn parses_hyphenated_dep_name_as_underscored() {
        // Deps in `modules.dep` are also expressed as on-disk paths, so
        // the same hyphen-fold rule must apply to dependency names.
        let text = "\
kernel/foo/parent.ko.xz: kernel/drivers/md/dm-mod.ko.xz
kernel/drivers/md/dm-mod.ko.xz:
";
        let root = PathBuf::from("/m");
        let entries = parse_modules_dep_text(text, &root);
        let parent = by(&entries, "parent");
        assert_eq!(parent.deps, vec!["dm_mod".to_owned()]);
    }

    #[test]
    fn resolve_underscore_query_against_hyphenated_entry() {
        // Caller passes `dm_mod` (the conventional spelling, e.g. from
        // boot.nmbl.kernelModules or the activation orchestrator), but
        // the on-disk filename is `dm-mod.ko.xz`. The query must
        // resolve, matching modprobe's behaviour.
        let text = "kernel/drivers/md/dm-mod.ko.xz:\n";
        let root = PathBuf::from("/lib/modules/6.6.71");
        let entries = parse_modules_dep_text(text, &root);
        let idx = index_by_name(&entries);
        let order = resolve_load_order("dm_mod", &idx).expect("resolve failed");
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].name, "dm_mod");
    }

    #[test]
    fn resolve_hyphen_query_against_hyphenated_entry() {
        // The reverse direction: caller passes the hyphen spelling,
        // still must resolve.
        let text = "kernel/drivers/md/dm-mod.ko.xz:\n";
        let root = PathBuf::from("/lib/modules/6.6.71");
        let entries = parse_modules_dep_text(text, &root);
        let idx = index_by_name(&entries);
        let order = resolve_load_order("dm-mod", &idx).expect("resolve failed");
        assert_eq!(order.len(), 1);
        assert_eq!(order[0].name, "dm_mod");
    }

    #[test]
    fn recoverable_classifier_covers_kernel_refusals() {
        // Every errno that the task / `init_module(2)` manpage flags as
        // "kernel cannot load this module right now" must be classified
        // as recoverable so we don't abort the boot for it.
        for errno in [
            Errno::EOPNOTSUPP,
            Errno::ENOEXEC,
            Errno::ENODEV,
            Errno::ENOSYS,
            Errno::EINVAL,
        ] {
            assert!(
                is_recoverable_module_error(errno),
                "{errno:?} should be recoverable"
            );
        }
    }

    #[test]
    fn recoverable_classifier_does_not_swallow_real_errors() {
        // File-IO failures (ENOENT, EACCES, …) and dep-graph bugs
        // (ELOOP) must NOT be classified as recoverable — they need to
        // surface as `NmblError::Module`. EEXIST is also excluded
        // because it has its own `LoadOutcome::AlreadyLoaded` variant
        // and never reaches the classifier.
        for errno in [
            Errno::ENOENT,
            Errno::EACCES,
            Errno::EPERM,
            Errno::ELOOP,
            Errno::EEXIST,
            Errno::ENOMEM,
            Errno::EIO,
        ] {
            assert!(
                !is_recoverable_module_error(errno),
                "{errno:?} must NOT be recoverable"
            );
        }
    }
}
