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
//! 6. Show the computed hash to the operator and let them confirm
//!    against the pre-filled expected value.
//! 7. Loop-mount the memfd and layer a writable overlay at `/rescue`,
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
    mount_overlay_for_child(&memfd).map_err(NetAttemptOutcome::Fatal)
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

    /// Anything that needs a real DHCP server / loop device / pivot
    /// is documented here as a discoverable smoke-marker so a future
    /// VM-based integration suite can flip the gate.
    #[test]
    #[ignore = "needs CAP_NET_ADMIN/CAP_NET_RAW + a DHCP server + loop devices"]
    fn try_network_rescue_full_flow_smoke() {}
}
