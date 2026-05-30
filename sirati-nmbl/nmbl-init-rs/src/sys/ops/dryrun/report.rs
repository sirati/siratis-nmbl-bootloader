//! [`MissingFile`] findings collected by the dry-run [`DryRunSys`].
//!
//! A `--validate-initrm` run walks the genuine boot spine against a
//! [`super::ClosureView`] over an extracted initramfs and records a
//! [`MissingFile`] for every file the boot would touch that the closure
//! lacks. The mode lists ALL findings and exits non-zero, mirroring the
//! `validate_hardware` collect-all-then-report style in
//! `main_parts::early_exit`.
//!
//! Findings carry an `op` tag (the [`super::super::SysOps`] method that
//! needed the file), the `path` that was absent, and a free-form
//! `context` line so the operator can tell WHICH config knob or boot
//! phase asked for it.

use std::path::{Path, PathBuf};

/// One file the dry-run determined the boot would need but the closure
/// does not contain.
///
/// `op` is the system-op method that required it (e.g. `"load_module"`,
/// `"kexec_load"`, `"run"`); `path` is the absent file; `context` adds
/// human-readable detail (which module, which activation, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingFile {
    /// The `SysOps` method that needed the file.
    pub op: &'static str,
    /// The absent path (as the boot would reference it).
    pub path: PathBuf,
    /// Human-readable detail: which config knob / phase asked for it.
    pub context: String,
}

impl MissingFile {
    /// Build a finding for `path`, attributing it to `op` with `context`.
    pub fn new(op: &'static str, path: impl Into<PathBuf>, context: impl Into<String>) -> Self {
        Self {
            op,
            path: path.into(),
            context: context.into(),
        }
    }
}

impl std::fmt::Display for MissingFile {
    /// One-line rendering matching the `validate_hardware` failure
    /// style: a leading op tag, the path, then the context.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}] {} — {}",
            self.op,
            self.path.display(),
            self.context
        )
    }
}

/// A growable collection of [`MissingFile`] findings with a render that
/// mirrors `validate_hardware`'s "(N problem(s)):" listing.
#[derive(Debug, Default, Clone)]
pub struct Findings {
    items: Vec<MissingFile>,
}

impl Findings {
    /// Empty collection.
    #[must_use]
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    /// Record one finding.
    pub fn push(&mut self, finding: MissingFile) {
        self.items.push(finding);
    }

    /// `true` if no findings were recorded (the closure satisfied every
    /// file the boot touched).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number of findings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Borrow the findings for inspection / assertions.
    #[must_use]
    pub fn items(&self) -> &[MissingFile] {
        &self.items
    }

    /// `true` if any finding was recorded for `path` (any op).
    #[must_use]
    pub fn contains_path(&self, path: &Path) -> bool {
        self.items.iter().any(|f| f.path == path)
    }

    /// Render every finding as a multi-line block in the same shape
    /// `validate_hardware` uses: a `(N problem(s)):` header and one
    /// `  - <finding>` line per item. Returns an empty string when there
    /// are no findings so callers can branch on `is_empty()` first.
    #[must_use]
    pub fn render(&self) -> String {
        if self.items.is_empty() {
            return String::new();
        }
        let mut out = format!(
            "initramfs closure incomplete ({} problem(s)):\n",
            self.items.len()
        );
        for f in &self.items {
            out.push_str(&format!("  - {f}\n"));
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn empty_findings_render_to_nothing() {
        let f = Findings::new();
        assert!(f.is_empty());
        assert_eq!(f.len(), 0);
        assert_eq!(f.render(), "");
    }

    #[test]
    fn push_records_and_renders() {
        let mut f = Findings::new();
        f.push(MissingFile::new(
            "load_module",
            "/lib/foo.ko",
            "phase 2b ext4",
        ));
        assert!(!f.is_empty());
        assert_eq!(f.len(), 1);
        assert!(f.contains_path(Path::new("/lib/foo.ko")));
        let rendered = f.render();
        assert!(rendered.contains("1 problem(s)"));
        assert!(rendered.contains("/lib/foo.ko"));
        assert!(rendered.contains("load_module"));
        assert!(rendered.contains("phase 2b ext4"));
    }

    #[test]
    fn missing_file_display_includes_op_path_context() {
        let m = MissingFile::new("kexec_load", "/boot/vmlinuz", "kernel image");
        let s = format!("{m}");
        assert!(s.contains("kexec_load"));
        assert!(s.contains("/boot/vmlinuz"));
        assert!(s.contains("kernel image"));
    }
}
