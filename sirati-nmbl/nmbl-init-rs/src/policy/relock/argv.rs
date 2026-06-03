//! Kind-aware relock-command derivation (FIX-47).
//!
//! Deriving `cryptsetup close <basename(produces_devices)>` is fragile:
//! `produces_devices` is arbitrary (`/dev/mapper/<name>`, `/dev/<vg>/<lv>`,
//! `/dev/md0`, `/dev/disk/by-id/…`), and a wrong basename yields a
//! best-effort WARN no-op while the audit believes the volume was locked.
//! [`relock_argv`] instead derives a STRICTLY-shaped command per activation
//! kind and loud-warns (returning `None`, so the refuse never runs a bogus
//! command) on any shape it does not recognise:
//!
//! * **LUKS** (`LuksTpm`/`LuksKeyfile`/`LuksPassword`): accept ONLY a
//!   `/dev/mapper/<name>` produced device — strip the prefix, never a
//!   generic basename — and run `cryptsetup close <name>`.
//! * **LVM**: deactivate the VG with `vgchange -an <vg>`, where `<vg>` is
//!   the volume-group component of a `/dev/mapper/<vg>-<lv>` or
//!   `/dev/<vg>/<lv>` produced device.
//! * **mdraid**: require a `/dev/md*` produced device and `mdadm --stop
//!   <md>`.
//! * **ZFS**: no relock (a refuse reboot resets the pool import); skipped.
//!
//! The activation already carries the `cryptsetup` binary it forked
//! (`activation.binary`) for the LUKS kinds, so the LUKS relock reuses it
//! verbatim rather than re-resolving a path; LVM/mdraid use the standard
//! tool names on `PATH` (matching how the activation's own open argv is
//! emitted).

use std::path::{Path, PathBuf};

use crate::config::{Activation, ActivationKind};

/// A validated, ready-to-run relock command. Built by [`relock_argv`] from
/// exactly one activation; `None` is returned for activations with no
/// relock shape (or a malformed `produces_devices`), so a caller never runs
/// a guessed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelockCommand {
    /// Binary to fork (the activation's own `cryptsetup` for LUKS, or
    /// `vgchange`/`mdadm` for LVM/mdraid).
    pub binary: PathBuf,
    /// Argv (NOT including `argv[0]`; the process runner supplies it).
    pub argv: Vec<String>,
    /// Operator-facing label for the warn/info line (the volume name).
    pub label: String,
    /// Exit code the tool returns for "nothing to deactivate / already
    /// inactive", treated as benign success alongside `0`. `cryptsetup
    /// close` exits 4 for an inactive mapper; `vgchange`/`mdadm` have no
    /// distinct already-inactive code, so `0` is the only success there
    /// and a non-zero exit is a real signal.
    pub absent_exit_code: i32,
}

/// Derive the relock command for `act`, or `None` if the kind has no
/// relock or `produces_devices` is not a shape we can safely lock.
#[must_use]
pub fn relock_argv(act: &Activation) -> Option<RelockCommand> {
    match act.kind {
        ActivationKind::LuksTpm | ActivationKind::LuksKeyfile | ActivationKind::LuksPassword => {
            luks_relock(act)
        }
        ActivationKind::Lvm => lvm_relock(act),
        ActivationKind::Mdraid => mdraid_relock(act),
        // A ZFS pool is re-imported clean after the refuse reboot; there is
        // no in-initramfs "relock" that adds security over the reset.
        ActivationKind::Zfs => None,
    }
}

/// LUKS: `cryptsetup close <name>` for the FIRST `/dev/mapper/<name>`
/// produced device. Accept ONLY that exact prefix (FIX-47) — a
/// `/dev/<vg>/<lv>` or by-id path here would mean a mis-emitted config, so
/// we loud-warn and return `None` rather than guess a mapper name.
fn luks_relock(act: &Activation) -> Option<RelockCommand> {
    let mapper = act
        .produces_devices
        .iter()
        .map(PathBuf::as_path)
        .find_map(mapper_name)?;
    if mapper.is_none_warned(act, "luks") {
        return None;
    }
    let name = mapper.into_name();
    Some(RelockCommand {
        binary: act.binary.clone(),
        argv: vec!["close".to_string(), name.clone()],
        label: format!("LUKS mapper {name}"),
        // cryptsetup exit 4 == "device <name> is not active".
        absent_exit_code: 4,
    })
}

/// LVM: `vgchange -an <vg>`, deriving `<vg>` from a `/dev/mapper/<vg>-<lv>`
/// or `/dev/<vg>/<lv>` produced device. Loud-warns and returns `None` on a
/// shape we cannot parse a VG from (matching the LUKS arm — a mis-shaped
/// device must be loud, not a silent no-relock — LOW-1).
fn lvm_relock(act: &Activation) -> Option<RelockCommand> {
    let Some(vg) = act
        .produces_devices
        .iter()
        .map(PathBuf::as_path)
        .find_map(vg_of)
    else {
        warn_unrecognized(act, "lvm", "a /dev/<vg>/<lv> or /dev/mapper/<vg>-<lv>");
        return None;
    };
    Some(RelockCommand {
        binary: PathBuf::from("vgchange"),
        argv: vec!["-an".to_string(), vg.clone()],
        label: format!("LVM volume group {vg}"),
        absent_exit_code: 0,
    })
}

