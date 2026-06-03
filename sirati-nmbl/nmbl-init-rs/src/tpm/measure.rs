//! PCR-11 measured-boot handoff (`secure-boot`-gated — #27 / FIX-12 / FIX-42).
//!
//! After the generation signature gate passes and BEFORE the image is loaded
//! ([`crate::boot::handoff::verify_measure_then_load`]), NMBL extends the lock
//! PCR with the boot handoff so a TPM-sealed secret is bound to the EXACT
//! `{kernel, initrd, cmdline, driver-images}` that is about to run. The events
//! are extended in a FIXED order; a host predictor reconstructs the same value
//! to seal against (FIX-12).
//!
//! ## PCR-11 entry binding (FIX-12)
//!
//! NMBL does NOT trust whatever the launching stub did or did not measure into
//! PCR-11. Instead NMBL SELF-EXTENDS a defined "NMBL-identity" marker as the
//! FIRST event of every handoff ([`measure_event::IDENTITY`]). This anchors the
//! log to an unambiguous, NMBL-controlled starting point: from the perspective
//! of a host predictor the measured sequence is
//! `extend(identity) · extend(kernel) · extend(initrd) · extend(cmdline) ·
//! extend(image_i …)`, independent of any firmware/stub pre-measurement. The
//! sealing tool (a host-side `systemd-measure predict` over the SAME committed
//! event list, or NMBL's own deterministic entry-extend) replays exactly these
//! events; the forbidden ~30-line hand-rolled firmware/stub replay is NOT used.
//!
//! ## What PCR-11 attests (FIX-42)
//!
//! `{kernel, pristine-initrd, driver-images, cmdline}` — explicitly NOT the
//! NMBL-injected cpio fragment (the log transcript + typed key material spliced
//! into the initrd at [`crate::boot::handoff`]). The initrd digest measured
//! here is of the PRISTINE on-disk initrd over the same pinned fd the verifier
//! hashed (FIX-02), so the fragment — which carries NMBL-internal,
//! attacker-uninfluenceable bytes — is intentionally outside the attested set.
//!
//! ## Event encoding (deterministic — FIX-42)
//!
//! Each event is a SHA-256 digest extended into the SHA-256 PCR bank (the bank
//! [`super::pcr_extend`] uses). The digest is `SHA-256(domain || 0x00 || body)`
//! where `domain` is a per-event ASCII tag and `body` is the event payload (the
//! reused SHA-512 generation digest, the NUL-terminated cmdline bytes, or an
//! ordered driver-image digest). The domain separation makes a kernel-digest
//! event un-substitutable for an initrd-digest event, and the encoding is fully
//! reproducible off-box, so the golden-vector test pins the resulting PCR-11.

use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::Result;
use crate::generations::Generation;
use crate::nmbl_info;

use super::transport::TpmDevice;
use super::{LOCK_PCR, pcr_extend};

/// A driver-image reference contributed to the PCR-11 measurement (event #4).
///
/// #27 accepts a possibly-empty slice of these; #28 (Wave-4) populates it from
/// the verified driver images loaded in `boot_runtime` (the SAME SHA-512 digest
/// the image verifier streamed over its pinned fd — FIX-02, no re-hash). The
/// `name` is the stable image identifier, folded into the event so two images
/// with an identical body but a different role still measure distinctly.
#[derive(Debug, Clone)]
pub struct DriverImageRef {
    /// Stable image name (the configured driver-image identifier).
    pub name: String,
    /// The image's SHA-512 digest, as computed by the image verifier over its
    /// single pinned fd. Reused here verbatim — never recomputed (FIX-02).
    pub digest: [u8; 64],
}

/// Per-event domain tags. ASCII, versioned, and committed: the host predictor
/// MUST use the identical bytes or the prediction diverges. Single-sourced here
/// so the Rust extend and any off-box predictor share one definition.
pub mod measure_event {
    /// The NMBL-identity entry marker (FIX-12): the first event of every
    /// handoff, anchoring PCR-11 to an NMBL-controlled starting point.
    pub const IDENTITY: &[u8] = b"nmbl:measure:identity:v1";
    /// The generation kernel digest event.
    pub const KERNEL: &[u8] = b"nmbl:measure:kernel:v1";
    /// The generation initrd (pristine) digest event.
    pub const INITRD: &[u8] = b"nmbl:measure:initrd:v1";
    /// The kexec cmdline event (the byte-exact loaded buffer — FIX-14).
    pub const CMDLINE: &[u8] = b"nmbl:measure:cmdline:v1";
    /// A driver-image event (one per ordered image — #28 fills it).
    pub const DRIVER_IMAGE: &[u8] = b"nmbl:measure:driver-image:v1";
}

