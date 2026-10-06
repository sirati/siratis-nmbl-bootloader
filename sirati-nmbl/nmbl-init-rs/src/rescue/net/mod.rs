//! Network-rescue orchestrator (PLAN.md Phase E.1).
//!
//! Drives the fallback path that activates when the disk-rescue
//! arm of [`super::dispatch`] fails (or there is no `nmbl-rescue.sfs`
//! on the boot partition to begin with). The flow is, in order:
//!
//! 1. Enumerate Ethernet interfaces via [`crate::net::iface`] and
//!    pick the first one that comes up with a carrier.
//! 2. Acquire a DHCPv4 lease on that interface with
//!    [`crate::net::dhcp::acquire`].
//! 3. Configure the interface (IP, netmask, default route) with the
//!    granted lease.
//! 4. Prompt the operator (via the [`RescueUi`] trait) for the rescue
//!    URL — pre-filled from `rescue.default_url` — and the expected
//!    SHA-256 hex.
//! 5. Open a `memfd_create(2)` in-RAM fd and stream the HTTP body
//!    through `sha2::Sha256` and `rustix::io::write` in one pass.
//! 6. Seal the memfd. With signing enabled, verify it under the
//!    rescue-sfs domain against `<url>.sig`; if the config pins the
//!    stage-2 SHA-512, require that image unless the operator explicitly
//!    chooses another. A refused image is never mounted.
//! 7. Show the computed hash to the operator and let them confirm
//!    against the pre-filled expected value.
//! 8. Loop-mount the memfd and layer a writable overlay at `/rescue`,
//!    then return [`NetOutcome::RunChild`] so the dispatcher runs the
//!    rescue system as a chrooted child via
//!    [`crate::rescue::child::run_external_rescue_child`] while NMBL
//!    stays PID 1.
//!
//! [`RescueUi`] is a trait so the TUI (E.2) can later plug in a
//! ratatui-backed implementation while this module stays
//! end-to-end testable with a stdin/stdout [`ConsoleRescueUi`] or a
//! canned-answer fake.
//!
//! All failure points map onto [`NmblError::Rescue { stage, ... }`]
//! so the emergency-shell banner surfaces a structured cause.

mod console_ui;
mod download;
mod netup;
mod types;

pub use console_ui::ConsoleRescueUi;
pub use types::{DownloadStatus, HashConfirmation, RescueSource, RescueUi};

use std::path::Path;

use crate::config::Config;
use crate::error::{NmblError, Result};
use crate::net::http::HttpUrl;
use crate::terminal::TerminalAction;

use download::{download_to_memfd, mount_overlay_for_child};
use netup::{apply_lease, bring_up_and_dhcp};

