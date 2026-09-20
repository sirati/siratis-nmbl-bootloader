//! Fail-closed loading of the writable external boot configuration.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsFd;
use std::path::Path;

use crate::error::{NmblError, Result};
use crate::util::hash;

use super::keys;
use super::sidecar::SigSidecar;
use super::verify::{DOMAIN_BOOT_CONFIG, VerifyPolicy, verify_digest};

/// Verify and read `path` through one pinned file descriptor.
///
/// The signature policy is unconditionally enforcing because the policy in
/// the external configuration cannot be trusted until that file verifies.
pub fn load_verified(path: &Path, signature: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|source| NmblError::Io {
        source,
        context: format!("open external boot config {}", path.display()),
    })?;
    let (digest, _) = hash::sha512_fd(file.as_fd())?;
    let signature_bytes = fs::read(signature).map_err(|source| NmblError::Io {
        source,
        context: format!("read boot-config signature {}", signature.display()),
    })?;
    let sidecar = SigSidecar::parse(&signature_bytes).map_err(|error| NmblError::Signature {
        stage: "boot-config-sidecar",
        detail: error.to_string(),
    })?;
    let baked = keys::parse_baked_keys()?;
    verify_digest(
        &digest,
        DOMAIN_BOOT_CONFIG,
        &sidecar,
        &baked,
        VerifyPolicy::Enforce,
    )?;

    file.seek(SeekFrom::Start(0))
        .map_err(|source| NmblError::Io {
            source,
            context: format!("rewind verified boot config {}", path.display()),
        })?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|source| NmblError::Io {
            source,
            context: format!("read verified boot config {}", path.display()),
        })?;
    Ok(text)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "test setup and assertions may panic on failure"
)]
mod tests {
    use super::*;

    #[test]
    fn missing_sidecar_refuses_before_config_is_consumed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config = directory.path().join("config.toml");
        fs::write(&config, "not valid toml").expect("write fixture");
        let error = load_verified(&config, &directory.path().join("config.toml.sig"))
            .expect_err("an unsigned external config must be refused");
        assert!(error.to_string().contains("signature"), "{error}");
    }
}
