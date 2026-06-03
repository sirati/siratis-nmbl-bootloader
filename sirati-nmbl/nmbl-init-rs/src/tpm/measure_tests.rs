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

/// The fixed, ordered driver-image set the with-drivers golden vector pins
/// (#28). Two images with stable names + deterministic digests, in load
/// order: a host predictor folds these into measure event #4 identically.
fn golden_driver_images() -> Vec<DriverImageRef> {
    vec![
        DriverImageRef {
            name: "nmbl/nic.sfs".to_string(),
            digest: kdig(),
        },
        DriverImageRef {
            name: "nmbl/gpu.sfs".to_string(),
            digest: idig(),
        },
    ]
}

/// #28: the driver-image refs threaded into the measure are the loader's
/// VERIFIED digests, folded VERBATIM into event #4 — `event_digest` over
/// `name_len(be32) ‖ name ‖ digest`, never re-hashed. An independent manual
/// reconstruction of the full event list (incl. the two driver events) must
/// equal `predict_handoff_pcr` over the same ordered set.
#[test]
fn golden_pcr11_with_drivers_matches_independent_reconstruction() {
    let images = golden_driver_images();
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
    // The two driver events, hand-framed exactly as `handoff_events` does:
    // `name_len(be32) ‖ name ‖ 64-byte digest` under the driver-image domain.
    for img in &images {
        let mut body: Vec<u8> = Vec::new();
        let name = img.name.as_bytes();
        body.extend_from_slice(&u32::try_from(name.len()).unwrap().to_be_bytes());
        body.extend_from_slice(name);
        body.extend_from_slice(&img.digest);
        let mut h = Sha256::new();
        h.update(b"nmbl:measure:driver-image:v1");
        h.update([0x00]);
        h.update(&body);
        manual.push(h.finalize().into());
    }
    let mut pcr = [0u8; 32];
    for e in &manual {
        let mut h = Sha256::new();
        h.update(pcr);
        h.update(e);
        pcr = h.finalize().into();
    }
    let got = predict_handoff_pcr(&kdig(), &idig(), GOLDEN_CMDLINE, &images);
    assert_eq!(
        got, pcr,
        "with-drivers prediction must equal the independent reconstruction",
    );
    // The with-drivers value MUST differ from the no-drivers golden: the two
    // driver events really moved PCR-11.
    let none = predict_handoff_pcr(&kdig(), &idig(), GOLDEN_CMDLINE, &[]);
    assert_ne!(got, none, "driver events must change the measured PCR");
}

/// #28: the golden PCR-11 hex for the frozen inputs WITH the two-image
/// ordered driver set, pinned as a literal. A drift in the driver-event
/// encoding, the name framing, the digest reuse, or the order moves this and
/// fails the test; a host predictor seals to exactly this value.
#[test]
fn golden_pcr11_with_drivers_matches_frozen() {
    let pcr = predict_handoff_pcr(&kdig(), &idig(), GOLDEN_CMDLINE, &golden_driver_images());
    let hexv = hex_lower(&pcr);
    assert_eq!(hexv.len(), 64, "PCR-11 is a 32-byte SHA-256 value");
    assert_eq!(
        hexv, FROZEN_PCR11_WITH_DRIVERS,
        "with-drivers golden PCR-11 drifted; if the encoding changed \
         intentionally, update FROZEN_PCR11_WITH_DRIVERS (and any off-box \
         predictor) to {hexv}"
    );
}

/// The frozen golden PCR-11 for `golden_pcr11_with_drivers_matches_frozen`:
/// the same fixed kernel/initrd/cmdline plus the two-image ordered driver
/// set `golden_driver_images()`. Pinned; a host predictor reproduces it.
const FROZEN_PCR11_WITH_DRIVERS: &str =
    "96c9a3d392bfff0b87f21084f7931c53061213d5a76a9d863183b82f02cf0cbf";
