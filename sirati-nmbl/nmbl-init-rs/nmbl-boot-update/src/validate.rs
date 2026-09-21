use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use cap_std::ambient_authority;
use cap_std::fs::{Dir, File};
use nmbl_host_tools::{domain, verify};
use sha2::{Digest, Sha512};

use crate::manifest::{Entry, Manifest};
use crate::{Error, Result};

pub struct ValidatedBundle {
    pub manifest: Manifest,
    pub manifest_bytes: Vec<u8>,
    pub manifest_signature: Vec<u8>,
    pub artifacts: Vec<Artifact>,
    pub allocated_bytes: u64,
}

pub struct Artifact {
    pub entry_index: usize,
    pub payload: File,
    pub signature: File,
    pub payload_len: u64,
    pub signature_len: u64,
}

pub fn bundle(path: &Path, public_key: &Path) -> Result<ValidatedBundle> {
    let dir = Dir::open_ambient_dir(path, ambient_authority())
        .map_err(|e| Error::io(format!("open bundle {}", path.display()), e))?;
    let manifest_bytes = read_limited(&dir, "manifest.json", 1024 * 1024)?;
    let manifest_signature = read_limited(&dir, "manifest.json.sig", 64 * 1024)?;
    let public_bytes = std::fs::read(public_key)
        .map_err(|e| Error::io(format!("read public key {}", public_key.display()), e))?;
    let manifest_domain = domain::domain_for("boot-set-manifest")
        .ok_or_else(|| Error::Invalid("boot-set manifest domain is unavailable".into()))?;
    verify::verify_reader(
        &mut manifest_bytes.as_slice(),
        &public_bytes,
        manifest_domain,
        &manifest_signature,
    )
    .map_err(|e| Error::Signature(e.to_string()))?;

    let manifest = Manifest::parse(&manifest_bytes)?;
    let mut artifacts = Vec::with_capacity(manifest.files.len());
    let mut allocated_bytes = allocation(manifest_bytes.len() as u64)
        .checked_add(allocation(manifest_signature.len() as u64))
        .ok_or_else(|| Error::Invalid("boot set size overflow".into()))?;
    for (entry_index, entry) in manifest.files.iter().enumerate() {
        let mut payload = dir
            .open(&entry.payload)
            .map_err(|e| Error::io(format!("open payload {}", entry.payload), e))?;
        let mut signature = dir
            .open(&entry.signature)
            .map_err(|e| Error::io(format!("open signature {}", entry.signature), e))?;
        reject_non_file(&payload, &entry.payload)?;
        reject_non_file(&signature, &entry.signature)?;
        let payload_len = payload
            .metadata()
            .map_err(|e| Error::io("stat payload", e))?
            .len();
        let signature_len = signature
            .metadata()
            .map_err(|e| Error::io("stat signature", e))?
            .len();
        let signature_bytes = read_open(&mut signature, 64 * 1024)?;
        signature
            .seek(SeekFrom::Start(0))
            .map_err(|e| Error::io("rewind signature", e))?;
        verify_artifact(&mut payload, entry, &public_bytes, &signature_bytes)?;
        payload
            .seek(SeekFrom::Start(0))
            .map_err(|e| Error::io("rewind payload", e))?;
        allocated_bytes = allocated_bytes
            .checked_add(allocation(payload_len))
            .and_then(|n| n.checked_add(allocation(signature_len)))
            .ok_or_else(|| Error::Invalid("boot set size overflow".into()))?;
        artifacts.push(Artifact {
            entry_index,
            payload,
            signature,
            payload_len,
            signature_len,
        });
    }
    Ok(ValidatedBundle {
        manifest,
        manifest_bytes,
        manifest_signature,
        artifacts,
        allocated_bytes,
    })
}

fn verify_artifact(
    payload: &mut File,
    entry: &Entry,
    public_bytes: &[u8],
    signature: &[u8],
) -> Result<()> {
    let mut hasher = Sha512::new();
    std::io::copy(payload, &mut hasher).map_err(|e| Error::io("hash payload", e))?;
    let actual = format!("{:x}", hasher.finalize());
    if actual != entry.sha512.to_ascii_lowercase() {
        return Err(Error::Invalid(format!(
            "digest mismatch for {}",
            entry.payload
        )));
    }
    payload
        .seek(SeekFrom::Start(0))
        .map_err(|e| Error::io("rewind payload", e))?;
    let expected_domain = domain::domain_for(&entry.domain)
        .ok_or_else(|| Error::Invalid(format!("unknown signature domain: {}", entry.domain)))?;
    verify::verify_reader(payload, public_bytes, expected_domain, signature)
        .map_err(|e| Error::Signature(e.to_string()))
}

fn read_limited(dir: &Dir, path: &str, limit: u64) -> Result<Vec<u8>> {
    let mut file = dir
        .open(path)
        .map_err(|e| Error::io(format!("open {path}"), e))?;
    reject_non_file(&file, path)?;
    read_open(&mut file, limit)
}

fn read_open(file: &mut File, limit: u64) -> Result<Vec<u8>> {
    let len = file
        .metadata()
        .map_err(|e| Error::io("stat input", e))?
        .len();
    if len > limit {
        return Err(Error::Invalid(format!("input exceeds {limit} bytes")));
    }
    let mut bytes = Vec::with_capacity(len as usize);
    file.read_to_end(&mut bytes)
        .map_err(|e| Error::io("read input", e))?;
    Ok(bytes)
}

fn reject_non_file(file: &File, name: &str) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|e| Error::io(format!("stat {name}"), e))?;
    if !metadata.is_file() {
        return Err(Error::Invalid(format!(
            "bundle member is not a file: {name}"
        )));
    }
    Ok(())
}

fn allocation(length: u64) -> u64 {
    length.saturating_add(4095) / 4096 * 4096
}
