//! Strict validation for the signed, data-only rescue network profile.

use std::collections::HashSet;
use std::net::IpAddr;

const MAX_CONFIG_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FamilyPolicy {
    Dual,
    V4,
    V6,
}

#[derive(Default)]
struct Profile {
    addresses: usize,
    gateway4: bool,
    gateway6: bool,
}

pub(super) fn validate_file(path: &std::path::Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("metadata: {error}"))?;
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err("network profile exceeds 64 KiB".into());
    }
    let text = std::fs::read_to_string(path).map_err(|error| format!("read: {error}"))?;
    validate(&text)
}

fn validate(text: &str) -> Result<(), String> {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let version = fields(lines.next().ok_or("network profile is empty")?);
    match version.as_slice() {
        ["version", "1"] => validate_dhcp(lines),
        ["version", "2"] => validate_static(lines),
        _ => Err("first directive must be version 1 or version 2".into()),
    }
}

fn validate_dhcp<'a>(lines: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut family = false;
    for line in lines {
        let item = fields(line);
        match item.as_slice() {
            ["address-family", value] if !family => {
                parse_family(value)?;
                family = true;
            }
            ["interface", name] if valid_interface(name) => {}
            _ => return Err(format!("invalid DHCP directive {line:?}")),
        }
    }
    family
        .then_some(())
        .ok_or("DHCP profile has no address-family".into())
}

fn validate_static<'a>(lines: impl Iterator<Item = &'a str>) -> Result<(), String> {
    let mut policy = None;
    let mut current: Option<Profile> = None;
    let mut selectors = HashSet::new();
    let mut profiles = 0usize;
    for line in lines {
        let item = fields(line);
        match item.as_slice() {
            ["address-family", value] if policy.is_none() && current.is_none() => {
                policy = Some(parse_family(value)?);
            }
            ["dns", address] if current.is_none() => {
                parse_ip(address, None)?;
            }
            ["profile", "interface", name] if current.is_none() && valid_interface(name) => {
                unique_selector(&mut selectors, &format!("interface:{name}"))?;
                current = Some(Profile::default());
            }
            ["profile", "mac", mac] if current.is_none() && valid_mac(mac) => {
                unique_selector(&mut selectors, &format!("mac:{}", mac.to_ascii_lowercase()))?;
                current = Some(Profile::default());
            }
            ["address", family, address] => {
                let family = parse_numbered_family(family, policy)?;
                parse_cidr(address, family)?;
                profile_mut(&mut current)?.addresses += 1;
            }
            ["gateway", family, address, mode] => {
                let family = parse_numbered_family(family, policy)?;
                parse_ip(address, Some(family))?;
                parse_mode(mode)?;
                let profile = profile_mut(&mut current)?;
                let seen = if family == 4 {
                    &mut profile.gateway4
                } else {
                    &mut profile.gateway6
                };
                if std::mem::replace(seen, true) {
                    return Err(format!("duplicate IPv{family} gateway"));
                }
            }
            ["route", family, destination, via, mode] => {
                let family = parse_numbered_family(family, policy)?;
                if *destination != "default" {
                    parse_cidr(destination, family)?;
                }
                if *via != "-" {
                    parse_ip(via, Some(family))?;
                }
                parse_mode(mode)?;
                profile_mut(&mut current)?;
            }
            ["end"] => {
                let profile = current.take().ok_or("end outside a profile")?;
                if profile.addresses == 0 {
                    return Err("static profile has no address".into());
                }
                profiles += 1;
            }
            _ => return Err(format!("invalid static directive {line:?}")),
        }
    }
    if current.is_some() {
        return Err("unterminated static profile".into());
    }
    policy.ok_or("static profile has no address-family")?;
    (profiles > 0)
        .then_some(())
        .ok_or("static config has no profiles".into())
}

fn fields(line: &str) -> Vec<&str> {
    line.split_ascii_whitespace().collect()
}

