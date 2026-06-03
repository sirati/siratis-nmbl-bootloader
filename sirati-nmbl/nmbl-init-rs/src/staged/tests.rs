//! Tests for the staged-boot apply: the transactional merge (FIX-32), the
//! fragment-can't-relax-policy guarantee (FIX-53), and the disabled no-op.
//!
//! The crypto-verify path (`verify::verify_staged_blobs`) and the full
//! re-run-effects path need either baked keys or a live runtime, so they are
//! exercised by the VM E2E matrix (#57); the unit tests here pin the
//! security-load-bearing pure logic: the merge's all-or-nothing transaction and
//! the schema-level rejection of any policy table in a fragment.

use crate::config::{Config, ConfigFragment};

use super::merge::merge_fragment;

/// A base config with a distinctive `[general]` + one plain filesystem, used to
/// prove a failed merge leaves the base byte-for-byte unchanged.
fn base_config() -> Config {
    let toml = "\
        [general]\n\
        timeout_ms = 4242\n\
        [paths]\n\
        shell = \"/bin/base-sh\"\n\
        [[filesystems]]\n\
        device = \"/dev/sda1\"\n\
        mountpoint = \"/\"\n\
        fstype = \"ext4\"\n";
    Config::parse_toml(toml, std::path::Path::new("/etc/nmbl/config.toml")).expect("base parses")
}

#[test]
fn merge_applies_a_valid_fragment() {
    // A fragment that replaces [general] and adds a benign filesystem merges
    // cleanly and the merged config takes effect.
    let mut config = base_config();
    let frag = ConfigFragment::parse_toml(
        "\
        [general]\n\
        timeout_ms = 9001\n\
        [[filesystems]]\n\
        device = \"/dev/sdb1\"\n\
        mountpoint = \"/data\"\n\
        fstype = \"ext4\"\n",
        std::path::Path::new("/frag.toml"),
    )
    .expect("fragment parses");

    merge_fragment(&mut config, frag).expect("valid fragment merges");

    assert_eq!(config.general.timeout_ms, 9001, "general table replaced");
    assert_eq!(config.filesystems.len(), 1, "filesystems table replaced");
    assert_eq!(config.filesystems[0].mountpoint.to_str(), Some("/data"));
    // A table the fragment did NOT mention is untouched.
    assert_eq!(config.paths.shell.to_str(), Some("/bin/base-sh"));
}

#[test]
fn merge_leaves_base_untouched_on_none_tables() {
    // An empty fragment mentions no table: every base field stays exactly as it
    // was (the `None` arm never swaps).
    let mut config = base_config();
    let frag = ConfigFragment::parse_toml("", std::path::Path::new("/frag.toml"))
        .expect("empty fragment parses");

    merge_fragment(&mut config, frag).expect("empty fragment merges");

    assert_eq!(config.general.timeout_ms, 4242);
    assert_eq!(config.filesystems.len(), 1);
    assert_eq!(config.filesystems[0].device.as_str(), "/dev/sda1");
    assert_eq!(config.paths.shell.to_str(), Some("/bin/base-sh"));
}

#[test]
fn merge_rolls_back_on_invalid_candidate_transactional() {
    // FIX-32: a fragment that makes the MERGED config invalid (a /dev/mapper
    // filesystem no activation can produce) must fail AND leave the base config
    // byte-for-byte unchanged — no partial apply.
    let mut config = base_config();
    let frag = ConfigFragment::parse_toml(
        "\
        [general]\n\
        timeout_ms = 1\n\
        [[filesystems]]\n\
        device = \"/dev/mapper/unbacked\"\n\
        mountpoint = \"/secret\"\n\
        fstype = \"ext4\"\n",
        std::path::Path::new("/frag.toml"),
    )
    .expect("fragment parses (validation is the merge's job)");

    let err = merge_fragment(&mut config, frag).expect_err("invalid candidate must refuse");
    assert!(
        matches!(err, crate::error::NmblError::ConfigInvalid { .. }),
        "expected ConfigInvalid, got {err:?}"
    );

    // The base is PRISTINE: both the table the fragment tried to replace AND the
    // [general] table it also carried are exactly the originals.
    assert_eq!(
        config.general.timeout_ms, 4242,
        "general must be rolled back, not left at the fragment's 1"
    );
    assert_eq!(config.filesystems.len(), 1, "filesystems rolled back");
    assert_eq!(
        config.filesystems[0].device.as_str(),
        "/dev/sda1",
        "the unbacked mapper fs must not survive the failed merge"
    );
}

