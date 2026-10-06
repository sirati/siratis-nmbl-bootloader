//! Host data handed to the host-independent stage-2 rescue image.
//!
//! The stage-2 EROFS image is the same for every host with the same NMBL
//! kernel and rescue package set: it contains no addresses, keys, ports or
//! module choices. Those live in NMBL's own config (`[rescue.system]`,
//! embedded or signed like the rest of it). Before the rescue child starts,
//! NMBL validates them and writes them as plain data files into the rescue
//! overlay at `/etc/nmbl-rescue/`, where the image's fixed `/init` reads
//! them. Nothing written here is ever evaluated as shell.
//!
//! Each file is installed only if its value validates. `/init` treats a
//! missing file fail-closed: no network profile or no port keeps the rescue
//! local-console only, no keys means no remote login.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};

use crate::config::{Config, RescueSystem};
use crate::error::{NmblError, Result};

/// Directory (relative to the rescue root) holding the handed-over files.
pub(crate) const HOST_DIR: &str = "etc/nmbl-rescue";

/// Write `[rescue.system]` into `<rescue_root>/etc/nmbl-rescue/`. A config
/// without that table (the flat rescue) installs nothing. Returns an error
/// naming every value that was rejected; the valid ones are still written.
pub(crate) fn install(config: &Config, rescue_root: &Path) -> Result<()> {
    let Some(system) = &config.rescue.system else {
        return Ok(());
    };
    let dir = rescue_root.join(HOST_DIR);
    std::fs::create_dir_all(&dir).map_err(|source| io(source, &dir))?;
    set_mode(&dir, 0o755)?;

    let mut rejected = Vec::new();
    for (name, value) in rendered(system, config.rescue.network_stage.is_some()) {
        match value {
            Ok(Some((contents, mode))) => write_file(&dir.join(name), &contents, mode)?,
            Ok(None) => {}
            Err(reason) => rejected.push(format!("{name}: {reason}")),
        }
    }
    if rejected.is_empty() {
        Ok(())
    } else {
        Err(NmblError::Rescue {
            stage: "rescue-host-data",
            source: Box::new(NmblError::ConfigInvalid {
                reason: rejected.join("; "),
                context: "[rescue.system]".to_string(),
            }),
        })
    }
}

type Rendered = std::result::Result<Option<(String, u32)>, String>;

/// Every handed-over file with its validated contents and mode.
fn rendered(system: &RescueSystem, network_stage: bool) -> Vec<(&'static str, Rendered)> {
    vec![
        ("sshd-port", sshd_port(system.sshd_port)),
        ("authorized_keys", authorized_keys(&system.authorized_keys)),
        (
            "host-key-path",
            host_key_path(system.host_key_path.as_deref()),
        ),
        ("modules", modules(&system.modules)),
        (
            "network.conf",
            network_profile(system.network_profile.as_deref(), network_stage),
        ),
        // Marks that a signed networking stage is configured: /init then
        // takes its modules and profile from /nmbl-network only.
        (
            "network-stage",
            Ok(network_stage.then(|| (String::new(), 0o644))),
        ),
    ]
}

fn sshd_port(port: u16) -> Rendered {
    if port == 0 {
        return Err("port 0".into());
    }
    Ok(Some((format!("{port}\n"), 0o644)))
}

fn authorized_keys(keys: &[String]) -> Rendered {
    let mut out = String::new();
    for key in keys {
        let key = key.trim();
        if key.is_empty() || key.chars().any(char::is_control) {
            return Err("an authorized key is empty or spans lines".into());
        }
        out.push_str(key);
        out.push('\n');
    }
    Ok(Some((out, 0o600)))
}

fn host_key_path(path: Option<&Path>) -> Rendered {
    let Some(path) = path else {
        return Ok(None);
    };
    let text = path.to_str().ok_or("not UTF-8")?;
    let normal = path.is_absolute()
        && path
            .components()
            .skip(1)
            .all(|part| matches!(part, Component::Normal(_)))
        && !text.chars().any(|c| c.is_control() || c.is_whitespace());
    if !normal {
        return Err(format!("`{text}` is not a plain absolute path"));
    }
    Ok(Some((format!("{text}\n"), 0o644)))
}

fn modules(names: &[String]) -> Rendered {
    let mut out = String::new();
    for name in names {
        let plain = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !plain {
            return Err(format!("`{name}` is not a module name"));
        }
        out.push_str(name);
        out.push('\n');
    }
    Ok(Some((out, 0o644)))
}

