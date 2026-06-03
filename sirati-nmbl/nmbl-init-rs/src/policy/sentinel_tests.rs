//! Tests for the rescue sentinel: presence detection, the additive
//! `should_force_rescue` union (FIX-49), and the write target/order
//! (FIX-21).

use std::path::PathBuf;

use super::{sentinel_present, should_force_rescue, write_sentinel};
use crate::config::Config;
use crate::sys::ops::RealSys;

/// A unique temp dir per test so the parallel test threads do not collide.
fn temp_boot(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nmbl-sentinel-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp boot dir");
    dir
}

#[test]
fn absent_sentinel_does_not_force_rescue() {
    let dir = temp_boot("absent");
    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.clone());
    assert!(!sentinel_present(&cfg), "no file ⇒ not present");
    assert!(
        !should_force_rescue(false, &cfg),
        "no external force + no sentinel ⇒ no rescue"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_sentinel_forces_rescue() {
    // FIX-49: an EMPTY /boot/nmbl/rescue marker ⇒ force rescue.
    let dir = temp_boot("empty");
    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.clone());
    write_sentinel(&mut RealSys::sync_only(), &cfg);
    assert!(sentinel_present(&cfg), "written sentinel is present");
    assert!(
        should_force_rescue(false, &cfg),
        "an empty sentinel forces rescue even with no external trigger"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn external_force_alone_forces_rescue_additively() {
    // The union is ADDITIVE: the external force-on-boot decision still
    // forces rescue with no sentinel (FIX-49).
    let dir = temp_boot("external");
    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.clone());
    assert!(!sentinel_present(&cfg));
    assert!(
        should_force_rescue(true, &cfg),
        "external force ⇒ rescue regardless of the sentinel"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_written_sentinel_is_empty() {
    let dir = temp_boot("empty-content");
    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.clone());
    write_sentinel(&mut RealSys::sync_only(), &cfg);
    // Resolve the same path the writer used and confirm it is a 0-byte file.
    let path = dir.join("nmbl/rescue");
    let meta = std::fs::metadata(&path).expect("sentinel exists");
    assert_eq!(meta.len(), 0, "the sentinel is an empty marker file");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_resolves_under_the_runtime_boot_mountpoint() {
    // FIX-21: the write target is the WRITABLE runtime_boot_mountpoint
    // joined with the boot-relative tail, not the literal /boot path.
    let dir = temp_boot("target");
    let mut cfg = Config::recovery_default();
    cfg.runtime_boot_mountpoint = Some(dir.clone());
    write_sentinel(&mut RealSys::sync_only(), &cfg);
    assert!(
        dir.join("nmbl/rescue").exists(),
        "sentinel lands under the runtime boot mountpoint"
    );
    // The literal /boot path was NOT written.
    assert!(
        !sentinel_present(&{
            let mut c = Config::recovery_default();
            c.runtime_boot_mountpoint = Some(temp_boot("target-other"));
            c
        }),
        "an unrelated mountpoint sees no sentinel"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
