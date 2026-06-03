//! The signed priority-file boot gate (#31 — FIX-08/FIX-26/FIX-34/FIX-35).
//!
//! The PRIORITY GATE is the secure-boot core (feature #5): before NMBL will
//! proceed to a measured boot or consume a staged fragment it reads a signed
//! file at a configured path on the FIRST-loaded volume and verifies it against
//! the baked trust anchor, NARROWED to the operator's `allowed_key_ids` on the
//! FULL 32-byte fingerprint (FIX-08), under the pinned domain
//! [`DOMAIN_PRIORITY_FILE`] (`b"nmbl:priority-file:v1"`). The verify then drives
//! a THREE-WAY branch:
//!
//! * **(a) valid signature** ⇒ proceed, returning the unforgeable
//!   [`AttestedVolume`] witness (FIX-26) that #33's `apply_staged_boot` REQUIRES
//!   by type — the volume cannot be consumed without passing this gate.
//! * **(b) missing / wrong / bad signature** ⇒ [`super::refuse_unsigned`] relocks
//!   then reboots into rescue via
//!   [`crate::terminal::TerminalAction::RebootIntoRescue`]. The boot is refused
//!   AND rescue is gated behind a reboot — the next boot re-evaluates the
//!   still-bad image with the TPM kept locked.
//! * **(c) sentinel present** (an empty `/boot/nmbl/rescue`) ⇒ force rescue with
//!   the TPM kept locked, taking the SAME `rescue::dispatch` path the
//!   force-on-boot trigger does (this is the sentinel-aware union from
//!   [`super::sentinel::should_force_rescue`] — checked by the caller before the
//!   gate runs, see MED-1).
//!
//! Posture: **enforce** ⇒ fail closed; **audit** (`enable && !enforce`, itself
//! gated by `allowAuditModeInsecure` — FIX-31) ⇒ verify + WARN but proceed;
//! **off** (`enable = false`) ⇒ the gate is skipped and the legacy path runs.
//!
//! ## Two hooks (FIX-34) + the pre-console deferral (FIX-35)
//!
//! [`run_priority_gate_at`] runs at the TWO boot points via a [`GatePhase`]:
//! [`GatePhase::PrePlainBoot`] (the plain-boot-FS gate, before any interactive
//! work) and [`GatePhase::PostUnlock`] (the inside-LUKS gate, after the storage
//! activations open the priority volume's backing mapper). BOTH defer their
//! refuse: instead of relocking inline they return `Err(NmblError::PolicyRefused)`
//! so the EXISTING `run_tui_session` Err arm renders the countdown through the
//! one shared `run_refuse_screen` entry — NEVER the shell-offering emergency
//! path (FIX-35). The relock/seal/countdown all happen there.

use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use crate::config::{Config, PriorityVolume};
use crate::error::{NmblError, Result};
use crate::sig::{
    DOMAIN_PRIORITY_FILE, FullFp, SigSidecar, VerifyPolicy, parse_baked_keys, resolve_allowed_keys,
    verify_digest,
};
use crate::util::hash;

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
#[path = "gate_tests.rs"]
mod tests;

/// Which of the two boot points the gate is running at (FIX-34).
///
/// The phase selects WHERE the priority file is read from and labels the log
/// line; it does NOT change the refuse semantics — both phases defer their
/// refuse to the shared `run_tui_session` Err arm (FIX-35).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GatePhase {
    /// The plain-boot-FS gate, run with a live console but before any
    /// interactive work. The priority file lives on the already-mounted boot
    /// FS (or a non-LUKS priority volume the gate mounts itself).
    PrePlainBoot,
    /// The inside-LUKS gate, run after the storage activations opened the
    /// priority volume's backing mapper.
    PostUnlock,
}

impl GatePhase {
    /// Whether this phase should run for the configured priority volume. The
    /// pre-plain-boot phase handles a volume that is NOT `inside_luks` (the
    /// boot FS is already up); the post-unlock phase handles the `inside_luks`
    /// volume that only appears after an activation.
    fn matches(self, vol: &PriorityVolume) -> bool {
        match self {
            Self::PrePlainBoot => !vol.inside_luks,
            Self::PostUnlock => vol.inside_luks,
        }
    }
}