/// Compute the 32-byte SHA-256 event digest `SHA-256(domain || 0x00 || body)`.
///
/// The single `0x00` separator keeps `domain` and `body` unambiguous even when
/// `body` could otherwise alias a longer domain. Pure + deterministic so the
/// golden-vector test and an off-box predictor agree byte-for-byte.
fn event_digest(domain: &[u8], body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0x00]);
    hasher.update(body);
    hasher.finalize().into()
}

/// The ordered list of PCR-11 event digests for this handoff, in extend order.
///
/// Pure (no IO): builds the exact sequence [`extend_handoff`] extends, so the
/// golden-vector test can fold them through a software PCR replay and assert the
/// resulting PCR-11 without a TPM. The order is FIXED (FIX-12):
/// `identity, kernel, initrd, cmdline, driver-image[0..n]`.
fn handoff_events(
    kernel_digest: &[u8; 64],
    initrd_digest: &[u8; 64],
    cmdline: &str,
    driver_images: &[DriverImageRef],
) -> Vec<[u8; 32]> {
    let mut events = Vec::with_capacity(4 + driver_images.len());
    // (1) NMBL-identity entry marker — anchors PCR-11 (FIX-12). Body is empty;
    // the domain tag alone is the marker.
    events.push(event_digest(measure_event::IDENTITY, &[]));
    // (2) Generation kernel digest (reused from verify — FIX-02).
    events.push(event_digest(measure_event::KERNEL, kernel_digest));
    // (3) Generation initrd (pristine) digest (reused from verify — FIX-02).
    events.push(event_digest(measure_event::INITRD, initrd_digest));
    // (4) The byte-exact kexec cmdline that will be loaded (FIX-14).
    events.push(event_digest(measure_event::CMDLINE, cmdline.as_bytes()));
    // (5) Each ordered driver image: name-length-framed name || digest, so a
    // rename or reorder changes the measurement. Empty slice ⇒ no events here.
    for image in driver_images {
        let mut body = Vec::with_capacity(4 + image.name.len() + 64);
        let name = image.name.as_bytes();
        // 4-byte big-endian name length frames the variable-length name so the
        // name/digest boundary is unambiguous (an off-box predictor parses it
        // identically).
        let name_len = u32::try_from(name.len()).unwrap_or(u32::MAX);
        body.extend_from_slice(&name_len.to_be_bytes());
        body.extend_from_slice(name);
        body.extend_from_slice(&image.digest);
        events.push(event_digest(measure_event::DRIVER_IMAGE, &body));
    }
    events
}

/// Software replay of `TPM2_PCR_Extend` (SHA-256 bank) from a zero start:
/// `pcr = SHA-256(pcr || event)` for each event, beginning at 32 zero bytes.
///
/// This predicts the PCR-11 VALUE the [`extend_handoff`] sequence yields when
/// PCR-11 starts at its post-reset zero state. The golden-vector test uses it
/// to pin a deterministic hex (FIX-42); a host predictor that seals to PCR-11
/// performs the identical fold over the identical committed event list.
#[must_use]
pub fn replay_pcr(events: &[[u8; 32]]) -> [u8; 32] {
    let mut pcr = [0u8; 32];
    for event in events {
        let mut hasher = Sha256::new();
        hasher.update(pcr);
        hasher.update(event);
        pcr = hasher.finalize().into();
    }
    pcr
}

/// Predict the post-handoff PCR-11 value for the given inputs, assuming PCR-11
/// starts at the reset zero state. The committed prediction a host sealing tool
/// reproduces (FIX-12). Pure; no TPM.
#[must_use]
pub fn predict_handoff_pcr(
    kernel_digest: &[u8; 64],
    initrd_digest: &[u8; 64],
    cmdline: &str,
    driver_images: &[DriverImageRef],
) -> [u8; 32] {
    let events = handoff_events(kernel_digest, initrd_digest, cmdline, driver_images);
    replay_pcr(&events)
}

