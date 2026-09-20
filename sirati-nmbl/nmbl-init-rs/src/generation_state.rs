//! Persistent selection state for signed EROFS generations.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;

use crate::error::{NmblError, Result};

pub const ROLLBACK_CMDLINE: &str = "nmbl.rollback-after-untested-new-generation-failed";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootStateOutcome {
    Proceed,
    RolledBack,
    Rescue,
}

pub fn prepare_boot(
    root: &Path,
    automatic_rollback: bool,
    automatic_rescue: bool,
) -> Result<BootStateOutcome> {
    let active = required_id(root, "active")?;
    let attempted = optional_id(root, "attempted")?;
    let pending = optional_id(root, "pending")?;
    let tested = optional_id(root, "tested")?;

    if let Some(failed) = attempted {
        if pending.as_deref() == Some(&failed) {
            if automatic_rollback
                && let Some(good) = tested
                && good != failed
            {
                replace_link(root, "active", &good)?;
                replace_link(root, "attempted", &good)?;
                remove_state(root, "pending")?;
                atomic_text(root, "rollback-event", &format!("{failed} {good}\n"))?;
                return Ok(BootStateOutcome::RolledBack);
            }
            return if automatic_rescue {
                Ok(BootStateOutcome::Rescue)
            } else {
                Err(invalid(
                    "untested generation failed and no tested rollback exists",
                ))
            };
        }
        return if automatic_rescue {
            Ok(BootStateOutcome::Rescue)
        } else {
            Err(invalid(
                "tested generation failed and automatic rescue is disabled",
            ))
        };
    }

    replace_link(root, "attempted", &active)?;
    Ok(BootStateOutcome::Proceed)
}

pub fn mark_success(root: &Path) -> Result<()> {
    let active = required_id(root, "active")?;
    let attempted = required_id(root, "attempted")?;
    if active != attempted {
        return Err(invalid(
            "attempted generation does not match active generation",
        ));
    }
    replace_link(root, "tested", &active)?;
    remove_state(root, "pending")?;
    remove_state(root, "rollback-event")?;
    remove_state(root, "attempted")
}

fn optional_id(root: &Path, name: &str) -> Result<Option<String>> {
    match fs::read_link(root.join(name)) {
        Ok(target) => parse_target(root, name, &target).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(io(source, format!("reading generation state {name}"))),
    }
}

fn required_id(root: &Path, name: &str) -> Result<String> {
    optional_id(root, name)?.ok_or_else(|| invalid(&format!("generation state {name} is missing")))
}

fn parse_target(root: &Path, name: &str, target: &Path) -> Result<String> {
    let text = target
        .to_str()
        .ok_or_else(|| invalid("generation link is not UTF-8"))?;
    let id = text
        .strip_prefix("generations/")
        .ok_or_else(|| invalid("generation link has an unsafe target"))?;
    if id.len() != 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(invalid("generation link has an invalid content id"));
    }
    let dir = root.join("generations").join(id);
    if !dir.join("nix.erofs").is_file() || !dir.join("nix.erofs.sig").is_file() {
        return Err(invalid(&format!(
            "generation state {name} points to an incomplete bundle"
        )));
    }
    Ok(id.to_owned())
}

fn replace_link(root: &Path, name: &str, id: &str) -> Result<()> {
    let temporary = root.join(format!(".{name}.new.{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    symlink(format!("generations/{id}"), &temporary)
        .map_err(|e| io(e, format!("creating generation state {name}")))?;
    fs::rename(&temporary, root.join(name))
        .map_err(|e| io(e, format!("publishing generation state {name}")))?;
    sync_root(root)
}

fn atomic_text(root: &Path, name: &str, text: &str) -> Result<()> {
    let temporary = root.join(format!(".{name}.new.{}", std::process::id()));
    fs::write(&temporary, text).map_err(|e| io(e, format!("writing generation event {name}")))?;
    let file = fs::File::open(&temporary)
        .map_err(|e| io(e, format!("opening generation event {name}")))?;
    rustix::fs::fsync(&file)
        .map_err(|e| io(e.into(), format!("syncing generation event {name}")))?;
    fs::rename(&temporary, root.join(name))
        .map_err(|e| io(e, format!("publishing generation event {name}")))?;
    sync_root(root)
}

fn remove_state(root: &Path, name: &str) -> Result<()> {
    match fs::remove_file(root.join(name)) {
        Ok(()) => sync_root(root),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io(error, format!("removing generation state {name}"))),
    }
}

fn sync_root(root: &Path) -> Result<()> {
    let directory =
        fs::File::open(root).map_err(|e| io(e, "opening generation state directory".into()))?;
    rustix::fs::fsync(&directory)
        .map_err(|e| io(e.into(), "syncing generation state directory".into()))
}

fn invalid(reason: &str) -> NmblError {
    NmblError::ConfigInvalid {
        reason: reason.into(),
        context: "generation-image state".into(),
    }
}

fn io(source: std::io::Error, context: String) -> NmblError {
    NmblError::Io { source, context }
}

#[cfg(test)]
#[path = "generation_state_tests.rs"]
mod tests;
