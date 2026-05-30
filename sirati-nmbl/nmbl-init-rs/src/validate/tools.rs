//! Resolved external-tool paths supplied to `--validate-hardware`.
//!
//! The Nix install script knows the exact store paths of the activation
//! tools (cryptsetup / lvm2 / mdadm), so it hands them to the validator
//! rather than letting the validator guess. Two equivalent channels are
//! accepted; an explicit `--tool=` arg wins over the env var:
//!
//! * repeatable arg `--tool=<kind>:<path>` (e.g.
//!   `--tool=cryptsetup:/nix/store/…/bin/cryptsetup`)
//! * env var `NMBL_TOOL_CRYPTSETUP=<path>` (uppercased kind)
//!
//! Only `cryptsetup` is consumed today (LUKS-header probing); the map is
//! generic so lvm2/mdadm can be threaded through later without a
//! signature change. An absent tool is not an error — the hardware
//! check falls back to reading the LUKS magic itself.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Tool kind -> absolute path, as supplied by the installer.
#[derive(Debug, Default, Clone)]
pub struct ToolPaths {
    map: BTreeMap<String, PathBuf>,
}

impl ToolPaths {
    /// Record a `<kind>:<path>` mapping. Returns `Err` when the spec has
    /// no `:` separator or an empty kind/path — an operator typo we want
    /// surfaced rather than silently dropped.
    pub fn insert_spec(&mut self, spec: &str) -> Result<(), String> {
        let Some((kind, path)) = spec.split_once(':') else {
            return Err(format!(
                "--tool expects <kind>:<path>, got {spec:?} (no ':' separator)"
            ));
        };
        if kind.is_empty() || path.is_empty() {
            return Err(format!(
                "--tool expects a non-empty <kind> and <path>, got {spec:?}"
            ));
        }
        self.map.insert(kind.to_string(), PathBuf::from(path));
        Ok(())
    }

    /// Look up a tool path, preferring an explicit `--tool=` entry and
    /// falling back to `NMBL_TOOL_<KIND>` from the environment.
    pub fn get(&self, kind: &str) -> Option<PathBuf> {
        if let Some(p) = self.map.get(kind) {
            return Some(p.clone());
        }
        let env_key = format!("NMBL_TOOL_{}", kind.to_uppercase());
        std::env::var_os(env_key).map(PathBuf::from)
    }

    /// Convenience accessor for the only tool used today.
    pub fn cryptsetup(&self) -> Option<PathBuf> {
        self.get("cryptsetup")
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests assert on contract failures")]
mod tests {
    use super::*;

    #[test]
    fn insert_and_get_roundtrips() {
        let mut t = ToolPaths::default();
        t.insert_spec("cryptsetup:/store/bin/cryptsetup")
            .expect("valid spec");
        assert_eq!(t.cryptsetup(), Some(PathBuf::from("/store/bin/cryptsetup")));
    }

    #[test]
    fn path_with_colon_is_preserved() {
        // split_once stops at the first ':', so a path that itself
        // contains ':' (unusual but legal) survives intact.
        let mut t = ToolPaths::default();
        t.insert_spec("cryptsetup:/odd:path/cryptsetup")
            .expect("valid spec");
        assert_eq!(
            t.get("cryptsetup"),
            Some(PathBuf::from("/odd:path/cryptsetup"))
        );
    }

    #[test]
    fn missing_separator_errors() {
        let mut t = ToolPaths::default();
        let err = t.insert_spec("cryptsetup").expect_err("no ':' must error");
        assert!(err.contains("no ':'"), "{err}");
    }

    #[test]
    fn empty_kind_errors() {
        let mut t = ToolPaths::default();
        let err = t.insert_spec(":/p").expect_err("empty kind must error");
        assert!(err.contains("non-empty"), "{err}");
    }

    #[test]
    fn absent_tool_is_none() {
        let t = ToolPaths::default();
        assert!(t.get("mdadm").is_none());
    }
}