fn network_profile(profile: Option<&str>, network_stage: bool) -> Rendered {
    match profile {
        // With a signed networking stage only its profile may be applied.
        Some(_) if network_stage => Err("a baked profile beside a networking stage".into()),
        Some(text) => {
            super::network_profile::validate_text(text)?;
            Ok(Some((text.to_string(), 0o644)))
        }
        None => Ok(None),
    }
}

fn write_file(path: &Path, contents: &str, mode: u32) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(|source| io(source, path))?;
    file.write_all(contents.as_bytes())
        .map_err(|source| io(source, path))?;
    set_mode(path, mode)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|source| io(source, path))
}

fn io(source: std::io::Error, path: &Path) -> NmblError {
    NmblError::Rescue {
        stage: "rescue-host-data",
        source: Box::new(NmblError::Io {
            source,
            context: format!("writing {}", path.display()),
        }),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use crate::config::RescueConfig;
    use std::path::PathBuf;

    fn config(system: RescueSystem, network_stage: bool) -> Config {
        let mut c = Config::recovery_default();
        c.rescue = RescueConfig {
            system: Some(system),
            network_stage: network_stage.then(|| crate::config::RescueNetworkStage {
                path: PathBuf::from("nmbl/network.erofs"),
                sha512: None,
            }),
            ..RescueConfig::default()
        };
        c
    }

    fn system() -> RescueSystem {
        RescueSystem {
            sshd_port: 22222,
            authorized_keys: vec!["ssh-ed25519 AAAA operator".into()],
            host_key_path: Some(PathBuf::from("/nmbl-identity/etc/ssh/ssh_host_ed25519_key")),
            modules: vec!["overlay".into(), "e1000e".into()],
            network_profile: Some("version 1\naddress-family dual-stack\n".into()),
        }
    }

    fn read(root: &Path, name: &str) -> Option<String> {
        std::fs::read_to_string(root.join(HOST_DIR).join(name)).ok()
    }

    #[test]
    fn installs_every_value_as_plain_data() {
        let root = tempfile::tempdir().expect("tempdir");
        install(&config(system(), false), root.path()).expect("install");
        assert_eq!(read(root.path(), "sshd-port").as_deref(), Some("22222\n"));
        assert_eq!(
            read(root.path(), "authorized_keys").as_deref(),
            Some("ssh-ed25519 AAAA operator\n")
        );
        assert_eq!(
            read(root.path(), "modules").as_deref(),
            Some("overlay\ne1000e\n")
        );
        assert!(read(root.path(), "network.conf").is_some());
        assert!(read(root.path(), "network-stage").is_none());
        use std::os::unix::fs::PermissionsExt;
        let keys =
            std::fs::metadata(root.path().join(HOST_DIR).join("authorized_keys")).expect("keys");
        assert_eq!(keys.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn flat_rescue_installs_nothing() {
        let root = tempfile::tempdir().expect("tempdir");
        install(&Config::recovery_default(), root.path()).expect("nothing to do");
        assert!(!root.path().join(HOST_DIR).exists());
    }

    #[test]
    fn rejected_values_stay_absent_and_are_reported() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut bad = system();
        bad.authorized_keys = vec!["ssh-ed25519 AAAA a\nssh-ed25519 BBBB injected".into()];
        bad.modules = vec!["e1000e; reboot".into()];
        bad.host_key_path = Some(PathBuf::from("/a/../etc/shadow"));
        bad.network_profile = Some("version 2\naddress-family dual-stack\nprofile interface eth0\naddress 4 999.0.0.1/24\nend\n".into());
        let err = install(&config(bad, false), root.path()).expect_err("must reject");
        let text = err.to_string() + &format!("{err:?}");
        for name in [
            "authorized_keys",
            "modules",
            "host-key-path",
            "network.conf",
        ] {
            assert!(text.contains(name), "{name} not reported: {text}");
            assert!(read(root.path(), name).is_none(), "{name} was written");
        }
        // The valid port is still handed over.
        assert_eq!(read(root.path(), "sshd-port").as_deref(), Some("22222\n"));
    }

    #[test]
    fn networking_stage_excludes_a_baked_profile() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut only_stage = system();
        only_stage.network_profile = None;
        install(&config(only_stage, true), root.path()).expect("install");
        assert!(read(root.path(), "network-stage").is_some());
        assert!(read(root.path(), "network.conf").is_none());

        let root = tempfile::tempdir().expect("tempdir");
        assert!(install(&config(system(), true), root.path()).is_err());
        assert!(read(root.path(), "network.conf").is_none());
    }
}
