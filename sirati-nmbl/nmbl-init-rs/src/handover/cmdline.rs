//! Kernel cmdline decoder: split NMBL-set parameters from the rest.
//!
//! NMBL builds the kexec cmdline from the generation's `kernel-params` plus an
//! `init=` argument, and appends its own markers (the untested-rollback
//! notification token, and the boot-set selector on GRUB-dispatcher hosts).
//! [`parse_cmdline`] classifies each whitespace-delimited token so a status
//! view can show "what NMBL set" separately from "what the generation
//! carried".

/// One parsed cmdline token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdlineParam {
    /// The raw token as it appears on the line (e.g. `init=/nix/store/…/init`).
    pub raw: String,
    /// The key half (`init` for `init=…`, or the whole token for a bare flag).
    pub key: String,
    /// The value half, when the token is `key=value`; `None` for a bare flag.
    pub value: Option<String>,
    /// Whether NMBL itself is responsible for this token (an NMBL marker or the
    /// `init=` it synthesises), as opposed to a generation kernel-param.
    pub nmbl_set: bool,
}

/// A cmdline split into the parameters NMBL set and everything else.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedCmdline {
    /// Every token, in order, classified.
    pub params: Vec<CmdlineParam>,
}

impl ParsedCmdline {
    /// The subset NMBL set (markers + synthesised `init=`).
    pub fn nmbl_params(&self) -> impl Iterator<Item = &CmdlineParam> {
        self.params.iter().filter(|p| p.nmbl_set)
    }

    /// The subset that came from the generation's kernel-params.
    pub fn generation_params(&self) -> impl Iterator<Item = &CmdlineParam> {
        self.params.iter().filter(|p| !p.nmbl_set)
    }

    /// Whether the untested-generation rollback marker is present.
    #[must_use]
    pub fn has_rollback_marker(&self) -> bool {
        self.params
            .iter()
            .any(|p| p.key == crate::security_consts::ROLLBACK_CMDLINE)
    }

    /// The selected boot-set config path (`nmbl.config=…`), when present.
    #[must_use]
    pub fn boot_set_config(&self) -> Option<&str> {
        self.params
            .iter()
            .find(|p| p.key == "nmbl.config")
            .and_then(|p| p.value.as_deref())
    }
}

/// The token keys NMBL sets itself (as opposed to generation kernel-params).
/// `init=` is synthesised by NMBL's cmdline builder; the two `nmbl.*` markers
/// are appended by the boot flow. A token is NMBL-set when its key matches one
/// of these OR carries the `nmbl.` prefix (future-proofing new markers).
fn is_nmbl_key(key: &str) -> bool {
    key == "init" || key == crate::security_consts::ROLLBACK_CMDLINE || key.starts_with("nmbl.")
}

/// Parse a kernel cmdline into classified tokens.
///
/// Splits on ASCII whitespace (matching how the kernel and NMBL build the
/// line). Each token becomes a [`CmdlineParam`]; `key=value` is split on the
/// FIRST `=` so a value containing `=` (rare, e.g. an embedded path) is kept
/// whole. NMBL-set tokens are flagged via [`is_nmbl_key`].
#[must_use]
pub fn parse_cmdline(cmdline: &str) -> ParsedCmdline {
    let params = cmdline
        .split_ascii_whitespace()
        .map(|tok| {
            let (key, value) = match tok.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (tok.to_string(), None),
            };
            let nmbl_set = is_nmbl_key(&key);
            CmdlineParam {
                raw: tok.to_string(),
                key,
                value,
                nmbl_set,
            }
        })
        .collect();
    ParsedCmdline { params }
}

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::indexing_slicing,
    clippy::expect_used,
    reason = "tests assert"
)]
mod tests {
    use super::*;

    #[test]
    fn splits_init_and_generation_params() {
        let parsed = parse_cmdline("quiet loglevel=4 init=/nix/store/abc/init");
        assert_eq!(parsed.params.len(), 3);
        // init is NMBL-set; quiet + loglevel are generation params.
        let nmbl: Vec<_> = parsed.nmbl_params().map(|p| p.raw.as_str()).collect();
        assert_eq!(nmbl, vec!["init=/nix/store/abc/init"]);
        let generation: Vec<_> = parsed.generation_params().map(|p| p.key.as_str()).collect();
        assert_eq!(generation, vec!["quiet", "loglevel"]);
    }

    #[test]
    fn detects_the_rollback_marker() {
        let line = format!("init=/x quiet {}", crate::security_consts::ROLLBACK_CMDLINE);
        let parsed = parse_cmdline(&line);
        assert!(parsed.has_rollback_marker());
        // And the marker is classified as NMBL-set.
        assert!(
            parsed
                .params
                .iter()
                .any(|p| p.key == crate::security_consts::ROLLBACK_CMDLINE && p.nmbl_set)
        );
    }

    #[test]
    fn no_rollback_marker_when_absent() {
        let parsed = parse_cmdline("init=/x quiet");
        assert!(!parsed.has_rollback_marker());
    }

    #[test]
    fn extracts_boot_set_config_selector() {
        let parsed = parse_cmdline("init=/x nmbl.config=/nmbl-boot-sets/B/config quiet");
        assert_eq!(parsed.boot_set_config(), Some("/nmbl-boot-sets/B/config"));
        // The selector is NMBL-set (nmbl. prefix).
        assert!(
            parsed
                .params
                .iter()
                .any(|p| p.key == "nmbl.config" && p.nmbl_set)
        );
    }

    #[test]
    fn value_with_embedded_equals_is_kept_whole() {
        let parsed = parse_cmdline("systemd.setenv=FOO=bar");
        assert_eq!(parsed.params[0].key, "systemd.setenv");
        assert_eq!(parsed.params[0].value.as_deref(), Some("FOO=bar"));
    }

    #[test]
    fn round_trips_the_nmbl_cmdline_builder_shape() {
        // Mirror the exact shape boot::handoff::build_cmdline emits for a
        // generation with kernel-params + a synthesised init=, plus the
        // rollback marker the boot flow appends. Parsing must recover the
        // NMBL-set set exactly.
        let line = format!(
            "console=ttyS0 quiet init=/nix/var/nix/profiles/system-3-link/init {}",
            crate::security_consts::ROLLBACK_CMDLINE
        );
        let parsed = parse_cmdline(&line);
        let nmbl: Vec<_> = parsed.nmbl_params().map(|p| p.key.as_str()).collect();
        assert_eq!(nmbl, vec!["init", crate::security_consts::ROLLBACK_CMDLINE]);
        assert!(parsed.has_rollback_marker());
    }
}
