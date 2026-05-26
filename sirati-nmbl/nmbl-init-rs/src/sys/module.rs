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

/// Derive the kernel module name from a `.ko[.xz|.zst|.gz]` path.
/// Underscores stay as-is — `modules.dep` and `/sys/module/<name>` both
/// use the underscore form, so no hyphen canonicalization here.
fn module_name_from_path(path: &Path) -> Option<String> {
    let file = path.file_name().and_then(|s| s.to_str())?;
    let stripped = file
        .strip_suffix(".xz")
        .or_else(|| file.strip_suffix(".zst"))
        .or_else(|| file.strip_suffix(".gz"))
        .unwrap_or(file);
    stripped.strip_suffix(".ko").map(str::to_owned)
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
pub fn resolve_load_order<'a>(
    name: &str,
    by_name: &'a HashMap<String, &'a ModuleEntry>,
) -> Result<Vec<&'a ModuleEntry>> {
    let mut order: Vec<&'a ModuleEntry> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut visiting: HashSet<String> = HashSet::new();
    visit(name, by_name, &mut order, &mut visited, &mut visiting)?;
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

/// Call `finit_module(fd, params, flags)` directly. Returns `Ok(())`
/// on success or when the module is already loaded (`EEXIST`).
fn finit_module(fd: &OwnedFd, params: &CString, flags: u32, name: &str, path: &Path) -> Result<()> {
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
        return Ok(());
    }
    let errno = Errno::last();
    if errno == Errno::EEXIST {
        // Module is already in the kernel — treat as success.
        return Ok(());
    }
    Err(module_err(name, path, errno))
}

/// Open the module's file at `entry.path` and load it via
/// `finit_module(2)`. Compressed `.ko.xz` / `.ko.zst` / `.ko.gz` are
/// passed with `MODULE_INIT_COMPRESSED_FILE` so the kernel handles
/// decompression. Returns `Ok(())` if loaded or already loaded.
pub fn load_module(entry: &ModuleEntry) -> Result<()> {
    let fd = open_module_fd(&entry.path, &entry.name)?;
    let flags = flags_for_path(&entry.path);
    // `modules.dep` carries no parameters and the bash bootloader never
    // set them — pass an empty string and move on.
    let params = CString::default();
    finit_module(&fd, &params, flags, &entry.name, &entry.path)
}

/// Resolve `name` against `by_name`, then load every entry in load
/// order. Idempotent because individual `load_module` calls swallow
/// `EEXIST`.
pub fn load_with_deps(name: &str, by_name: &HashMap<String, &ModuleEntry>) -> Result<()> {
    for entry in resolve_load_order(name, by_name)? {
        load_module(entry)?;
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
}
