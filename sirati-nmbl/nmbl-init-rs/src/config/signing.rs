use serde::Deserialize;

/// `[signing]` section of the runtime config (secure-boot builds only).
///
/// POLICY ONLY. This struct carries the operator's signature-enforcement
/// *posture* — whether generation/image signatures are required, which
/// algorithm and sidecar suffix the verifier expects, and the UKI
/// install-time signing policy. It does NOT carry the trust material:
/// the `boot.nmbl.signing.publicKeys` are `include_bytes!`-baked into the
/// `nmbl-init` binary (generated `src/sig/baked_keys.rs`, R-5/FIX-24) and
/// are NEVER emitted to `config.toml` nor stored here. A config.toml is
/// an untrusted, writable-boot-partition artifact; baking the keys means
/// the trust anchor cannot be swapped by editing the TOML.
///
/// The `enable ⇒ enforce` direction is asserted in Nix
/// (`lib/modules/security/signing.nix`), gated by a separate deliberate
/// `secureBoot.allowAuditModeInsecure` flag (FIX-31); audit mode
/// (`enable && !enforce`) still compiles `src/sig` in (FIX-16). There is
/// deliberately NO `allow_unsigned_generations` knob anywhere (FIX-04):
/// the only relaxation of enforcement is the two-flag audit mode.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningConfig {
    /// Master switch for the signature-verification subsystem. When
    /// `false` (the default) no signatures are checked. Enabling it
    /// without `enforce` is audit mode — sidecars are parsed/verified and
    /// mismatches logged, but a bad/missing signature does not refuse the
    /// boot. Audit mode is itself gated by `secureBoot.allowAuditModeInsecure`
    /// on the Nix side (FIX-31).
    #[serde(default)]
    pub enable: bool,

    /// Fail-closed enforcement. When `true`, a bad or missing signature on
    /// a generation/image routes to `policy::refuse_unsigned`
    /// (R-1/FIX-04). When `false` with `enable = true`, the same checks
    /// run but only warn (audit mode). The Nix assertion `enable ⇒ enforce`
    /// makes the insecure audit combination require an explicit second
    /// flag, so production configs are fail-closed by construction
    /// (FIX-31).
    #[serde(default)]
    pub enforce: bool,

    /// Signature algorithm the verifier expects, as the operator-facing
    /// name emitted by Nix (e.g. `"ml-dsa-65"`, `"ml-dsa-87"`). Carried as
    /// the wire string here; the real [`crate::sig`] `AlgId` mapping and
    /// per-key length validation land in F2. Defaults to the Nix-side
    /// default so an absent `[signing]` table parses unchanged.
    #[serde(default = "default_algorithm")]
    pub algorithm: String,

    /// Filename suffix of the detached signature sidecars NMBL looks up
    /// next to each signed blob (e.g. `kernel${suffix}`). Defaults to
    /// `".sig"`. Policy only — where sidecars are *located* is fixed by
    /// the boot flow (`/boot/nmbl/sigs/<gen-id>/…`, R-4).
    #[serde(default = "default_sig_path_suffix")]
    pub sig_path_suffix: String,

    /// UKI Secure-Boot signing policy (R-9). Carries only whether install
    /// time `sbsign`/`ukify` UKI signing is requested; the actual key/cert
    /// material is read impurely at install time, never emitted here.
    #[serde(default)]
    pub uki: UkiSigningConfig,
}

/// `[signing.uki]` sub-table. INSTALL-TIME policy for Secure-Boot signing
/// of the NMBL UKI (R-9). The `keyFile`/`certFile` paths are consumed by
/// `lib/install-signing.nix` at install time; they are NOT part of the
/// runtime trust path and so are NOT carried as struct fields here — only
/// the enable bit is policy the runtime might surface.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UkiSigningConfig {
    /// Whether the install step signs the NMBL UKI with the firmware
    /// `db`-enrolled Secure-Boot key. Defaults to `false`. The
    /// `keyFile`/`certFile` are install-time-only and never reach the
    /// runtime config, so this sub-table is policy-presence only.
    #[serde(default)]
    pub enable: bool,
}

fn default_algorithm() -> String {
    "ml-dsa-65".to_owned()
}

fn default_sig_path_suffix() -> String {
    ".sig".to_owned()
}

impl Default for SigningConfig {
    fn default() -> Self {
        Self {
            enable: false,
            enforce: false,
            algorithm: default_algorithm(),
            sig_path_suffix: default_sig_path_suffix(),
            uki: UkiSigningConfig::default(),
        }
    }
}