fn parse_family(value: &str) -> Result<FamilyPolicy, String> {
    match value {
        "dual-stack" => Ok(FamilyPolicy::Dual),
        "ipv4-only" => Ok(FamilyPolicy::V4),
        "ipv6-only" => Ok(FamilyPolicy::V6),
        _ => Err(format!("invalid address family {value:?}")),
    }
}

fn parse_numbered_family(value: &str, policy: Option<FamilyPolicy>) -> Result<u8, String> {
    let family = match value {
        "4" => 4,
        "6" => 6,
        _ => return Err("family must be 4 or 6".into()),
    };
    match (
        policy.ok_or("address-family must precede profiles")?,
        family,
    ) {
        (FamilyPolicy::V4, 6) | (FamilyPolicy::V6, 4) => {
            Err("directive contradicts address-family".into())
        }
        _ => Ok(family),
    }
}

fn parse_ip(value: &str, family: Option<u8>) -> Result<IpAddr, String> {
    let address: IpAddr = value
        .parse()
        .map_err(|_| format!("invalid IP address {value:?}"))?;
    if matches!(
        (family, address),
        (Some(4), IpAddr::V6(_)) | (Some(6), IpAddr::V4(_))
    ) {
        return Err(format!("IP address {value:?} has the wrong family"));
    }
    Ok(address)
}

fn parse_cidr(value: &str, family: u8) -> Result<(), String> {
    let (address, prefix) = value
        .split_once('/')
        .ok_or_else(|| format!("CIDR {value:?} has no prefix"))?;
    parse_ip(address, Some(family))?;
    let prefix: u8 = prefix
        .parse()
        .map_err(|_| format!("invalid CIDR prefix {value:?}"))?;
    let maximum = if family == 4 { 32 } else { 128 };
    if prefix > maximum {
        return Err(format!("CIDR prefix is too large in {value:?}"));
    }
    Ok(())
}

fn parse_mode(value: &str) -> Result<(), String> {
    match value {
        "normal" | "onlink" => Ok(()),
        _ => Err(format!("invalid route mode {value:?}")),
    }
}

fn profile_mut(profile: &mut Option<Profile>) -> Result<&mut Profile, String> {
    profile
        .as_mut()
        .ok_or("network directive outside a profile".into())
}

fn unique_selector(selectors: &mut HashSet<String>, selector: &str) -> Result<(), String> {
    selectors
        .insert(selector.into())
        .then_some(())
        .ok_or_else(|| format!("duplicate selector {selector:?}"))
}

fn valid_interface(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 15
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}

fn valid_mac(value: &str) -> bool {
    value.split(':').count() == 6
        && value
            .split(':')
            .all(|part| part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_dhcp_and_dual_stack_static_profiles() {
        assert!(validate("version 1\naddress-family dual-stack\ninterface eth0\n").is_ok());
        assert!(validate("version 2\naddress-family dual-stack\ndns 1.1.1.1\nprofile mac 52:54:00:12:34:56\naddress 4 10.0.2.15/32\ngateway 4 10.0.2.2 onlink\naddress 6 fec0::15/64\nroute 6 default fe80::2 onlink\nend\n").is_ok());
    }

    #[test]
    fn rejects_malformed_or_ambiguous_profiles() {
        for text in [
            "version 2\naddress-family dual-stack\nprofile interface eth0\nend\n",
            "version 2\naddress-family ipv4-only\nprofile mac nope\naddress 4 10.0.0.1/24\nend\n",
            "version 2\naddress-family ipv4-only\nprofile interface eth0\naddress 6 ::1/128\nend\n",
            "version 2\naddress-family ipv4-only\nprofile interface eth0\naddress 4 999.0.0.1/24\nend\n",
            "version 2\naddress-family ipv4-only\nprofile interface eth0\naddress 4 10.0.0.1/33\nend\n",
            "version 2\naddress-family ipv4-only\nunknown value\n",
        ] {
            assert!(validate(text).is_err(), "accepted {text:?}");
        }
    }
}
