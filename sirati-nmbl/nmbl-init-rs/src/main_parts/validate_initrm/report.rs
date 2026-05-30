//! Aggregated result of a `--validate-initrm` run: per-scenario dry-run
//! findings plus the optional UKI structural findings, de-duplicated for
//! the operator-facing listing and reduced to an exit-code contract.

use std::collections::BTreeSet;

use nmbl_init::sys::ops::dryrun::MissingFile;
use nmbl_init::sys::uki::UkiFinding;

/// One dry-run finding tagged with the scenario that surfaced it, so the
/// report can tell the operator WHICH boot path needed the absent file.
pub(super) struct ScenarioFinding {
    /// Human-readable scenario name (e.g. `"NormalBoot"`).
    pub(super) scenario: &'static str,
    /// The underlying missing-file finding.
    pub(super) finding: MissingFile,
}

/// Everything a `--validate-initrm` run collected. `render` produces the
/// operator listing (mirroring `validate_hardware`'s style) and
/// `is_clean` drives the exit code.
pub(crate) struct InitrmReport {
    /// Every dry-run finding across all four scenarios, scenario-tagged.
    findings: Vec<ScenarioFinding>,
    /// Structural UKI findings (empty when no `--uki` was passed).
    uki: Vec<UkiFinding>,
    /// `true` once at least one scenario ran (so an all-empty report
    /// means "validated clean", not "nothing ran").
    ran: bool,
}

impl InitrmReport {
    /// Empty report; scenarios push into it as they run.
    pub(crate) fn new() -> Self {
        Self {
            findings: Vec::new(),
            uki: Vec::new(),
            ran: false,
        }
    }

    /// Mark that a scenario executed (so a clean report is meaningful).
    pub(crate) fn mark_ran(&mut self) {
        self.ran = true;
    }

    /// Record every finding from one scenario, tagging each with the
    /// scenario name.
    pub(crate) fn add_scenario(&mut self, scenario: &'static str, findings: &[MissingFile]) {
        for f in findings {
            self.findings.push(ScenarioFinding {
                scenario,
                finding: f.clone(),
            });
        }
    }

    /// Merge structural UKI findings into the report.
    pub(crate) fn add_uki(&mut self, findings: Vec<UkiFinding>) {
        self.uki.extend(findings);
    }

    /// `true` when nothing was found wrong (the closure satisfied every
    /// file every reachable boot path touched, and the UKI — if checked —
    /// is structurally valid).
    pub(crate) fn is_clean(&self) -> bool {
        self.findings.is_empty() && self.uki.is_empty()
    }

    /// Render the operator-facing listing. Dry-run findings are
    /// de-duplicated across scenarios on `(op, path, context)`, with the
    /// set of scenarios that hit each one shown inline. Returns an empty
    /// string when the report is clean so callers branch on `is_clean`.
    pub(crate) fn render(&self) -> String {
        if self.is_clean() {
            return String::new();
        }
        let mut out = String::new();
        if !self.findings.is_empty() {
            out.push_str(&self.render_missing_files());
        }
        if !self.uki.is_empty() {
            out.push_str(&self.render_uki());
        }
        out
    }

    /// `true` if at least one scenario ran. The driver only ever calls
    /// `render`/`is_clean` after running, but this keeps the invariant
    /// inspectable and documents the "ran" flag's purpose.
    #[cfg(test)]
    pub(super) fn ran(&self) -> bool {
        self.ran
    }

    /// De-duplicate the scenario-tagged missing-file findings on
    /// `(op, path, context)` and render one line each, listing the
    /// scenarios that surfaced it. A `BTreeSet` keys the dedup AND sorts
    /// the output deterministically (stable across runs / scenario order).
    fn render_missing_files(&self) -> String {
        // Key dedup on the rendered finding line; collect the scenario set
        // per key. A `BTreeMap`/`BTreeSet` gives stable ordering for both
        // the keys and the per-key scenario list.
        let mut per_key: std::collections::BTreeMap<String, BTreeSet<&'static str>> =
            std::collections::BTreeMap::new();
        for sf in &self.findings {
            per_key
                .entry(format!("{}", sf.finding))
                .or_default()
                .insert(sf.scenario);
        }
        let mut out = format!(
            "initramfs closure incomplete ({} distinct problem(s)):\n",
            per_key.len()
        );
        for (line, scenarios) in &per_key {
            let joined = scenarios.iter().copied().collect::<Vec<_>>().join(", ");
            out.push_str(&format!("  - {line} [{joined}]\n"));
        }
        out
    }

    /// Render the UKI structural findings as their own block.
    fn render_uki(&self) -> String {
        let mut out = format!("UKI validation FAILED ({} problem(s)):\n", self.uki.len());
        for f in &self.uki {
            out.push_str(&format!("  - [{:?}] {}\n", f.kind, f.detail));
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;
    use nmbl_init::sys::uki::UkiFindingKind;
    use std::path::Path;

    #[test]
    fn empty_report_is_clean_and_renders_nothing() {
        let mut r = InitrmReport::new();
        r.mark_ran();
        assert!(r.is_clean());
        assert!(r.ran());
        assert_eq!(r.render(), "");
    }

    #[test]
    fn dedups_same_finding_across_scenarios() {
        let mut r = InitrmReport::new();
        let f = MissingFile::new("load_module", "/lib/foo.ko", "phase 2b");
        r.add_scenario("NormalBoot", std::slice::from_ref(&f));
        r.add_scenario("PrettyShell", std::slice::from_ref(&f));
        assert!(!r.is_clean());
        let rendered = r.render();
        // One distinct problem despite two scenarios hitting it.
        assert!(rendered.contains("1 distinct problem"), "{rendered}");
        assert!(rendered.contains("/lib/foo.ko"), "{rendered}");
        assert!(rendered.contains("NormalBoot"), "{rendered}");
        assert!(rendered.contains("PrettyShell"), "{rendered}");
    }

    #[test]
    fn distinct_findings_are_listed_separately() {
        let mut r = InitrmReport::new();
        r.add_scenario(
            "NormalBoot",
            &[
                MissingFile::new("load_module", "/lib/a.ko", "x"),
                MissingFile::new("kexec_load", "/boot/vmlinuz", "y"),
            ],
        );
        let rendered = r.render();
        assert!(rendered.contains("2 distinct problem"), "{rendered}");
        assert!(rendered.contains("/lib/a.ko"));
        assert!(rendered.contains("/boot/vmlinuz"));
    }

    #[test]
    fn uki_findings_merge_and_break_cleanliness() {
        let mut r = InitrmReport::new();
        r.mark_ran();
        assert!(r.is_clean());
        r.add_uki(vec![UkiFinding {
            kind: UkiFindingKind::MissingSection,
            detail: "required section `.linux` is missing".to_string(),
        }]);
        assert!(!r.is_clean());
        let rendered = r.render();
        assert!(rendered.contains("UKI validation FAILED"), "{rendered}");
        assert!(rendered.contains(".linux"), "{rendered}");
        let _ = Path::new("/");
    }
}