/// Outcome of the network-rescue flow. Either a terminal action the
/// operator chose at the source picker (reboot / halt), or a prepared
/// writable `/rescue` overlay the caller should hand to the chrooted
/// child runner — the same runner the disk path uses, so NMBL stays
/// PID 1 and reaps the rescue system rather than execve'ing into it.
#[derive(Debug)]
pub enum NetOutcome {
    /// Perform this terminal action directly (reboot / halt).
    Action(TerminalAction),
    /// Run the chrooted rescue child against this writable `/rescue`.
    RunChild(&'static Path),
}

// ---------------------------------------------------------------------------
// Public entrypoint
// ---------------------------------------------------------------------------

/// Run the full network-rescue flow.
///
/// `disk_reason` is the formatted error chain of the disk-rescue
/// attempt that triggered the fallback; it is shown verbatim on the
/// source-picker screen. Returns a [`TerminalAction`] the dispatcher
/// in `main` performs after every stack-allocated resource is
/// dropped.
///
/// When `config.rescue.network` is `false` the function short-circuits
/// with `NmblError::Rescue { stage: "net-disabled", ... }`, letting
/// the caller fall back to a halt-with-banner.
pub fn try_network_rescue<R: RescueUi>(
    config: &Config,
    ui: &mut R,
    disk_reason: &str,
) -> Result<NetOutcome> {
    if !config.rescue.network {
        return Err(NmblError::Rescue {
            stage: "net-disabled",
            source: Box::new(NmblError::ConfigInvalid {
                reason: "network rescue is disabled in [rescue].network".to_string(),
                context: "entering try_network_rescue".to_string(),
            }),
        });
    }

    // Outer loop so the operator can redownload after a hash mismatch
    // without re-running the whole DHCP exchange.
    let mut latest_reason = disk_reason.to_string();
    loop {
        match ui.pick_source(&latest_reason)? {
            RescueSource::Reboot => return Ok(NetOutcome::Action(TerminalAction::Reboot)),
            RescueSource::Halt => {
                return Ok(NetOutcome::Action(TerminalAction::HaltWithBanner {
                    cause: NmblError::Rescue {
                        stage: "operator-halt",
                        source: Box::new(NmblError::ConfigInvalid {
                            reason: "operator chose halt from rescue source picker".to_string(),
                            context: "network-rescue UI".to_string(),
                        }),
                    },
                }));
            }
            RescueSource::Network => {}
        }

        match run_network_attempt(config, ui) {
            Ok(rescue_dir) => return Ok(NetOutcome::RunChild(rescue_dir)),
            Err(NetAttemptOutcome::Restart(reason)) => {
                // Mismatched hash / operator-aborted download — show
                // the picker again with the updated reason so they
                // know which step failed this round.
                latest_reason = reason;
                continue;
            }
            Err(NetAttemptOutcome::Fatal(e)) => return Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal flow control
// ---------------------------------------------------------------------------

/// Internal flow control for [`try_network_rescue`]. `Restart` loops
/// back to the source picker; `Fatal` aborts the whole rescue and
/// propagates the error to the caller (which will halt-with-banner).
enum NetAttemptOutcome {
    Restart(String),
    Fatal(NmblError),
}

impl From<NmblError> for NetAttemptOutcome {
    fn from(e: NmblError) -> Self {
        NetAttemptOutcome::Fatal(e)
    }
}

/// One trip through "bring up NIC + DHCP + download + verify + mount".
/// Returns the prepared writable `/rescue` overlay on the success path
/// (the caller funnels it into the chrooted child runner),
/// `NetAttemptOutcome::Restart` on operator-driven retries, and
/// `NetAttemptOutcome::Fatal` for non-recoverable errors.
fn run_network_attempt<R: RescueUi>(
    config: &Config,
    ui: &mut R,
) -> std::result::Result<&'static Path, NetAttemptOutcome> {
    let (iface, lease) = bring_up_and_dhcp()?;
    apply_lease(&iface, &lease)?;

    let prefill_url = config.rescue.default_url.as_str();
    let url_str = ui
        .prompt_url(prefill_url)
        .map_err(NetAttemptOutcome::Fatal)?;
    let url = HttpUrl::parse(&url_str).map_err(NetAttemptOutcome::Fatal)?;

    let (memfd, computed_hex) = download_to_memfd(&url, ui)?;

    // Trust before the operator's hash: the signature under the
    // rescue-sfs domain, then the stage-2 pin, both over the sealed memfd
    // that is mounted below.
    let signed_digest = verify_signature(config, &url_str, &memfd)?;
    check_stage2_pin(config, ui, &memfd, signed_digest)?;

    let prefill_hash = config.rescue.default_sha256.as_str();
    match ui
        .confirm_hash(&computed_hex, prefill_hash)
        .map_err(NetAttemptOutcome::Fatal)?
    {
        HashConfirmation::Confirmed => {}
        HashConfirmation::Mismatch => {
            // Drop the memfd by letting it fall out of scope. squashfs
            // bytes are not secret so no zeroize pass is required.
            drop(memfd);
            return Err(NetAttemptOutcome::Restart(format!(
                "hash mismatch: computed {computed_hex} did not match expected"
            )));
        }
        HashConfirmation::Aborted => {
            drop(memfd);
            return Err(NetAttemptOutcome::Restart(
                "operator aborted at hash confirmation".to_string(),
            ));
        }
    }

    // Mount the downloaded squashfs as a writable overlay at /rescue and
    // hand the path back; the caller runs the chrooted child against it.
    mount_overlay_for_child(config, &memfd, config.rescue.image.format)
        .map_err(NetAttemptOutcome::Fatal)
}

/// With signing enabled, the download must verify under the rescue-sfs
/// domain with the baked keys, against the detached signature fetched
/// from `<url><signing.sig_path_suffix>`, exactly as the disk path checks
/// `nmbl-rescue.sfs` against its sibling sidecar. Enforce refuses before
/// anything is mounted; audit warns and proceeds. Returns the SHA-512 the
/// check streamed over `image`.
#[cfg(feature = "secure-boot")]
fn verify_signature(
    config: &Config,
    url: &str,
    image: &rustix::fd::OwnedFd,
) -> std::result::Result<Option<[u8; 64]>, NetAttemptOutcome> {
    use std::os::fd::AsFd;

    if !config.signing.enable {
        return Ok(None);
    }
    let sig_url = format!("{url}{}", config.signing.sig_path_suffix);
    let result = HttpUrl::parse(&sig_url)
        .and_then(|sig_url| download::fetch_signature(&sig_url))
        .and_then(|sig| {
            crate::sig::verify_image_fd_sidecar_bytes(
                image.as_fd(),
                "downloaded rescue image",
                &sig,
                crate::sig::DOMAIN_RESCUE_SFS,
                config,
            )
        });
    let digest = result.as_ref().ok().copied();
    match crate::sig::apply_policy(config, result.map(|_| ())) {
        crate::sig::PolicyDecision::Proceed => Ok(digest),
        crate::sig::PolicyDecision::Refuse(cause) => Err(NetAttemptOutcome::Restart(format!(
            "rescue image signature refused ({sig_url}): {cause}"
        ))),
    }
}

#[cfg(not(feature = "secure-boot"))]
fn verify_signature(
    _config: &Config,
    _url: &str,
    _image: &rustix::fd::OwnedFd,
) -> std::result::Result<Option<[u8; 64]>, NetAttemptOutcome> {
    Ok(None)
}

/// If the boot configuration pins the stage-2 image, the download must be
/// that image, unless the operator explicitly chooses to boot another one.
/// `known` is the digest the signature check already streamed.
fn check_stage2_pin<R: RescueUi>(
    config: &Config,
    ui: &mut R,
    image: &rustix::fd::OwnedFd,
    known: Option<[u8; 64]>,
) -> std::result::Result<(), NetAttemptOutcome> {
    let Some(pin) = config.rescue.image.sha512.as_deref() else {
        return Ok(());
    };
    let what = "downloaded rescue image";
    let pinned = crate::rescue::image::parse_pin(pin, what)?;
    let actual = match known {
        Some(digest) => digest,
        None => crate::rescue::image::digest_of(image, what)?,
    };
    if actual == pinned {
        return Ok(());
    }
    let pinned_hex = download::hex_lower(&pinned);
    let actual_hex = download::hex_lower(&actual);
    if ui.use_unpinned_image(&pinned_hex, &actual_hex, known.is_some())? {
        crate::nmbl_warn!(
            "rescue: operator chose a rescue image other than the pinned stage-2 \
             (pinned {pinned_hex}, got {actual_hex})"
        );
        return Ok(());
    }
    Err(NetAttemptOutcome::Restart(format!(
        "downloaded rescue image is not the pinned stage-2 image (SHA-512 {actual_hex})"
    )))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::RescueConfig;
    use crate::rescue::RescueMode;
    use std::collections::VecDeque;

    /// Canned-answer UI used by the unit tests. Pushes responses
    /// into the per-method queue; methods pop the front element and
    /// fall back to a default when the queue is empty (lets tests
    /// hit only the screens they care about).
    #[derive(Default)]
    struct FakeUi {
        source_choices: VecDeque<RescueSource>,
        urls: VecDeque<String>,
        confirms: VecDeque<HashConfirmation>,
        progress_calls: u32,
        last_disk_reason: Option<String>,
        accept_unpinned: bool,
        unpinned_prompts: Vec<bool>,
    }

    impl RescueUi for FakeUi {
        fn pick_source(&mut self, disk_reason: &str) -> Result<RescueSource> {
            self.last_disk_reason = Some(disk_reason.to_string());
            Ok(self
                .source_choices
                .pop_front()
                .unwrap_or(RescueSource::Halt))
        }
        fn prompt_url(&mut self, prefill: &str) -> Result<String> {
            Ok(self.urls.pop_front().unwrap_or_else(|| prefill.to_string()))
        }
        fn progress(&mut self, _status: DownloadStatus) {
            self.progress_calls = self.progress_calls.saturating_add(1);
        }
        fn confirm_hash(
            &mut self,
            _computed_hex: &str,
            _prefill_expected: &str,
        ) -> Result<HashConfirmation> {
            Ok(self
                .confirms
                .pop_front()
                .unwrap_or(HashConfirmation::Aborted))
        }
        fn use_unpinned_image(
            &mut self,
            _pinned: &str,
            _actual: &str,
            signed: bool,
        ) -> Result<bool> {
            self.unpinned_prompts.push(signed);
            Ok(self.accept_unpinned)
        }
    }

    fn cfg_with_rescue(rescue: RescueConfig) -> Config {
        let mut c = Config::recovery_default();
        c.rescue = rescue;
        c
    }

    #[test]
    fn try_network_rescue_disabled_returns_net_disabled_error() {
        let cfg = cfg_with_rescue(RescueConfig {
            mode: RescueMode::External,
            network: false,
            ..RescueConfig::default()
        });
        let mut ui = FakeUi::default();
        let err = try_network_rescue(&cfg, &mut ui, "disk: synthetic")
            .expect_err("network=false must short-circuit");
        match err {
            NmblError::Rescue { stage, source } => {
                assert_eq!(stage, "net-disabled");
                match *source {
                    NmblError::ConfigInvalid { reason, .. } => {
                        assert!(
                            reason.contains("network rescue is disabled"),
                            "diagnostic should explain the cause, got: {reason}",
                        );
                    }
                    other => panic!("expected ConfigInvalid inside Rescue, got {other:?}"),
                }
            }
            other => panic!("expected Rescue variant, got {other:?}"),
        }
        // The UI must not have been touched — net-disabled is the
        // very first check.
        assert_eq!(ui.progress_calls, 0);
        assert!(ui.last_disk_reason.is_none());
    }

    /// Empty-input SHA-256 is RFC 6234's canonical vector — pinning
    /// it catches accidental algorithm swaps + the hex encoder.
    #[test]
    fn compute_hex_sha256_of_empty_matches_canonical() {
        assert_eq!(
            download::compute_hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        );
    }

    /// "abc" is another classic vector from FIPS 180-2; cheap second
    /// sanity check.
    #[test]
    fn compute_hex_sha256_of_abc_matches_canonical() {
        assert_eq!(
            download::compute_hex_sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
    }

    #[test]
    fn hex_lower_pads_single_byte_with_zero() {
        assert_eq!(download::hex_lower(&[0x0a]), "0a");
        assert_eq!(download::hex_lower(&[0xff, 0x00, 0x10]), "ff0010");
    }

    fn image_with(bytes: &[u8]) -> rustix::fd::OwnedFd {
        use std::io::Write;
        let mut tmp = tempfile::tempfile().expect("tempfile");
        tmp.write_all(bytes).expect("write image");
        rustix::fd::OwnedFd::from(tmp)
    }

    #[cfg(any(feature = "secure-boot", feature = "rescue-stages"))]
    fn pinned_cfg(bytes: &[u8]) -> Config {
        use sha2::{Digest, Sha512};
        let mut c = Config::recovery_default();
        c.rescue.image.sha512 = Some(download::hex_lower(&Sha512::digest(bytes)));
        c
    }

    #[cfg(any(feature = "secure-boot", feature = "rescue-stages"))]
    #[test]
    fn the_pinned_image_passes_without_asking() {
        let cfg = pinned_cfg(b"stage two");
        let mut ui = FakeUi::default();
        assert!(check_stage2_pin(&cfg, &mut ui, &image_with(b"stage two"), None).is_ok());
        assert!(ui.unpinned_prompts.is_empty());
    }

    #[cfg(any(feature = "secure-boot", feature = "rescue-stages"))]
    #[test]
    fn another_image_is_refused_unless_the_operator_chooses_it() {
        let cfg = pinned_cfg(b"stage two");
        let other = image_with(b"another stage two");
        let mut ui = FakeUi::default();
        assert!(matches!(
            check_stage2_pin(&cfg, &mut ui, &other, None),
            Err(NetAttemptOutcome::Restart(reason)) if reason.contains("not the pinned stage-2")
        ));
        assert_eq!(ui.unpinned_prompts, [false]);

        let mut ui = FakeUi {
            accept_unpinned: true,
            ..FakeUi::default()
        };
        assert!(check_stage2_pin(&cfg, &mut ui, &other, None).is_ok());
        assert_eq!(ui.unpinned_prompts, [false]);
    }

    #[test]
    fn without_a_pin_any_image_passes() {
        let cfg = Config::recovery_default();
        assert!(cfg.rescue.image.sha512.is_none());
        let mut ui = FakeUi::default();
        assert!(check_stage2_pin(&cfg, &mut ui, &image_with(b"x"), None).is_ok());
        assert!(ui.unpinned_prompts.is_empty());
    }

    #[cfg(feature = "secure-boot")]
    fn signing_cfg(enable: bool, enforce: bool) -> Config {
        let mut c = Config::recovery_default();
        c.signing.enable = enable;
        c.signing.enforce = enforce;
        c
    }

    /// Port 1 on loopback refuses the connection: the signature is
    /// missing.
    #[cfg(feature = "secure-boot")]
    const NO_SIGNATURE_URL: &str = "http://127.0.0.1:1/nmbl-rescue.sfs";

    #[cfg(feature = "secure-boot")]
    #[test]
    fn enforced_signing_refuses_a_download_without_signature() {
        let image = image_with(b"stage two");
        match verify_signature(&signing_cfg(true, true), NO_SIGNATURE_URL, &image) {
            Err(NetAttemptOutcome::Restart(reason)) => {
                assert!(reason.contains("signature refused"), "{reason}");
                assert!(reason.contains("nmbl-rescue.sfs.sig"), "{reason}");
            }
            Err(NetAttemptOutcome::Fatal(e)) => panic!("fatal instead of refused: {e}"),
            Ok(digest) => panic!("unsigned download accepted: {digest:?}"),
        }
    }

    #[cfg(feature = "secure-boot")]
    #[test]
    fn enforced_signing_refuses_a_bad_signature() {
        let image = image_with(b"stage two");
        let result = crate::sig::verify_image_fd_sidecar_bytes(
            std::os::fd::AsFd::as_fd(&image),
            "downloaded rescue image",
            b"not a signature",
            crate::sig::DOMAIN_RESCUE_SFS,
            &signing_cfg(true, true),
        );
        assert!(result.is_err());
    }

    #[cfg(feature = "secure-boot")]
    #[test]
    fn audit_and_disabled_signing_proceed_without_a_verified_digest() {
        let image = image_with(b"stage two");
        for cfg in [signing_cfg(true, false), signing_cfg(false, false)] {
            assert!(matches!(
                verify_signature(&cfg, NO_SIGNATURE_URL, &image),
                Ok(None)
            ));
        }
    }

    /// Anything that needs a real DHCP server / loop device / pivot
    /// is documented here as a discoverable smoke-marker so a future
    /// VM-based integration suite can flip the gate.
    #[test]
    #[ignore = "needs CAP_NET_ADMIN/CAP_NET_RAW + a DHCP server + loop devices"]
    fn try_network_rescue_full_flow_smoke() {}
}
