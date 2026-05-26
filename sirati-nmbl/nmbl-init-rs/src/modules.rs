//! Explicit kernel-module loader.
//!
//! Replaces the `for module in $explicit_modules; do modprobe ...` loop
//! in `scripts/mount-and-kernel.sh.nix`. Loads every module listed in
//! `config.kernel_modules.explicit`, plus each module's transitive
//! dependencies, via `sys::module`. Blacklisted names are skipped
//! (blacklist wins over explicit).

use std::collections::{HashMap, HashSet};

use crate::config::Config;
use crate::error::Result;
use crate::sys::module::{self, ModuleEntry};
use crate::{nmbl_info, nmbl_verbose, nmbl_warn};

/// Load every explicit module + its transitive deps. Blacklisted module
/// names (top-level or transitive) are skipped with a log line; a
/// blacklisted dep is a config inconsistency and gets a warning.
pub fn load_explicit_modules(config: &Config) -> Result<()> {
    let release = crate::sys::uname::kernel_release()?;
    let entries = module::load_modules_dep(&config.kernel_modules.modules_dir, &release)?;
    let by_name: HashMap<String, &ModuleEntry> = module::index_by_name(&entries);
    let blacklist: HashSet<&str> = config
        .kernel_modules
        .blacklist
        .iter()
        .map(String::as_str)
        .collect();

    let mut loaded: usize = 0;
    for name in &config.kernel_modules.explicit {
        if blacklist.contains(name.as_str()) {
            nmbl_verbose!("skipping blacklisted module {}", name);
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
            module::load_module(entry)?;
            loaded += 1;
        }
    }

    nmbl_info!("loaded {} explicit modules", loaded);
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
}
