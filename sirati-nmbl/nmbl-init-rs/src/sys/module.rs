//! Kernel module loading via `init_module(2)`, with a dep-graph
//! resolver fed by `<modules_dir>/<release>/modules.dep`.
//!
//! This is the moral replacement of the `modprobe` invocation in the
//! bash `mount-and-kernel.sh.nix`. It deliberately does not shell out.
//!
//! ## Why `init_module(2)` instead of `finit_module(2)`?
//!
//! The kernel-side `MODULE_INIT_COMPRESSED_FILE` flag (passed to
//! `finit_module`) only works when the running kernel was built with
//! `CONFIG_MODULE_DECOMPRESS=y`. NixOS kernels do **not** enable that
//! option; userspace (`kmod`) is expected to decompress modules before
//! handing them to the kernel. Passing the flag against such a kernel
//! results in `EOPNOTSUPP` on every load and the boot can never
//! progress past phase 3a. We therefore decompress `.ko.xz` /
//! `.ko.zst` / `.ko.gz` in-process with pure-Rust crates and call the
//! raw `init_module(2)` syscall with the resulting bytes.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io::Read;
use std::os::raw::{c_ulong, c_void};
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

/// Compression scheme of a `.ko*` file, inferred from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// No compression — raw ELF (`.ko`).
    None,
    /// XZ / LZMA2 (`.ko.xz`).
    Xz,
    /// Zstandard (`.ko.zst`).
    Zst,
    /// gzip / DEFLATE (`.ko.gz`).
    Gz,
}

/// Classify a `.ko*` path by compression suffix. Paths whose
/// `file_name()` is not UTF-8 or whose extension is unrecognised fall
/// back to `Compression::None` — `decompress_module` will then try to
/// load the file verbatim, and the kernel will reject it with
/// `ENOEXEC` if it is not a valid ELF.
fn compression_for_path(path: &Path) -> Compression {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return Compression::None;
    };
    if name.ends_with(".ko.xz") {
        Compression::Xz
    } else if name.ends_with(".ko.zst") {
        Compression::Zst
    } else if name.ends_with(".ko.gz") {
        Compression::Gz
    } else {
        Compression::None
    }
}

/// Read `path` from disk and decompress it according to the suffix
/// reported by [`compression_for_path`]. Returns the raw module image
/// suitable for `init_module(2)`.
///
/// All compression backends are pure Rust — `lzma-rs` for XZ, `ruzstd`
/// for Zstandard, `flate2`'s `rust_backend` (which is `miniz_oxide`
/// under the hood) for gzip — so the static-musl build stays free of
/// C library dependencies.
///
/// Decompression failures are surfaced as `NmblError::Module { …,
/// source: Errno::EIO }` (we have no errno for "userspace
/// decompressor blew up"); a `nmbl_warn!` immediately precedes the
/// return so the operator sees the real decompressor message.
fn decompress_module(path: &Path, name: &str) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).map_err(|source| NmblError::Io {
        source,
        context: format!("reading kernel module {}", path.display()),
    })?;
    match compression_for_path(path) {
        Compression::None => Ok(raw),
        Compression::Xz => {
            // `lzma_rs::xz_decompress` wants a `BufRead`; wrap the
            // owned `Vec<u8>` in a `Cursor` (which is `BufRead`).
            let mut reader = std::io::Cursor::new(&raw);
            let mut out = Vec::with_capacity(raw.len() * 3);
            lzma_rs::xz_decompress(&mut reader, &mut out).map_err(|e| {
                nmbl_warn!(
                    "xz decompression failed for module {} ({}): {}",
                    name,
                    path.display(),
                    e
                );
                module_err(name, path, Errno::EIO)
            })?;
            Ok(out)
        }
        Compression::Zst => {
            let mut decoder = ruzstd::StreamingDecoder::new(raw.as_slice()).map_err(|e| {
                nmbl_warn!(
                    "zstd frame init failed for module {} ({}): {}",
                    name,
                    path.display(),
                    e
                );
                module_err(name, path, Errno::EIO)
            })?;
            let mut out = Vec::with_capacity(raw.len() * 3);
            decoder.read_to_end(&mut out).map_err(|e| {
                nmbl_warn!(
                    "zstd decompression failed for module {} ({}): {}",
                    name,
                    path.display(),
                    e
                );
                module_err(name, path, Errno::EIO)
            })?;
            Ok(out)
        }
        Compression::Gz => {
            let mut decoder = flate2::read::GzDecoder::new(raw.as_slice());
            let mut out = Vec::with_capacity(raw.len() * 3);
            decoder.read_to_end(&mut out).map_err(|e| {
                nmbl_warn!(
                    "gzip decompression failed for module {} ({}): {}",
                    name,
                    path.display(),
                    e
                );
                module_err(name, path, Errno::EIO)
            })?;
            Ok(out)
        }
    }
}

