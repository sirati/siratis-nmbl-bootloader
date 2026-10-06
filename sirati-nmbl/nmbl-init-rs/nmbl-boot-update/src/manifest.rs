use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub set_id: String,
    pub target_slot: String,
    pub files: Vec<Entry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub role: Role,
    pub destination: String,
    pub payload: String,
    pub signature: String,
    pub domain: String,
    pub sha512: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Bootloader,
    Kernel,
    Initrd,
    Rescue,
    Network,
    Tools,
    Config,
}

impl Role {
    /// The signature domain the installed file is verified under. The bundle
    /// signature is installed as the slot's sidecar, so it must match the
    /// consumer: NMBL checks a slot's config, rescue image, network stage and
    /// tools image under their own domains; nothing at boot re-checks the rest.
    pub fn domain(self) -> &'static str {
        match self {
            Role::Config => "boot-config",
            Role::Rescue => "rescue-sfs",
            Role::Network => "network-stage",
            Role::Tools => "rescue-tools",
            Role::Bootloader | Role::Kernel | Role::Initrd => "boot-set-artifact",
        }
    }
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|e| Error::Invalid(format!("invalid manifest JSON: {e}")))?;
        value.validate_shape()?;
        Ok(value)
    }

    fn validate_shape(&self) -> Result<()> {
        if self.version != 1 {
            return Err(Error::Invalid("manifest version must be 1".into()));
        }
        if self.set_id.len() != 128 || !self.set_id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Invalid("set_id must be a SHA-512 hex digest".into()));
        }
        if self.target_slot != "A" && self.target_slot != "B" {
            return Err(Error::Invalid("target_slot must be A or B".into()));
        }
        let mut roles = BTreeSet::new();
        let mut destinations = BTreeSet::new();
        let mut sources = BTreeSet::new();
        for entry in &self.files {
            validate_relative(&entry.destination, false)?;
            validate_relative(&entry.payload, false)?;
            validate_relative(&entry.signature, false)?;
            if entry.payload != entry.destination
                || entry.signature != format!("{}.sig", entry.destination)
            {
                return Err(Error::Invalid(
                    "payload/signature names must match the slot destination".into(),
                ));
            }
            let expected = match entry.role {
                Role::Bootloader => "bootloader",
                Role::Kernel => "kernel",
                Role::Initrd => "initrd",
                Role::Rescue => "rescue",
                Role::Network => "network",
                Role::Tools => "tools",
                Role::Config => "config",
            };
            if entry.destination != expected {
                return Err(Error::Invalid(format!(
                    "role {:?} must use destination {expected}",
                    entry.role
                )));
            }
            if entry.domain != entry.role.domain() {
                return Err(Error::Invalid(format!(
                    "role {:?} must be signed under domain {}",
                    entry.role,
                    entry.role.domain()
                )));
            }
            if entry.sha512.len() != 128 || !entry.sha512.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Error::Invalid("artifact SHA-512 is malformed".into()));
            }
            if !roles.insert(entry.role) {
                return Err(Error::Invalid(format!("duplicate role: {:?}", entry.role)));
            }
            if !destinations.insert(&entry.destination) {
                return Err(Error::Invalid("duplicate destination".into()));
            }
            if !sources.insert(&entry.payload) || !sources.insert(&entry.signature) {
                return Err(Error::Invalid("duplicate bundle filename".into()));
            }
        }
        for required in [
            Role::Bootloader,
            Role::Kernel,
            Role::Initrd,
            Role::Rescue,
            Role::Config,
        ] {
            if !roles.contains(&required) {
                return Err(Error::Invalid(format!(
                    "missing required role: {required:?}"
                )));
            }
        }
        Ok(())
    }

    pub fn slot(&self) -> char {
        if self.target_slot == "A" { 'A' } else { 'B' }
    }
}

fn validate_relative(path: &str, allow_subdirs: bool) -> Result<()> {
    let parsed = std::path::Path::new(path);
    if path.is_empty() || parsed.is_absolute() {
        return Err(Error::Invalid(format!("unsafe relative path: {path}")));
    }
    let count = parsed.components().count();
    if parsed
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
        || (!allow_subdirs && count != 1)
    {
        return Err(Error::Invalid(format!("unsafe relative path: {path}")));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod tests {
    use super::*;

    fn manifest(config_domain: &str) -> String {
        let digest = "0".repeat(128);
        let files = ["bootloader", "kernel", "initrd", "rescue", "config"]
            .into_iter()
            .map(|name| {
                let domain = match name {
                    "config" => config_domain,
                    "rescue" => "rescue-sfs",
                    _ => "boot-set-artifact",
                };
                format!(
                    r#"{{"role":"{name}","destination":"{name}","payload":"{name}","signature":"{name}.sig","domain":"{domain}","sha512":"{digest}"}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(r#"{{"version":1,"set_id":"{digest}","target_slot":"A","files":[{files}]}}"#)
    }

    #[test]
    fn each_role_must_carry_its_verifier_domain() {
        Manifest::parse(manifest("boot-config").as_bytes()).expect("matching domains");
        let error = Manifest::parse(manifest("boot-set-artifact").as_bytes())
            .expect_err("config under the artifact domain");
        assert!(error.to_string().contains("boot-config"), "{error}");
    }
}