/// The attested-volume witness (FIX-26): proof that the priority gate PASSED.
///
/// Holds the mountpoint the signed file was read from and, when the gate
/// mounted the volume itself, the mount lifetime — its [`Drop`] best-effort
/// unmounts a gate-owned mount. `apply_staged_boot` (#33, Wave-3) REQUIRES an
/// `AttestedVolume` by value, so the staged path is structurally unreachable
/// without passing this gate. There is no public constructor: the only way to
/// obtain one is [`run_priority_gate`] returning `Ok`.
#[derive(Debug)]
pub struct AttestedVolume {
    /// Mountpoint the verified priority file was read from. `apply_staged_boot`
    /// resolves the staged image + fragment relative to this.
    mountpoint: PathBuf,
    /// `Some` when the gate mounted the volume (Drop unmounts it); `None` when
    /// the file lives on an already-mounted FS (the plain boot FS) the gate
    /// must NOT tear down.
    owned_mount: Option<PathBuf>,
}

impl AttestedVolume {
    /// The mountpoint the attested file was read from.
    #[must_use]
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for AttestedVolume {
    fn drop(&mut self) {
        // Only a gate-owned mount is torn down; an existing FS (the boot FS) is
        // left for the rest of the boot. Best-effort + lazy: a busy unmount
        // must never panic in Drop.
        if let Some(mp) = self.owned_mount.take()
            && let Err(e) = crate::sys::mount::umount(&mp, nix::mount::MntFlags::MNT_DETACH)
        {
            crate::nmbl_warn!(
                "priority-gate: could not unmount attested volume {}: {e}",
                mp.display()
            );
        }
    }
}

/// The PURE outcome of evaluating the priority file, decoupled from the refuse
/// action so the 3-way branch is unit-testable without a live runtime.
#[derive(Debug)]
enum GateDecision {
    /// (a) valid signature ⇒ proceed with this attested volume.
    Attested(AttestedVolume),
    /// (b) the verify FAILED under an ENFORCING posture ⇒ refuse the boot,
    /// carrying the originating cause for the refuse banner.
    Refuse(NmblError),
    /// The verify failed but the posture is AUDIT (or the gate is off): proceed
    /// with the attested volume anyway, having WARNED. Carries the volume so the
    /// caller still gets a witness.
    AuditProceed(AttestedVolume),
}

/// Run the priority gate at one boot point (FIX-34). Returns the
/// [`AttestedVolume`] witness on pass; on a refuse it returns
/// `Err(NmblError::PolicyRefused{..})` so the caller's `run_tui_session` Err arm
/// renders the countdown through the shared `run_refuse_screen` entry — the
/// refuse is NEVER taken inline here (FIX-35).
///
/// `Ok(None)` means the gate did not apply this phase (no priority volume, the
/// volume belongs to the other phase, or secure-boot is disabled): the caller
/// simply continues the legacy boot path.
pub fn run_priority_gate_at(phase: GatePhase, config: &Config) -> Result<Option<AttestedVolume>> {
    if !config.secure_boot.enable {
        return Ok(None);
    }
    let Some(vol) = config.secure_boot.priority_volume.as_ref() else {
        // `enable` with no priority volume is a config that names no file to
        // gate on; nothing for this phase to do. (The Nix `enable ⇒ keys`
        // assertion still guarantees a trust anchor exists for #33.)
        return Ok(None);
    };
    if !phase.matches(vol) {
        return Ok(None);
    }

    crate::nmbl_info!(
        "priority-gate ({phase:?}): verifying signed file {} on {}",
        config.secure_boot.signed_file_path.display(),
        vol.mountpoint.display()
    );

    match evaluate(phase, config, vol) {
        GateDecision::Attested(vol) => {
            crate::nmbl_info!("priority-gate ({phase:?}): signature VALID, proceeding");
            Ok(Some(vol))
        }
        GateDecision::AuditProceed(vol) => Ok(Some(vol)),
        // FIX-35: do NOT relock/refuse here. Surface PolicyRefused so the
        // shared run_tui_session Err arm renders the countdown.
        GateDecision::Refuse(cause) => Err(NmblError::PolicyRefused {
            cause: Box::new(cause),
        }),
    }
}

/// The single-shot priority gate (FIX-26): the `PostUnlock` hook unwrapped to
/// the bare [`AttestedVolume`], used where there is exactly one gate point (the
/// boot path calls [`run_priority_gate_at`] at each of the two phases instead).
pub fn run_priority_gate(config: &Config) -> Result<AttestedVolume> {
    match run_priority_gate_at(GatePhase::PostUnlock, config)? {
        Some(vol) => Ok(vol),
        None => Err(NmblError::Signature {
            stage: "priority-gate-not-configured",
            detail: "run_priority_gate requires an inside-LUKS priority volume".to_string(),
        }),
    }
}

/// Evaluate the priority file into a [`GateDecision`]. Mounts the volume (when
/// the gate owns the mount), reads the signed file + sidecar, narrows the keys
/// on the FULL fingerprint, and verifies under [`DOMAIN_PRIORITY_FILE`]. A
/// verify failure under an enforcing posture is a [`GateDecision::Refuse`]; under
/// audit it WARNs and proceeds with the (still-mounted) attested volume.
fn evaluate(phase: GatePhase, config: &Config, vol: &PriorityVolume) -> GateDecision {
    let attested = match mount_priority_volume(phase, config, vol) {
        Ok(a) => a,
        // No attested volume to keep: a mount failure is a hard refuse under
        // enforce (the trust-anchor file is unreachable, indistinguishable from a
        // removed/tampered file) and a no-witness proceed under audit.
        Err(e) => return decide_no_volume(config, e),
    };

    let signed_path = attested
        .mountpoint
        .join(&config.secure_boot.signed_file_path);
    match verify_signed_file(config, &signed_path) {
        Ok(()) => GateDecision::Attested(attested),
        Err(cause) if enforcing(config) => {
            crate::nmbl_warn!(
                "priority-gate: signature check FAILED under enforcement; refusing boot: {cause}"
            );
            GateDecision::Refuse(cause)
        }
        Err(cause) => {
            crate::nmbl_warn!(
                "priority-gate: signature check failed but AUDIT mode is active; proceeding (INSECURE): {cause}"
            );
            GateDecision::AuditProceed(attested)
        }
    }
}

/// Decide a failure that left no attested volume (a mount error). Enforcing ⇒
/// refuse; audit ⇒ proceed with a path-less placeholder witness (the staged
/// path is unreachable in audit-only configs anyway — `staged.enable ⇒
/// secure_boot.enable` is enforced, FIX-26).
fn decide_no_volume(config: &Config, cause: NmblError) -> GateDecision {
    if enforcing(config) {
        crate::nmbl_warn!(
            "priority-gate: could not mount/read the priority volume under enforcement; \
             refusing boot: {cause}"
        );
        GateDecision::Refuse(cause)
    } else {
        crate::nmbl_warn!(
            "priority-gate: could not mount/read the priority volume but AUDIT mode is active; \
             proceeding (INSECURE): {cause}"
        );
        GateDecision::AuditProceed(AttestedVolume {
            mountpoint: PathBuf::new(),
            owned_mount: None,
        })
    }
}

/// Resolve the priority volume to an [`AttestedVolume`] mountpoint. For the
/// pre-plain-boot phase the file lives on the already-mounted boot FS, so we
/// resolve against `runtime_boot_mountpoint` and DO NOT mount (no owned mount).
/// For the post-unlock (inside-LUKS) phase the gate mounts the configured
/// device read-only at the volume's mountpoint and owns that mount.
fn mount_priority_volume(
    phase: GatePhase,
    config: &Config,
    vol: &PriorityVolume,
) -> Result<AttestedVolume> {
    match phase {
        GatePhase::PrePlainBoot => {
            let mountpoint = config
                .runtime_boot_mountpoint
                .clone()
                .unwrap_or_else(|| vol.mountpoint.clone());
            Ok(AttestedVolume {
                mountpoint,
                owned_mount: None,
            })
        }
        GatePhase::PostUnlock => {
            std::fs::create_dir_all(&vol.mountpoint).map_err(|source| NmblError::Io {
                source,
                context: format!(
                    "priority-gate: create mountpoint {}",
                    vol.mountpoint.display()
                ),
            })?;
            // Always mount READ-ONLY (the gate never trusts the volume's own
            // options for the mount it performs).
            let opts = ensure_ro(&vol.options);
            crate::sys::mount::mount_fs(Some(&vol.device), &vol.mountpoint, &vol.fstype, &opts)?;
            Ok(AttestedVolume {
                mountpoint: vol.mountpoint.clone(),
                owned_mount: Some(vol.mountpoint.clone()),
            })
        }
    }
}

/// Force `ro` into a mount-options string (idempotent): the gate ALWAYS mounts
/// the priority volume read-only regardless of the configured options.
fn ensure_ro(options: &str) -> String {
    if options.split(',').any(|o| o == "ro") {
        options.to_string()
    } else if options.is_empty() {
        "ro".to_string()
    } else {
        format!("ro,{options}")
    }
}

/// Verify the signed priority file at `path` against the NARROWED key set under
/// [`DOMAIN_PRIORITY_FILE`] (FIX-08). Opens the file ONCE, streams it through
/// SHA-512 over the single pinned fd, reads its sidecar (`<path><suffix>`), and
/// delegates to the pure [`verify_priority_against`] with the BAKED key set.
fn verify_signed_file(config: &Config, path: &Path) -> Result<()> {
    let file = std::fs::File::open(path).map_err(|source| NmblError::Io {
        source,
        context: format!("priority-gate: open signed file {}", path.display()),
    })?;
    let (digest, _len) = hash::sha512_fd(file.as_fd())?;

    let sig_path = sidecar_path(path, &config.signing.sig_path_suffix);
    let sig_bytes = std::fs::read(&sig_path).map_err(|source| NmblError::Io {
        source,
        context: format!("priority-gate: read sidecar {}", sig_path.display()),
    })?;

    let baked = parse_baked_keys()?;
    verify_priority_against(config, &digest, &sig_bytes, &baked)
}

/// The PURE narrowed-key verify core (FIX-08): parse the sidecar, narrow `baked`
/// to the operator's FULL 32-byte fingerprints, and verify `digest` under the
/// pinned [`DOMAIN_PRIORITY_FILE`] domain. Factored out of [`verify_signed_file`]
/// so the FULL-fp narrowing and domain pinning are unit-testable with a crafted
/// key set (the real `BAKED_KEYS` static is empty in this build).
///
/// Narrows ONLY on the full fingerprint: a key matching merely a short prefix of
/// an allowed id is NOT in the narrowed set, so a prefix-collision key is
/// rejected. The domain is pinned to `priority-file:v1`, so a sidecar minted for
/// any other role fails the domain-cross check before a key is even tried.
fn verify_priority_against(
    config: &Config,
    digest: &[u8; 64],
    sig_bytes: &[u8],
    baked: &[crate::sig::BakedKey],
) -> Result<()> {
    let sidecar = SigSidecar::parse(sig_bytes).map_err(|e| NmblError::Signature {
        stage: "priority-sidecar-parse",
        detail: format!("priority-file sidecar: {e}"),
    })?;
    let allowed = allowed_fingerprints(config)?;
    let narrowed: Vec<_> = resolve_allowed_keys(baked, &allowed)
        .into_iter()
        .cloned()
        .collect();
    verify_digest(
        digest,
        DOMAIN_PRIORITY_FILE,
        &sidecar,
        &narrowed,
        VerifyPolicy::from_config(config),
    )
}

/// The sidecar path for a signed image: `<path><suffix>` (the configured
/// `signing.sig_path_suffix`, default `.sig`), matching how the signer and the
/// generation verify path name their siblings.
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Parse the operator's `allowed_key_ids` (hex fingerprint strings) into the
/// FULL 32-byte `FullFp` set the gate narrows on (FIX-08). A malformed entry is
/// a hard error — an unparseable fingerprint must NOT silently widen trust. An
/// EMPTY list means "no restriction" (the whole baked set; the policy layer
/// enforces the ≥2-keys ⇒ list-required rule, FIX-54).
fn allowed_fingerprints(config: &Config) -> Result<Vec<FullFp>> {
    let mut out = Vec::with_capacity(config.secure_boot.allowed_key_ids.len());
    for id in &config.secure_boot.allowed_key_ids {
        let fp = crate::util::hex::decode_fixed::<32>(id).ok_or_else(|| NmblError::Signature {
            stage: "priority-allowed-key-id",
            detail: format!("allowed_key_id {id:?} is not a 64-char hex full fingerprint (FIX-08)"),
        })?;
        out.push(fp);
    }
    Ok(out)
}

/// Whether the gate must FAIL CLOSED on a bad signature (enforce posture). When
/// `enable && !enforce` the gate is in audit mode and only WARNs (FIX-31).
fn enforcing(config: &Config) -> bool {
    config.secure_boot.enforce
}