/// Call `init_module(image, len, params)` — the original
/// (non-`f`-prefixed) module-load syscall — on a raw module image.
///
/// We use this rather than `finit_module(2)` because NixOS kernels are
/// not built with `CONFIG_MODULE_DECOMPRESS=y`; userspace must
/// decompress (`decompress_module`) and then pass the resulting ELF
/// bytes here. See module-level docs for the full rationale.
///
/// Returns:
/// * `Ok(LoadOutcome::Loaded)` on success.
/// * `Ok(LoadOutcome::AlreadyLoaded)` when the kernel reports `EEXIST`.
/// * `Ok(LoadOutcome::KernelRefused { source })` for errnos classified
///   as recoverable by [`is_recoverable_module_error`].
/// * `Err(NmblError::Module)` for every other errno.
fn init_module(image: &[u8], params: &CString, name: &str, path: &Path) -> Result<LoadOutcome> {
    // SAFETY: Unavoidable raw syscall.
    //   * Why no safe wrapper: no Rust crate wraps `init_module(2)` —
    //     `nix` (0.29) exposes neither this nor `finit_module`;
    //     `rustix` 0.38 has no covering API; `libkmod` is a C library
    //     we explicitly do not want in PID 1. (Verified by inspecting
    //     `nix` 0.29 and `rustix` 0.38 sources in the offline cache.)
    //   * Why this is safe: `image` is a live borrow valid for the
    //     duration of the call, `params` is a NUL-terminated C string
    //     owned by the caller, and `image.len()` cannot exceed
    //     `c_ulong::MAX` because a `Vec<u8>` can hold at most
    //     `isize::MAX` bytes. The syscall reads, never writes, our
    //     pointers; failure is reported via the return value + errno.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_init_module,
            image.as_ptr() as *const c_void,
            image.len() as c_ulong,
            params.as_ptr(),
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

/// Read `entry.path` from disk, decompress it if its extension says
/// so, and hand the raw module image to `init_module(2)`.
///
/// File-IO and decompression errors surface as `NmblError::Module`.
/// Idempotent re-loads return `Ok(LoadOutcome::AlreadyLoaded)`;
/// kernel-side refusals return `Ok(LoadOutcome::KernelRefused { … })`
/// and must be logged + skipped by the caller rather than aborting
/// the boot.
pub fn load_module(entry: &ModuleEntry) -> Result<LoadOutcome> {
    // Shrunk module closures (NixOS `makeModulesClosure { allowMissing
    // = true; }`) can leave `modules.dep` referencing `.ko` files that
    // aren't on disk. Surface that as `FileMissing` so callers warn +
    // skip instead of aborting the boot for an over-eager soft dep.
    if !entry.path.exists() {
        return Ok(LoadOutcome::FileMissing);
    }
    let image = decompress_module(&entry.path, &entry.name)?;
    // `modules.dep` carries no parameters and the bash bootloader never
    // set them — pass an empty string and move on.
    let params = CString::default();
    init_module(&image, &params, &entry.name, &entry.path)
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
        // as recoverable so we don't abort the boot for it. ENOENT here
        // is the module's own init() returning -ENOENT (e.g. a backend
        // cipher is unavailable); file-not-found at the .ko path itself
        // is intercepted earlier by load_module's existence pre-check.
        for errno in [
            Errno::EOPNOTSUPP,
            Errno::ENOEXEC,
            Errno::ENODEV,
            Errno::ENOSYS,
            Errno::EINVAL,
            Errno::ENOENT,
        ] {
            assert!(
                is_recoverable_module_error(errno),
                "{errno:?} should be recoverable"
            );
        }
    }

    #[test]
    fn recoverable_classifier_does_not_swallow_real_errors() {
        // Filesystem permission / OOM / generic IO failures and
        // dep-graph bugs (ELOOP) must NOT be classified as recoverable.
        // EEXIST is excluded because it has its own
        // `LoadOutcome::AlreadyLoaded` variant and never reaches the
        // classifier.
        for errno in [
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

    #[test]
    fn compression_for_path_classifies_known_suffixes() {
        assert_eq!(
            compression_for_path(Path::new("/lib/modules/6.6.71/kernel/fs/ext4/ext4.ko")),
            Compression::None
        );
        assert_eq!(
            compression_for_path(Path::new("/lib/modules/6.6.71/kernel/fs/ext4/ext4.ko.xz")),
            Compression::Xz
        );
        assert_eq!(
            compression_for_path(Path::new("/lib/modules/6.6.71/kernel/fs/ext4/ext4.ko.zst")),
            Compression::Zst
        );
        assert_eq!(
            compression_for_path(Path::new("/lib/modules/6.6.71/kernel/fs/ext4/ext4.ko.gz")),
            Compression::Gz
        );
    }

    #[test]
    fn compression_for_path_falls_back_to_none_for_unknown_suffix() {
        // Unrecognised suffixes are treated as "no compression" — the
        // kernel will reject the bytes with ENOEXEC if they're not a
        // valid ELF, which is the right failure mode (clear errno
        // rather than a confused decompression attempt).
        assert_eq!(
            compression_for_path(Path::new("/m/weird.ko.lz4")),
            Compression::None
        );
        assert_eq!(
            compression_for_path(Path::new("/m/no_suffix_at_all")),
            Compression::None
        );
    }

    /// Helper: write `bytes` to a fresh temp file with the given
    /// suffix and return the path + the holding `TempDir` so the file
    /// lives until the dir is dropped.
    fn write_temp(suffix: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("widget{suffix}"));
        std::fs::write(&path, bytes).expect("write temp module");
        (dir, path)
    }

    #[test]
    fn decompress_module_passes_through_uncompressed() {
        let payload: Vec<u8> = (0u8..=63).collect();
        let (_dir, path) = write_temp(".ko", &payload);
        let got = decompress_module(&path, "widget").expect("decompress");
        assert_eq!(got, payload);
    }

    #[test]
    fn decompress_module_round_trips_xz() {
        // Encode with the same crate, decode with `decompress_module`.
        let payload: Vec<u8> = b"NMBL-MODULE-LOAD-TEST-XZ".repeat(8);
        let mut compressed: Vec<u8> = Vec::new();
        {
            let mut reader = std::io::Cursor::new(&payload);
            lzma_rs::xz_compress(&mut reader, &mut compressed).expect("xz_compress");
        }
        let (_dir, path) = write_temp(".ko.xz", &compressed);
        let got = decompress_module(&path, "widget").expect("decompress");
        assert_eq!(got, payload);
    }

    #[test]
    fn decompress_module_round_trips_gz() {
        use flate2::Compression as GzLevel;
        use flate2::write::GzEncoder;
        use std::io::Write;
        let payload: Vec<u8> = b"NMBL-MODULE-LOAD-TEST-GZ".repeat(8);
        let mut encoder = GzEncoder::new(Vec::new(), GzLevel::default());
        encoder.write_all(&payload).expect("gz write");
        let compressed = encoder.finish().expect("gz finish");
        let (_dir, path) = write_temp(".ko.gz", &compressed);
        let got = decompress_module(&path, "widget").expect("decompress");
        assert_eq!(got, payload);
    }

    #[test]
    fn decompress_module_decodes_zst_fixture() {
        // `ruzstd` is decode-only, so we can't synthesize a fixture
        // round-trip in-process. Embed a pre-compressed blob whose
        // plaintext is `b"NMBL-MODULE-LOAD-TEST"` (21 bytes, produced
        // once with `zstd -19`). If this ever needs regenerating:
        //
        //     printf '%s' 'NMBL-MODULE-LOAD-TEST' | zstd -19 | od -An -tx1
        const ZST_FIXTURE: &[u8] = &[
            0x28, 0xb5, 0x2f, 0xfd, 0x04, 0x68, 0xa9, 0x00, 0x00, 0x4e, 0x4d, 0x42, 0x4c, 0x2d,
            0x4d, 0x4f, 0x44, 0x55, 0x4c, 0x45, 0x2d, 0x4c, 0x4f, 0x41, 0x44, 0x2d, 0x54, 0x45,
            0x53, 0x54, 0x62, 0xec, 0xd6, 0x51,
        ];
        let (_dir, path) = write_temp(".ko.zst", ZST_FIXTURE);
        let got = decompress_module(&path, "widget").expect("decompress");
        assert_eq!(got, b"NMBL-MODULE-LOAD-TEST");
    }

    #[test]
    fn decompress_module_surfaces_corrupt_xz_as_module_error() {
        // A 4-byte garbage payload labelled `.ko.xz` cannot possibly
        // be a valid XZ stream. The contract is that decompression
        // failure surfaces as `NmblError::Module { source: EIO }`
        // (with the real backend message logged via nmbl_warn!) so
        // the caller's match arms stay simple.
        let (_dir, path) = write_temp(".ko.xz", b"junk");
        let err = decompress_module(&path, "widget").expect_err("must fail");
        match err {
            NmblError::Module { name, source, .. } => {
                assert_eq!(name, "widget");
                assert_eq!(source, nix::Error::from(Errno::EIO));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn load_module_returns_file_missing_for_absent_ko_file() {
        // makeModulesClosure { allowMissing = true; } prunes transitive
        // modules out of the closure but leaves them referenced in
        // modules.dep. load_module must surface that as a non-fatal
        // FileMissing outcome rather than propagating the underlying
        // ENOENT as a fatal error.
        let dir = tempfile::tempdir().expect("tempdir");
        let entry = ModuleEntry {
            name: "ghostly".to_owned(),
            path: dir.path().join("ghostly.ko.xz"),
            deps: Vec::new(),
        };
        let outcome = load_module(&entry).expect("must not error");
        assert!(matches!(outcome, LoadOutcome::FileMissing));
    }

    #[test]
    fn decompress_module_surfaces_missing_file_as_io_error() {
        // Reading the file itself failing is a real IO error, not a
        // decompression error — surface it as `NmblError::Io` so
        // operators can see the path in the context message.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("does_not_exist.ko.xz");
        let err = decompress_module(&path, "widget").expect_err("must fail");
        match err {
            NmblError::Io { .. } => {}
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
