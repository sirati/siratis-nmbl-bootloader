//! Kernel module loading via `finit_module(2)`, with a dep-graph
//! resolver fed by `<modules_dir>/<release>/modules.dep`.
//!
//! This is the moral replacement of the `modprobe` invocation in the
//! bash `mount-and-kernel.sh.nix`. It deliberately does not shell out.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use nix::errno::Errno;

use crate::error::{NmblError, Result};

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

/// Strip the `.ko` and any compression suffix from a filename, yielding
/// the kernel's module name (with underscores intact — `modules.dep` and
/// `/sys/module/<name>` both use the underscore form, so we don't
/// canonicalize hyphens here).
fn module_name_from_filename(file_name: &str) -> Option<String> {
    // Order matters: strip the compression suffix first, then `.ko`.
    let stripped = file_name
        .strip_suffix(".xz")
        .or_else(|| file_name.strip_suffix(".zst"))
        .or_else(|| file_name.strip_suffix(".gz"))
        .unwrap_or(file_name);
    stripped.strip_suffix(".ko").map(str::to_owned)
}

/// Derive the module name from a path that points at the `.ko*` file.
fn module_name_from_path(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|s| s.to_str())
        .and_then(module_name_from_filename)
}

/// Parse a `modules.dep` text body into entries, anchoring relative
/// paths under `root` (which is normally `<modules_dir>/<release>/`).
///
/// Lines that don't contain a `:` are skipped. Empty dependency lists
/// are valid — most modules have no deps.
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
        let mod_rel = lhs.trim();
        if mod_rel.is_empty() {
            continue;
        }
        let mod_path = root.join(mod_rel);
        let Some(name) = module_name_from_path(&mod_path) else {
            continue;
        };
        let deps: Vec<String> = rhs
            .split_whitespace()
            .filter_map(|dep_rel| {
                let dep_path = root.join(dep_rel);
                module_name_from_path(&dep_path)
            })
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

/// Synthesize a `NmblError::Module` from a raw errno + module name.
fn module_err(name: &str, errno: Errno) -> NmblError {
    NmblError::Module {
        source: nix::Error::from(errno),
        name: name.to_owned(),
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
        // We were already mid-visit on this node → cycle.
        return Err(module_err(name, Errno::ELOOP));
    }
    let entry = by_name
        .get(name)
        .copied()
        .ok_or_else(|| module_err(name, Errno::ENOENT))?;
    for dep in &entry.deps {
        visit(dep, by_name, order, visited, visiting)?;
    }
    visiting.remove(name);
    visited.insert(name.to_owned());
    order.push(entry);
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

        let ext4 = entries
            .iter()
            .find(|e| e.name == "ext4")
            .expect("ext4 entry missing");
        assert_eq!(ext4.path, root.join("kernel/fs/ext4/ext4.ko.xz"));
        assert_eq!(ext4.deps, vec!["jbd2".to_owned(), "crc16".to_owned()]);

        let crc16 = entries
            .iter()
            .find(|e| e.name == "crc16")
            .expect("crc16 entry missing");
        assert!(crc16.deps.is_empty());
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
