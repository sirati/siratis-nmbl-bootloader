//! [`TpmOps`] for [`DryRunSys`]: side-effect-free TPM presence / state
//! reads and NO-OP mutating ops for `--validate-initrm`.
//!
//! This is the Property-6 boundary: the dry-run must NEVER open `/dev/tpm*`,
//! extend a real PCR, or poison the irreversible lock PCR. So every mutating
//! op (`tpm_transmit`, `pcr_extend`, `cap_lock_pcr`) records a finding and
//! returns a benign no-op result rather than touching the hardware. The two
//! read ops are synthetic: `tpm_present` reports a TPM ONLY when the closure
//! actually ships the sysfs node (so a dry-run on a closure without a TPM
//! tree does not pretend one is present), and `read_sb_state` reads the
//! `SecureBoot` efivar through the closure, degrading to `Unreadable` when
//! it is absent.

use std::path::Path;

use crate::error::Result;
use crate::tpm::presence::TPM_SYSFS_CLASS;
use crate::tpm::{CapOutcome, SbEfiState};

use super::DryRunSys;
use super::report::MissingFile;
use crate::sys::ops::TpmOps;

impl TpmOps for DryRunSys {
    fn tpm_present(&self) -> bool {
        // Synthetic: a TPM is "present" for the dry-run iff the closure
        // ships the kernel sysfs class node. Never probes the live host.
        self.closure().exists(Path::new(TPM_SYSFS_CLASS))
    }

    fn tpm_transmit(&mut self, _command: &[u8]) -> Result<Vec<u8>> {
        // NEVER open /dev/tpmrm0. Record + return an empty response; no
        // dry-run consumer parses it (the seal cap routes through
        // `cap_lock_pcr`, which no-ops below).
        self.record(MissingFile::new(
            "tpm_transmit",
            Path::new("/dev/tpmrm0"),
            "dry-run: TPM transmit suppressed (no real device opened)",
        ));
        Ok(Vec::new())
    }

    fn pcr_extend(&mut self, index: u32, _digest: &[u8]) -> Result<()> {
        // NEVER extend a real PCR. Record + succeed so the measure flow
        // continues as if the extend landed.
        self.record(MissingFile::new(
            "pcr_extend",
            Path::new("/dev/tpmrm0"),
            format!("dry-run: PCR-{index} extend suppressed (no real device opened)"),
        ));
        Ok(())
    }

    fn read_sb_state(&self) -> SbEfiState {
        // Read the SecureBoot efivar THROUGH the closure (side-effect-free);
        // absence degrades to Unreadable exactly as the real reader does on
        // a BIOS/CSM box.
        crate::tpm::sbstate::classify_secure_boot_bytes(
            self.closure()
                .read_file(&crate::tpm::sbstate::secure_boot_efivar_path())
                .ok()
                .as_deref(),
        )
    }

    fn cap_lock_pcr(&mut self) -> CapOutcome {
        // THE Property-6 boundary: NEVER perform the irreversible lock-PCR
        // poison-extend. Record + return `NoTpm` (vacuous cap) so the seal's
        // `cap_step` degrades open on a dry-run rather than diverting to a
        // refuse it cannot honour. No real PCR is ever touched.
        self.record(MissingFile::new(
            "cap_lock_pcr",
            Path::new("/dev/tpmrm0"),
            "dry-run: lock-PCR cap suppressed (no real PCR poisoned)",
        ));
        // cap-exempt: the dry-run NEVER opens a TPM nor poisons a PCR, so the
        // cap is vacuous by construction; returning NoTpm keeps the seal's
        // degrade-open path (it cannot, and must not, fail-closed into a real
        // refuse from a side-effect-free validation run).
        CapOutcome::NoTpm
    }
}
