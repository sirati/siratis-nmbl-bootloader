use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use nmbl_host_tools::{domain, sign};
use sha2::{Digest, Sha512};

use crate::manifest::{Entry, Manifest, Role};
use crate::{Error, Result, validate};

pub fn prepare(
    slot: char,
    source: &Path,
    output: &Path,
    private_key: &Path,
    public_key: &Path,
) -> Result<()> {
    if !matches!(slot, 'A' | 'B') {
        return Err(Error::Invalid("slot must be A or B".into()));
    }
    reject_store_private_key(private_key)?;
    fs::create_dir(output).map_err(|e| Error::io("create output bundle", e))?;
    fs::set_permissions(output, fs::Permissions::from_mode(0o700))
        .map_err(|e| Error::io("protect output bundle", e))?;
    let result = populate(slot, source, output, private_key, public_key);
    if result.is_err() {
        let _ = fs::remove_dir_all(output);
    }
    result
}

fn populate(
    slot: char,
    source: &Path,
    output: &Path,
    private_key: &Path,
    public_key: &Path,
) -> Result<()> {
    let roles = [
        (Role::Bootloader, "bootloader", true),
        (Role::Kernel, "kernel", true),
        (Role::Initrd, "initrd", true),
        (Role::Rescue, "rescue", true),
        (Role::Config, "config", true),
        (Role::Network, "network", false),
    ];
    let mut entries = Vec::new();
    let mut identity = Sha512::new();
    identity.update([slot as u8]);
    for (role, name, required) in roles {
        let source_file = source.join(name);
        if !source_file.exists() && !required {
            continue;
        }
        copy_regular(&source_file, &output.join(name))?;
        let digest = sha512(&output.join(name))?;
        identity.update(name.as_bytes());
        identity.update(digest.as_bytes());
        let signature = format!("{name}.sig");
        sign::run(
            &output.join(name),
            private_key,
            domain::domain_for("boot-set-artifact")
                .ok_or_else(|| Error::Invalid("artifact signature domain missing".into()))?,
            Some(&output.join(&signature)),
        )
        .map_err(|e| Error::Invalid(format!("sign {name}: {e}")))?;
        entries.push(Entry {
            role,
            destination: name.into(),
            payload: name.into(),
            signature,
            domain: "boot-set-artifact".into(),
            sha512: digest,
        });
    }
    let manifest = Manifest {
        version: 1,
        set_id: format!("{:x}", identity.finalize()),
        target_slot: slot.to_string(),
        files: entries,
    };
    let bytes = serde_json::to_vec(&manifest)
        .map_err(|e| Error::Invalid(format!("encode manifest: {e}")))?;
    write_new(&output.join("manifest.json"), &bytes)?;
    sign::run(
        &output.join("manifest.json"),
        private_key,
        domain::domain_for("boot-set-manifest")
            .ok_or_else(|| Error::Invalid("manifest signature domain missing".into()))?,
        Some(&output.join("manifest.json.sig")),
    )
    .map_err(|e| Error::Invalid(format!("sign manifest: {e}")))?;
    // The producer independently consumes its finished output before it can
    // be handed to request(), which performs the next unprivileged check.
    validate::bundle(output, public_key)?;
    Ok(())
}

fn reject_store_private_key(path: &Path) -> Result<()> {
    let canonical = path
        .canonicalize()
        .map_err(|e| Error::io("resolve private key", e))?;
    if canonical.starts_with("/nix/store") {
        return Err(Error::Invalid(
            "private key must remain outside /nix/store".into(),
        ));
    }
    Ok(())
}

fn copy_regular(source: &Path, destination: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(source).map_err(|e| Error::io("inspect source", e))?;
    if !metadata.file_type().is_file() {
        return Err(Error::Invalid(format!(
            "source is not a regular file: {}",
            source.display()
        )));
    }
    let mut input = fs::File::open(source).map_err(|e| Error::io("open source", e))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(destination)
        .map_err(|e| Error::io("create bundle member", e))?;
    std::io::copy(&mut input, &mut output).map_err(|e| Error::io("copy bundle member", e))?;
    output
        .sync_all()
        .map_err(|e| Error::io("sync bundle member", e))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::io("create manifest", e))?;
    file.write_all(bytes)
        .map_err(|e| Error::io("write manifest", e))?;
    file.sync_all().map_err(|e| Error::io("sync manifest", e))
}

fn sha512(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(|e| Error::io("open copied member", e))?;
    let mut hasher = Sha512::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| Error::io("hash copied member", e))?;
    Ok(format!("{:x}", hasher.finalize()))
}