#[test]
fn merge_rollback_restores_every_swapped_table() {
    // A fragment that carries SEVERAL tables but fails validation must restore
    // ALL of them, not just the one that triggered the failure — the whole
    // transaction unwinds.
    let mut config = base_config();
    let frag = ConfigFragment::parse_toml(
        "\
        [general]\n\
        timeout_ms = 7\n\
        [paths]\n\
        shell = \"/bin/frag-sh\"\n\
        [[filesystems]]\n\
        device = \"/dev/mapper/nope\"\n\
        mountpoint = \"/x\"\n\
        fstype = \"ext4\"\n",
        std::path::Path::new("/frag.toml"),
    )
    .expect("fragment parses");

    merge_fragment(&mut config, frag).expect_err("invalid candidate must refuse");

    assert_eq!(config.general.timeout_ms, 4242, "general rolled back");
    assert_eq!(
        config.paths.shell.to_str(),
        Some("/bin/base-sh"),
        "paths rolled back even though it was valid"
    );
}

#[test]
fn fragment_rejects_a_signing_policy_table() {
    // FIX-53: a fragment can never relax enforcement. The omitted `[signing]`
    // table is an unknown field, so `deny_unknown_fields` rejects it at PARSE
    // time — it never reaches the merge.
    let err = ConfigFragment::parse_toml(
        "[signing]\nenable = false\nenforce = false\n",
        std::path::Path::new("/frag.toml"),
    )
    .expect_err("a [signing] table must be rejected");
    assert!(matches!(err, crate::error::NmblError::Config { .. }));
}

#[test]
fn fragment_rejects_a_secure_boot_policy_table() {
    // FIX-53: same for `[secure_boot]` — a fragment cannot widen the trust
    // anchor or disable enforcement.
    let err = ConfigFragment::parse_toml(
        "[secure_boot]\nenable = false\nenforce = false\n",
        std::path::Path::new("/frag.toml"),
    )
    .expect_err("a [secure_boot] table must be rejected");
    assert!(matches!(err, crate::error::NmblError::Config { .. }));
}

#[test]
fn fragment_rejects_a_staged_self_repoint_table() {
    // FIX-53: the fragment cannot re-point the staged source it was loaded
    // through — the `[staged]` table is omitted, so even naming it is a hard
    // parse error.
    let err = ConfigFragment::parse_toml(
        "[staged]\nenable = true\nimage = \"x\"\nfragment = \"y\"\nsig = \"z\"\n",
        std::path::Path::new("/frag.toml"),
    )
    .expect_err("a [staged] table must be rejected");
    assert!(matches!(err, crate::error::NmblError::Config { .. }));
}

#[test]
fn staged_boot_is_disabled_without_a_table() {
    // No `[staged]` table ⇒ the production short-circuit predicate is false, so
    // `apply_staged_boot` returns `Ok(empty)` without touching the attested
    // volume. This is the EXACT guard the entry point branches on.
    let config = base_config();
    assert!(config.staged.is_none());
    assert!(
        !super::staged_boot_enabled(&config),
        "an absent [staged] table must be a no-op"
    );
}

#[test]
fn staged_boot_is_disabled_when_enable_is_false() {
    // A present `[staged]` table with `enable = false` is still a no-op.
    let toml = "\
        [paths]\n\
        shell = \"/bin/sh\"\n\
        [staged]\n\
        enable = false\n\
        image = \"nmbl/staged.sfs\"\n\
        fragment = \"nmbl/frag.toml\"\n\
        sig = \"nmbl/frag.toml.sig\"\n";
    let config = Config::parse_toml(toml, std::path::Path::new("/c.toml")).expect("config parses");
    assert!(config.staged.is_some(), "table present");
    assert!(
        !super::staged_boot_enabled(&config),
        "enable = false must be a no-op"
    );
}

#[test]
fn staged_boot_is_enabled_when_table_opts_in() {
    // The positive: `enable = true` flips the guard on so the apply pipeline
    // runs (the verify/merge/re-run path the VM matrix then exercises E2E).
    let toml = "\
        [paths]\n\
        shell = \"/bin/sh\"\n\
        [staged]\n\
        enable = true\n\
        image = \"nmbl/staged.sfs\"\n\
        fragment = \"nmbl/frag.toml\"\n\
        sig = \"nmbl/frag.toml.sig\"\n";
    let config = Config::parse_toml(toml, std::path::Path::new("/c.toml")).expect("config parses");
    assert!(super::staged_boot_enabled(&config));
}
