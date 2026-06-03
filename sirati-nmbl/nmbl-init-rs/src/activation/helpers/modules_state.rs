//! Required-modules warnings and loaded-modules state for activation.

use std::collections::HashSet;

use crate::config::Activation;
use crate::error::{NmblError, Result};
use crate::nmbl_warn;

use super::kind_label;

pub(super) const PROC_MODULES: &str = "/proc/modules";

pub(crate) fn check_required_modules(activation: &Activation, loaded: &HashSet<String>) {
    for module in &activation.required_modules {
        // /proc/modules always uses the underscore spelling; config
        // entries may use either (e.g. "dm-crypt" vs "dm_crypt").
        let canonical = module.replace('-', "_");
        if !loaded.contains(&canonical) {
            nmbl_warn!(
                "activation {} requires module {} but it's not loaded; attempting anyway",
                kind_label(activation.kind),
                module
            );
        }
    }
}

pub(crate) fn loaded_modules() -> Result<HashSet<String>> {
    let text = std::fs::read_to_string(PROC_MODULES).map_err(|source| NmblError::Io {
        source,
        context: format!("reading {PROC_MODULES} to check activation prerequisites"),
    })?;
    Ok(parse_loaded_modules(&text))
}

/// First whitespace token of each non-blank line; factored for unit tests.
pub(super) fn parse_loaded_modules(text: &str) -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(name) = trimmed.split_whitespace().next()
            && !name.is_empty()
        {
            out.insert(name.to_string());
        }
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure; production lints are too strict for asserts"
)]
mod tests {
    use super::*;

    /// Mirrors `/proc/modules` (+ a leading-whitespace and a blank line).
    const SAMPLE_PROC_MODULES: &str = "\
ext4 901120 1 - Live 0x0000000000000000
\tnvme 49152 0 - Live 0x0000000000000000
crc32c_generic 16384 1 ext4, Live 0x0000000000000000

";

    #[test]
    fn parse_loaded_modules_extracts_names_and_edge_cases() {
        let set = parse_loaded_modules(SAMPLE_PROC_MODULES);
        assert_eq!(set.len(), 3, "exactly three modules in the sample");
        assert!(set.contains("ext4"));
        assert!(set.contains("nvme"), "leading whitespace must be ignored");
        assert!(set.contains("crc32c_generic"));

        assert!(parse_loaded_modules("").is_empty(), "empty input");
        assert!(
            parse_loaded_modules("\n   \n\t\n").is_empty(),
            "blank / whitespace-only lines must not become entries"
        );

        // Truncated last line (no trailing newline) — the kernel always
        // emits one, but the parser should tolerate its absence.
        let truncated = parse_loaded_modules("ext4 901120 1 - Live 0x0\nvfat");
        assert!(truncated.contains("ext4"));
        assert!(truncated.contains("vfat"));
    }
}