/// Extend PCR-11 with the boot handoff (#27). Returns `Ok(())` once EVERY event
/// has been extended into the live TPM; ANY transport/protocol failure is a
/// hard `Err` so the caller fails closed on a measure-required build.
///
/// The events are extended in the FIXED order [`handoff_events`] builds:
/// identity marker, kernel digest, initrd digest, cmdline, then each ordered
/// driver image. `kernel_digest`/`initrd_digest` are the SHA-512 digests the
/// verifier already computed over its pinned fds (FIX-02) — passed through, not
/// recomputed. `cmdline` is the byte-exact buffer the load consumes (FIX-14).
///
/// Posture gating (measure-on vs measure-off) and fail-closed routing live in
/// the caller ([`crate::boot::handoff`]); this function only performs the
/// extends and reports success/failure. It opens `/dev/tpmrm0` ONCE and reuses
/// the handle for every extend.
pub fn extend_handoff(
    _config: &Config,
    generation: &Generation,
    kernel_digest: &[u8; 64],
    initrd_digest: &[u8; 64],
    cmdline: &str,
    driver_images: &[DriverImageRef],
) -> Result<()> {
    let events = handoff_events(kernel_digest, initrd_digest, cmdline, driver_images);
    let dev = TpmDevice::open()?;
    for event in &events {
        pcr_extend(&dev, LOCK_PCR, event)?;
    }
    nmbl_info!(
        "measure: extended PCR-{} with {} handoff event(s) for generation {} (kernel+initrd+cmdline+{} image(s))",
        LOCK_PCR,
        events.len(),
        generation.number,
        driver_images.len(),
    );
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests assert on exact event digests and the golden PCR-11 value"
)]
mod tests {
    use super::*;
    use crate::util::hex::hex_lower;

    fn kdig() -> [u8; 64] {
        let mut d = [0u8; 64];
        for (i, b) in d.iter_mut().enumerate() {
            *b = i as u8;
        }
        d
    }

    fn idig() -> [u8; 64] {
        let mut d = [0u8; 64];
        for (i, b) in d.iter_mut().enumerate() {
            *b = (0xff - i) as u8;
        }
        d
    }

    /// The fixed cmdline used by the golden-vector tests below.
    const GOLDEN_CMDLINE: &str = "init=/sbin/init root=/dev/sda1 ro quiet";

    /// FIX-42: the prediction is DETERMINISTIC for fixed inputs — two calls
    /// over the same inputs return the identical value.
    #[test]
    fn golden_pcr11_is_deterministic() {
        let pcr = predict_handoff_pcr(&kdig(), &idig(), GOLDEN_CMDLINE, &[]);
        let again = predict_handoff_pcr(&kdig(), &idig(), GOLDEN_CMDLINE, &[]);
        assert_eq!(pcr, again, "prediction must be deterministic");
    }

    /// FIX-42: an INDEPENDENT manual reconstruction of the event encoding +
    /// SHA-256 PCR fold (the golden vector) must equal `predict_handoff_pcr`.
    /// This is the off-box predictor in miniature: `SHA-256(domain‖0x00‖body)`
    /// per event, then `pcr = SHA-256(pcr‖event)` folded from a zero PCR. If
    /// the production encoding drifts from this hand-rolled one, the two
    /// diverge and the test fails — exactly as a real host predictor would.
    #[test]
    fn golden_pcr11_matches_independent_reconstruction() {
        // Hand-built events (no reuse of the production `event_digest`/order):
        let mut manual: Vec<[u8; 32]> = Vec::new();
        for (domain, body) in [
            (&b"nmbl:measure:identity:v1"[..], &[][..]),
            (&b"nmbl:measure:kernel:v1"[..], &kdig()[..]),
            (&b"nmbl:measure:initrd:v1"[..], &idig()[..]),
            (&b"nmbl:measure:cmdline:v1"[..], GOLDEN_CMDLINE.as_bytes()),
        ] {
            let mut h = Sha256::new();
            h.update(domain);
            h.update([0x00]);
            h.update(body);
            manual.push(h.finalize().into());
        }
        let mut pcr = [0u8; 32];
        for e in &manual {
            let mut h = Sha256::new();
            h.update(pcr);
            h.update(e);
            pcr = h.finalize().into();
        }
        let got = predict_handoff_pcr(&kdig(), &idig(), GOLDEN_CMDLINE, &[]);
        assert_eq!(
            got, pcr,
            "production prediction must equal the independent reconstruction",
        );
    }

    /// The event list is in the FIXED order and length (FIX-12): identity,
    /// kernel, initrd, cmdline, then one per driver image.
    #[test]
    fn event_order_and_count() {
        let images = vec![
            DriverImageRef {
                name: "nic".to_string(),
                digest: kdig(),
            },
            DriverImageRef {
                name: "gpu".to_string(),
                digest: idig(),
            },
        ];
        let events = handoff_events(&kdig(), &idig(), "cmd", &images);
        assert_eq!(events.len(), 4 + images.len(), "4 fixed + per-image");
        // Event 0 is the identity marker (empty body under its domain).
        assert_eq!(events[0], event_digest(measure_event::IDENTITY, &[]));
        // Event 1/2 reuse the passed-in digests verbatim (FIX-02): swapping the
        // kernel/initrd digests changes events 1 and 2.
        assert_eq!(events[1], event_digest(measure_event::KERNEL, &kdig()));
        assert_eq!(events[2], event_digest(measure_event::INITRD, &idig()));
    }

