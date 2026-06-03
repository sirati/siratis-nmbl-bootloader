//! Unit tests for the Secure-Boot state awareness gate (FIX-11).
//!
//! The four cases the plan pins (#29):
//!   * SB-enabled            -> Proceed
//!   * SB-disabled + enforce -> Refuse
//!   * SB-disabled + audit   -> Warn (proceed)
//!   * efivar-absent         -> Warn (degrade)
//!
//! The efivar reader is exercised over a tempdir fixture carrying a crafted
//! `SecureBoot-<GUID>` file (the 4-byte attribute header + the state byte), so
//! the parse path is tested without a real efivarfs. The decision logic is
//! pure ([`decide_sb_action`]) and tested directly.

use std::path::Path;

use super::{EFI_GLOBAL_GUID, SbAction, SbEfiState, decide_sb_action, read_secure_boot_efivar_at};

/// Write a `SecureBoot-<GUID>` efivar file under `dir` with the standard 4-byte
/// little-endian attribute header followed by `state` (or no state byte when
/// `state` is `None`, to model a malformed/short body).
fn write_sb_efivar(dir: &Path, state: Option<u8>) {
    let mut body: Vec<u8> = vec![0x07, 0x00, 0x00, 0x00]; // NV+BS+RT attributes
    if let Some(s) = state {
        body.push(s);
    }
    let path = dir.join(format!("SecureBoot-{EFI_GLOBAL_GUID}"));
    std::fs::write(&path, &body).expect("write sb efivar fixture");
}

#[test]
fn efivar_enabled_reads_enabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_sb_efivar(dir.path(), Some(1));
    assert_eq!(read_secure_boot_efivar_at(dir.path()), SbEfiState::Enabled);
}

#[test]
fn efivar_disabled_reads_disabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_sb_efivar(dir.path(), Some(0));
    assert_eq!(read_secure_boot_efivar_at(dir.path()), SbEfiState::Disabled);
}

#[test]
fn efivar_absent_reads_unreadable() {
    let dir = tempfile::tempdir().expect("tempdir");
    // No file written: efivarfs-less / BIOS-CSM box.
    assert_eq!(
        read_secure_boot_efivar_at(dir.path()),
        SbEfiState::Unreadable
    );
}

#[test]
fn efivar_short_body_is_unreadable() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Attribute header but no state byte -> too short to trust.
    write_sb_efivar(dir.path(), None);
    assert_eq!(
        read_secure_boot_efivar_at(dir.path()),
        SbEfiState::Unreadable
    );
}

#[test]
fn efivar_out_of_range_value_is_unreadable() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_sb_efivar(dir.path(), Some(0x42));
    assert_eq!(
        read_secure_boot_efivar_at(dir.path()),
        SbEfiState::Unreadable
    );
}

/// SB enabled proceeds regardless of the enforce posture.
#[test]
fn enabled_proceeds_in_both_postures() {
    assert_eq!(
        decide_sb_action(SbEfiState::Enabled, true),
        SbAction::Proceed
    );
    assert_eq!(
        decide_sb_action(SbEfiState::Enabled, false),
        SbAction::Proceed
    );
}

/// SB disabled + enforce -> Refuse (fail-closed): positive proof the firmware
/// is not refusing unsigned images, so an unprotected measured boot is refused.
#[test]
fn disabled_enforce_refuses() {
    assert_eq!(
        decide_sb_action(SbEfiState::Disabled, true),
        SbAction::Refuse
    );
}

/// SB disabled + audit (enforce off) -> Warn (proceed).
#[test]
fn disabled_audit_warns() {
    assert_eq!(
        decide_sb_action(SbEfiState::Disabled, false),
        SbAction::Warn
    );
}

/// efivar absent / unreadable -> Warn (degrade-open) in BOTH postures: we lack
/// positive proof SB is NOT enforcing, so we never refuse a BIOS/efivarfs-less
/// box — we warn loudly and proceed, mirroring the no-TPM degrade.
#[test]
fn unreadable_degrades_open_in_both_postures() {
    assert_eq!(
        decide_sb_action(SbEfiState::Unreadable, true),
        SbAction::Warn
    );
    assert_eq!(
        decide_sb_action(SbEfiState::Unreadable, false),
        SbAction::Warn
    );
}

/// End-to-end over the efivar fixture path: a disabled-SB efivar under enforce
/// produces the refuse decision; under audit it produces warn.
#[test]
fn fixture_drives_action() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_sb_efivar(dir.path(), Some(0));
    let state = read_secure_boot_efivar_at(dir.path());
    assert_eq!(decide_sb_action(state, true), SbAction::Refuse);
    assert_eq!(decide_sb_action(state, false), SbAction::Warn);
}
