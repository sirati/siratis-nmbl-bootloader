use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::validate::{self, ValidatedBundle};
use crate::{Error, Result};

#[derive(Debug, Eq, PartialEq)]
pub enum Outcome {
    Activated(char),
    Unchanged(char),
}

#[derive(Default)]
pub struct Limits {
    pub available_bytes: Option<u64>,
    /// Fault-injection seam used by filesystem tests; the production CLI
    /// always leaves it unset.
    pub fail_after_files: Option<usize>,
}

pub fn install(
    bundle_path: &Path,
    public_key: &Path,
    boot_root: &Path,
    limits: &Limits,
) -> Result<Outcome> {
    // This is deliberately repeated by the daemon even after the client ran
    // the same validator. The returned open files pin the checked bytes.
    let mut bundle = validate::bundle(bundle_path, public_key)?;
    let root = boot_root.join("nmbl-boot-sets");
    fs::create_dir_all(&root).map_err(|e| Error::io("create boot-set root", e))?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
        .map_err(|e| Error::io("protect boot-set root", e))?;
    let active = read_active(&root)?;
    if let Some(slot) = active {
        let installed = root.join(slot.to_string());
        if installed.join("manifest.json").exists()
            && validate::bundle(&installed, public_key)
                .map(|v| v.manifest_bytes == bundle.manifest_bytes)
                .unwrap_or(false)
        {
            return Ok(Outcome::Unchanged(slot));
        }
    }
    let inactive = if active == Some('A') { 'B' } else { 'A' };
    if bundle.manifest.slot() != inactive {
        return Err(Error::Invalid(format!(
            "bundle targets slot {}, but inactive slot is {inactive}",
            bundle.manifest.slot()
        )));
    }
    preflight(&root, inactive, bundle.allocated_bytes, limits)?;
    let stage = root.join(format!("{inactive}.new"));
    remove_confined(&stage)?;
    fs::create_dir(&stage).map_err(|e| Error::io("create inactive staging slot", e))?;
    let staged = stage_bundle(&stage, &mut bundle, limits.fail_after_files);
    if let Err(error) = staged {
        let _ = fs::remove_dir_all(&stage);
        return Err(error);
    }
    sync_dir(&stage)?;
    // Re-open and independently verify the completed on-disk slot before it
    // can become reachable by the selector.
    validate::bundle(&stage, public_key)?;
    let destination = root.join(inactive.to_string());
    remove_confined(&destination)?;
    fs::rename(&stage, &destination).map_err(|e| Error::io("publish inactive slot", e))?;
    sync_dir(&root)?;
    write_selector(&root, inactive)?;
    Ok(Outcome::Activated(inactive))
}

fn preflight(root: &Path, inactive: char, need: u64, limits: &Limits) -> Result<()> {
    let stat =
        rustix::fs::statvfs(root).map_err(|e| Error::io("stat boot filesystem", e.into()))?;
    let available = limits
        .available_bytes
        .unwrap_or(stat.f_bavail.saturating_mul(stat.f_frsize));
    if available >= need {
        return Ok(());
    }
    let old = root.join(inactive.to_string());
    let reclaim = allocated_tree(&old)?;
    if available.saturating_add(reclaim) < need {
        return Err(Error::Invalid(format!(
            "insufficient boot space: need {need}, free {available}, reclaimable {reclaim}"
        )));
    }
    // Low-space mode reclaims only the complete inactive set. The active set
    // and selector remain untouched until a complete replacement is durable.
    remove_confined(&old)
}

fn stage_bundle(
    stage: &Path,
    bundle: &mut ValidatedBundle,
    fail_after: Option<usize>,
) -> Result<()> {
    write_bytes(&stage.join("manifest.json"), &bundle.manifest_bytes)?;
    write_bytes(&stage.join("manifest.json.sig"), &bundle.manifest_signature)?;
    for (written, artifact) in bundle.artifacts.iter_mut().enumerate() {
        if fail_after == Some(written) {
            return Err(Error::Io {
                context: "injected staged-write failure".into(),
                source: std::io::Error::from(std::io::ErrorKind::WriteZero),
            });
        }
        let entry = bundle
            .manifest
            .files
            .get(artifact.entry_index)
            .ok_or_else(|| Error::Invalid("artifact index out of range".into()))?;
        artifact
            .payload
            .seek(SeekFrom::Start(0))
            .map_err(|e| Error::io("rewind payload", e))?;
        artifact
            .signature
            .seek(SeekFrom::Start(0))
            .map_err(|e| Error::io("rewind signature", e))?;
        copy_open(
            &mut artifact.payload,
            &stage.join(&entry.destination),
            artifact.payload_len,
        )?;
        copy_open(
            &mut artifact.signature,
            &stage.join(&entry.signature),
            artifact.signature_len,
        )?;
    }
    Ok(())
}

fn copy_open(source: &mut impl Read, destination: &Path, expected: u64) -> Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .map_err(|e| Error::io("create staged artifact", e))?;
    let copied = std::io::copy(source, &mut output).map_err(|e| Error::io("copy artifact", e))?;
    if copied != expected {
        return Err(Error::Invalid("artifact changed while copying".into()));
    }
    output
        .sync_all()
        .map_err(|e| Error::io("sync staged artifact", e))
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::io("create staged metadata", e))?;
    output
        .write_all(bytes)
        .map_err(|e| Error::io("write staged metadata", e))?;
    output
        .sync_all()
        .map_err(|e| Error::io("sync staged metadata", e))
}

fn read_active(root: &Path) -> Result<Option<char>> {
    let path = root.join("active");
    let value = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::io("read active selector", e)),
    };
    match value.as_str() {
        "set nmbl_slot=A\n" => Ok(Some('A')),
        "set nmbl_slot=B\n" => Ok(Some('B')),
        _ => Err(Error::Invalid("active selector is malformed".into())),
    }
}

fn write_selector(root: &Path, slot: char) -> Result<()> {
    let temporary = root.join("active.new");
    if temporary.exists() {
        fs::remove_file(&temporary).map_err(|e| Error::io("remove stale selector", e))?;
    }
    write_bytes(&temporary, format!("set nmbl_slot={slot}\n").as_bytes())?;
    fs::rename(&temporary, root.join("active")).map_err(|e| Error::io("activate boot set", e))?;
    sync_dir(root)
}

fn allocated_tree(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for item in fs::read_dir(path).map_err(|e| Error::io("read inactive slot", e))? {
        let item = item.map_err(|e| Error::io("read inactive entry", e))?;
        let metadata = item
            .metadata()
            .map_err(|e| Error::io("stat inactive entry", e))?;
        if !metadata.is_file() {
            return Err(Error::Invalid("inactive slot contains non-file".into()));
        }
        total = total.saturating_add(metadata.blocks().saturating_mul(512));
    }
    Ok(total)
}

fn remove_confined(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(path).map_err(|e| Error::io("stat slot", e))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Error::Invalid("boot slot is not a plain directory".into()));
    }
    fs::remove_dir_all(path).map_err(|e| Error::io("remove inactive slot", e))
}

fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| Error::io("sync directory", e))
}