    /// Domain separation: the kernel digest event and the initrd digest event
    /// differ even when the underlying digest is identical, so a kernel
    /// signature/measurement can never be replayed as an initrd one.
    #[test]
    fn kernel_initrd_events_are_domain_separated() {
        let same = kdig();
        let k = event_digest(measure_event::KERNEL, &same);
        let i = event_digest(measure_event::INITRD, &same);
        assert_ne!(k, i, "domain tags must separate kernel from initrd");
    }

    /// FIX-14: the measured value is bound to the EXACT cmdline bytes; a
    /// one-character cmdline change moves PCR-11.
    #[test]
    fn cmdline_change_moves_pcr() {
        let a = predict_handoff_pcr(&kdig(), &idig(), "init=/a", &[]);
        let b = predict_handoff_pcr(&kdig(), &idig(), "init=/b", &[]);
        assert_ne!(a, b, "cmdline must be bound into the measurement");
    }

    /// Adding a driver image changes PCR-11 (event #4 is real, not a no-op),
    /// and reordering two images changes it too (order-sensitive).
    #[test]
    fn driver_images_affect_pcr() {
        let none = predict_handoff_pcr(&kdig(), &idig(), "cmd", &[]);
        let one = predict_handoff_pcr(
            &kdig(),
            &idig(),
            "cmd",
            &[DriverImageRef {
                name: "nic".to_string(),
                digest: kdig(),
            }],
        );
        assert_ne!(none, one, "a driver image must change the measurement");

        let ab = predict_handoff_pcr(
            &kdig(),
            &idig(),
            "cmd",
            &[
                DriverImageRef {
                    name: "a".to_string(),
                    digest: kdig(),
                },
                DriverImageRef {
                    name: "b".to_string(),
                    digest: idig(),
                },
            ],
        );
        let ba = predict_handoff_pcr(
            &kdig(),
            &idig(),
            "cmd",
            &[
                DriverImageRef {
                    name: "b".to_string(),
                    digest: idig(),
                },
                DriverImageRef {
                    name: "a".to_string(),
                    digest: kdig(),
                },
            ],
        );
        assert_ne!(ab, ba, "image order must be bound into the measurement");
    }

    /// The replay starts from the zero PCR and folds `SHA-256(pcr || event)`.
    /// A hand-rolled two-event replay must match `replay_pcr`.
    #[test]
    fn replay_matches_manual_fold() {
        let e0 = [0x11u8; 32];
        let e1 = [0x22u8; 32];
        let got = replay_pcr(&[e0, e1]);

        let mut pcr = [0u8; 32];
        for e in [e0, e1] {
            let mut h = Sha256::new();
            h.update(pcr);
            h.update(e);
            pcr = h.finalize().into();
        }
        assert_eq!(got, pcr);
        // An empty event list is the untouched zero PCR.
        assert_eq!(replay_pcr(&[]), [0u8; 32]);
    }

    /// The golden PCR-11 hex for the frozen inputs is pinned as a literal
    /// (FIX-42). A drift in any domain tag, the separator, the event order, or
    /// the fold moves this value and fails the test; a host predictor seals to
    /// exactly this value.
    #[test]
    fn golden_pcr11_matches_frozen() {
        let pcr = predict_handoff_pcr(&kdig(), &idig(), GOLDEN_CMDLINE, &[]);
        let hexv = hex_lower(&pcr);
        assert_eq!(hexv.len(), 64, "PCR-11 is a 32-byte SHA-256 value");
        assert_eq!(
            hexv, FROZEN_PCR11,
            "golden PCR-11 drifted; if the encoding changed intentionally, \
             update FROZEN_PCR11 (and any off-box predictor) to {hexv}"
        );
    }

    /// The frozen golden PCR-11 for the fixed inputs in
    /// `golden_pcr11_matches_frozen`. Computed by `predict_handoff_pcr` and
    /// pinned; a host predictor reproduces it to seal.
    const FROZEN_PCR11: &str = "d9cf09b69aa5a7c669432d255f56f9758725d88118f24efb1b00728cad855421";
}