/// mdraid: `mdadm --stop <md>`, requiring a `/dev/md*` produced device.
/// Loud-warns and returns `None` when no `/dev/md*` node is present (LOW-1).
fn mdraid_relock(act: &Activation) -> Option<RelockCommand> {
    let Some(md) = act
        .produces_devices
        .iter()
        .map(PathBuf::as_path)
        .find(|p| is_md_node(p))
    else {
        warn_unrecognized(act, "mdraid", "a /dev/md*");
        return None;
    };
    let md = md.to_string_lossy().into_owned();
    Some(RelockCommand {
        binary: PathBuf::from("mdadm"),
        argv: vec!["--stop".to_string(), md.clone()],
        label: format!("md array {md}"),
        absent_exit_code: 0,
    })
}

/// Loud-warn that an activation's `produces_devices` had no shape this kind
/// could safely relock, so NO relock is run (LOW-1). The LUKS arm already
/// warns via `MapperParse::is_none_warned`; this gives LVM/mdraid the same
/// audible signal instead of a silent `None`.
fn warn_unrecognized(act: &Activation, kind: &str, wanted: &str) {
    crate::nmbl_warn!(
        "refuse: {kind} activation {:?} produced no {wanted} device; \
         NOT relocking (could not derive a safe target)",
        act.description
    );
}

/// A `/dev/mapper/<name>` device, parsed into its bare `<name>`. Returns
/// `None` for any non-`/dev/mapper/` path so the caller can skip it.
fn mapper_name(p: &Path) -> Option<MapperParse> {
    let s = p.to_str()?;
    let name = s.strip_prefix("/dev/mapper/")?;
    if name.is_empty() || name.contains('/') {
        return Some(MapperParse::Malformed);
    }
    Some(MapperParse::Name(name.to_string()))
}

/// Result of parsing a `/dev/mapper/` device into a relockable name.
enum MapperParse {
    Name(String),
    Malformed,
}

impl MapperParse {
    /// Loud-warn + `true` when the parse is malformed (so the caller bails
    /// rather than run `cryptsetup close ""`).
    fn is_none_warned(&self, act: &Activation, kind: &str) -> bool {
        if matches!(self, MapperParse::Malformed) {
            crate::nmbl_warn!(
                "refuse: {kind} activation {:?} has a malformed /dev/mapper device; \
                 NOT relocking (could not derive a safe mapper name)",
                act.description
            );
            true
        } else {
            false
        }
    }

    fn into_name(self) -> String {
        match self {
            MapperParse::Name(n) => n,
            // Unreachable: callers gate on `is_none_warned` first.
            MapperParse::Malformed => String::new(),
        }
    }
}

/// Volume-group name from a `/dev/mapper/<vg>-<lv>` or `/dev/<vg>/<lv>`
/// path. LVM mangles a literal `-` in a VG/LV name to `--` in the
/// `/dev/mapper` form, so the `/dev/<vg>/<lv>` symlink form is parsed first
/// (it is unambiguous); the mapper form is split on the LAST single `-`
/// only as a fallback.
fn vg_of(p: &Path) -> Option<String> {
    let s = p.to_str()?;
    if let Some(rest) = s.strip_prefix("/dev/")
        && let Some((vg, lv)) = rest.split_once('/')
        && !vg.is_empty()
        && !lv.is_empty()
        && vg != "mapper"
    {
        return Some(vg.to_string());
    }
    // `/dev/mapper/<vg>-<lv>` fallback: the VG is everything before the
    // first single (un-doubled) `-`. We only need the VG, and `vgchange
    // -an <vg>` deactivates the whole group regardless of which LV we saw.
    let name = s.strip_prefix("/dev/mapper/")?;
    vg_from_mangled(name)
}

/// Split a `<vg>-<lv>` device-mapper name (with LVM's `--` escaping of a
/// literal `-`) at the first single `-`, returning the un-escaped `<vg>`.
/// Iterator-driven (no indexing) so it stays panic-free.
fn vg_from_mangled(name: &str) -> Option<String> {
    let mut vg = String::new();
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' {
            if chars.peek() == Some(&'-') {
                // Escaped literal dash (`--`): emit one `-`, consume both.
                vg.push('-');
                chars.next();
                continue;
            }
            // Single dash: end of the VG component.
            return (!vg.is_empty()).then_some(vg);
        }
        vg.push(c);
    }
    // No single `-` separator found: not a `<vg>-<lv>` mapper name.
    None
}

/// Whether `p` is a `/dev/md*` array node.
fn is_md_node(p: &Path) -> bool {
    p.to_str().is_some_and(|s| s.starts_with("/dev/md"))
}
