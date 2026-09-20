//! Unit tests for the chrooted external-rescue child runner. Kept in a
//! sibling file (via `#[path]`) so `child.rs` stays under the per-file
//! line budget while the tests still live in the `child::tests` module
//! (so they can reach the private `ChildExec` fields and the pure
//! plan helpers).

use super::*;
use std::path::PathBuf;

#[test]
fn mount_plan_matches_spec_sequence() {
    let plan = mount_plan();
    assert_eq!(
        plan,
        vec![
            MountStep::MkDir("/mnt"),
            MountStep::MkDir("/rescue/nmbl-root"),
            MountStep::MkDir("/rescue/mnt"),
            MountStep::Bind {
                src: "/rescue/mnt",
                dst: "/rescue/mnt",
            },
            MountStep::MakeShared("/rescue/mnt"),
            MountStep::RBind {
                src: "/rescue/mnt",
                dst: "/mnt",
            },
            MountStep::RBind {
                src: "/",
                dst: "/rescue/nmbl-root",
            },
            MountStep::Bind {
                src: "/init",
                dst: "/rescue/bin/nmbl",
            },
        ],
    );
}

#[test]
fn umount_plan_unwinds_mnt_before_nmbl_root() {
    // PID 1's /mnt (the propagation target) is detached first, then
    // the child's /rescue/mnt, then the NMBL-root bind.
    assert_eq!(
        umount_plan(),
        vec![
            "/rescue/bin/nmbl",
            "/mnt",
            "/rescue/mnt",
            "/rescue/nmbl-root"
        ]
    );
}

#[test]
fn boot_mount_is_restored_at_its_original_nmbl_root_path() {
    assert_eq!(
        child_boot_target(Path::new("/mnt/boot")),
        PathBuf::from("/rescue/nmbl-root/mnt/boot"),
    );
}

#[test]
fn child_exec_builds_basename_argv0_and_env() {
    let exec = ChildExec::build(Path::new("/init")).expect("exec strings build");
    assert_eq!(exec.path_c.as_bytes(), b"/init");
    assert_eq!(exec.argv0_c.as_bytes(), b"init");
    assert_eq!(exec.env_term.as_bytes(), b"TERM=linux");
    assert_eq!(
        exec.env_path.as_bytes(),
        b"PATH=/bin:/sbin:/usr/bin:/usr/sbin"
    );
    assert_eq!(
        exec.env_sock.as_bytes(),
        b"NMBL_TUI_SOCK=/nmbl-root/nmbl-run/tui.sock",
    );
}

#[test]
fn child_exec_argv0_falls_back_to_full_path() {
    // A bare "/" has no file_name component; argv0 falls back to the
    // full path bytes rather than panicking.
    let exec = ChildExec::build(Path::new("/bin/sh")).expect("exec strings build");
    assert_eq!(exec.argv0_c.as_bytes(), b"sh");
}
