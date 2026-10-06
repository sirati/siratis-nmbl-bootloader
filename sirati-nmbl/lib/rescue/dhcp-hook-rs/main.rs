//! dhcpcd's script in the rescue system.
//!
//! dhcpcd hands every DHCP, DHCPv6 and router-advertisement option it
//! received to its script as environment variables. The stock script is
//! bash running as root on that network input. This one does the only part
//! the rescue system needs, what dhcpcd's 20-resolv.conf hook does: keep a
//! resolver list per interface and protocol, and write /etc/resolv.conf from
//! their union. Every value is parsed as an address or a domain name and
//! anything else is dropped; nothing is ever evaluated.
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const STATE: &str = "/run/nmbl-rescue-dns";
const RESOLV_CONF: &str = "/etc/resolv.conf";
const MAX_ND_OPTIONS: u32 = 64;

#[derive(Debug, Default, PartialEq)]
struct Resolver {
    domain: Option<String>,
    search: Vec<String>,
    servers: Vec<String>,
}

#[derive(Debug, PartialEq)]
enum Action {
    Add(String, Resolver),
    Remove(String),
    Nothing,
}

fn valid_interface(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 16
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-@".contains(&b))
        && name != "."
        && name != ".."
}

fn valid_domain(name: &str) -> bool {
    let name = name.strip_suffix('.').unwrap_or(name);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// A nameserver as resolv.conf takes it, or None. IPv6 link-local
/// servers keep a scope, which must name an interface.
fn server(value: &str) -> Option<String> {
    let (address, scope) = match value.split_once('%') {
        Some((address, scope)) => (address, Some(scope)),
        None => (value, None),
    };
    let address: IpAddr = address.parse().ok()?;
    match scope {
        None => Some(address.to_string()),
        Some(scope) if address.is_ipv6() && valid_interface(scope) => Some(format!("{address}%{scope}")),
        Some(_) => None,
    }
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.contains(&value) {
        list.push(value);
    }
}

/// What dhcpcd's environment asks for, following dhcpcd-run-hooks and
/// 20-resolv.conf.
fn action(env: &BTreeMap<String, String>) -> Result<Action, String> {
    let get = |key: &str| env.get(key).map(String::as_str).unwrap_or("");
    if get("if_configured") != "true" {
        return Ok(Action::Nothing);
    }
    let interface = get("interface");
    if !valid_interface(interface) {
        return Err(format!("invalid interface {interface:?}"));
    }
    let protocol = get("protocol");
    if !protocol.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!("invalid protocol {protocol:?}"));
    }
    let key = if protocol.is_empty() { interface.to_owned() } else { format!("{interface}.{protocol}") };
    let reason = get("reason");
    if get("if_up") != "true" && reason != "ROUTERADVERT" {
        return Ok(if get("if_down") == "true" { Action::Remove(key) } else { Action::Nothing });
    }

    let v6 = matches!(reason, "BOUND6" | "RENEW6" | "REBIND6" | "REBOOT6" | "INFORM6");
    let (servers, search) = if v6 {
        ("new_dhcp6_name_servers", "new_dhcp6_domain_search")
    } else {
        ("new_domain_name_servers", "new_domain_search")
    };
    let mut raw_servers: Vec<&str> = get(servers).split_whitespace().collect();
    let mut raw_search: Vec<&str> = get(search).split_whitespace().collect();

    // Router advertisements: RDNSS and DNSSL options that are still alive.
    for i in 1..=MAX_ND_OPTIONS {
        let (Ok(acquired), Ok(now)) = (
            get(&format!("nd{i}_acquired")).parse::<i64>(),
            get(&format!("nd{i}_now")).parse::<i64>(),
        ) else {
            break;
        };
        let offset = now - acquired;
        for j in 1..=MAX_ND_OPTIONS {
            let rdnss = env.get(&format!("nd{i}_rdnss{j}_lifetime"));
            let dnssl = env.get(&format!("nd{i}_dnssl{j}_lifetime"));
            if rdnss.is_none() && dnssl.is_none() {
                break;
            }
            if rdnss.and_then(|t| t.parse::<i64>().ok()).is_some_and(|t| t - offset > 0) {
                raw_servers.extend(get(&format!("nd{i}_rdnss{j}_servers")).split_whitespace());
            }
            if dnssl.and_then(|t| t.parse::<i64>().ok()).is_some_and(|t| t - offset > 0) {
                raw_search.extend(get(&format!("nd{i}_dnssl{j}_search")).split_whitespace());
            }
        }
    }

    let mut domain = get("new_domain_name").split_whitespace().next().map(str::to_owned);
    if domain.is_none() {
        domain = ["new_dhcp6_fqdn", "new_fqdn", "new_host_name"]
            .iter()
            .find_map(|key| get(key).split_once('.').map(|(_, rest)| rest.to_owned()));
    }
    let domain = domain.filter(|name| valid_domain(name));

    let mut resolver = Resolver { domain, ..Resolver::default() };
    for name in raw_search {
        if valid_domain(name) {
            push_unique(&mut resolver.search, name.to_owned());
        }
    }
    if resolver.search.is_empty() {
        if let Some(domain) = &resolver.domain {
            resolver.search.push(domain.clone());
        }
    }
    for value in raw_servers {
        if let Some(value) = server(value) {
            push_unique(&mut resolver.servers, value);
        }
    }
    if resolver.servers.is_empty() && resolver.domain.is_none() && resolver.search.is_empty() {
        return Ok(Action::Remove(key));
    }
    Ok(Action::Add(key, resolver))
}

fn render_entry(resolver: &Resolver) -> String {
    let mut out = String::new();
    if let Some(domain) = &resolver.domain {
        out += &format!("domain {domain}\n");
    }
    if !resolver.search.is_empty() {
        out += &format!("search {}\n", resolver.search.join(" "));
    }
    for server in &resolver.servers {
        out += &format!("nameserver {server}\n");
    }
    out
}

/// resolv.conf from the per-interface entries, in name order: the first
/// domain, every search name and every server, each once.
fn render(entries: &[(String, String)]) -> String {
    let mut domain = None;
    let mut search = Vec::new();
    let mut servers = Vec::new();
    for (_, text) in entries {
        for line in text.lines() {
            match line.split_once(' ') {
                Some(("domain", value)) if domain.is_none() => domain = Some(value.to_owned()),
                Some(("search", values)) => values.split(' ').for_each(|v| push_unique(&mut search, v.to_owned())),
                Some(("nameserver", value)) => push_unique(&mut servers, value.to_owned()),
                _ => {}
            }
        }
    }
    if domain.as_ref().is_some_and(|d| search.len() == 1 && &search[0] == d) {
        search.clear();
    }
    let names: Vec<_> = entries.iter().map(|(name, _)| name.as_str()).collect();
    let mut out = format!("# Generated by the NMBL rescue DHCP hook from {}\n", names.join(", "));
    out += &render_entry(&Resolver { domain, search, servers });
    out
}

fn write_atomic(path: &Path, contents: &str, mode: u32) -> io::Result<()> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".nmbl-tmp");
    let temporary = PathBuf::from(temporary);
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&temporary)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
    fs::rename(&temporary, path)
}

fn apply(action: Action, state: &Path, resolv_conf: &Path) -> io::Result<()> {
    match &action {
        Action::Nothing => return Ok(()),
        Action::Add(key, resolver) => {
            fs::create_dir_all(state)?;
            fs::set_permissions(state, fs::Permissions::from_mode(0o700))?;
            write_atomic(&state.join(key), &render_entry(resolver), 0o600)?;
        }
        Action::Remove(key) => match fs::remove_file(state.join(key)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        },
    }
    let mut entries = Vec::new();
    if let Ok(dir) = fs::read_dir(state) {
        for entry in dir {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_file() && !name.ends_with(".nmbl-tmp") {
                entries.push((name, fs::read_to_string(entry.path())?));
            }
        }
    }
    entries.sort();
    write_atomic(resolv_conf, &render(&entries), 0o644)
}

fn main() {
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let result = action(&env)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
        .and_then(|action| apply(action, Path::new(STATE), &PathBuf::from(RESOLV_CONF)));
    if let Err(error) = result {
        eprintln!("nmbl-rescue-dhcp-hook: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut map: BTreeMap<String, String> =
            [("if_configured", "true"), ("interface", "eth0")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        map.extend(pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        map
    }

    #[test]
    fn dhcp4_servers_and_domain_are_validated() {
        let got = action(&env(&[
            ("protocol", "dhcp"), ("reason", "BOUND"), ("if_up", "true"),
            ("new_domain_name_servers", "198.19.0.1 $(reboot) 198.19.0.1 ::1 fe80::1%eth0 fe80::1%a;b"),
            ("new_domain_name", "example.test"),
            ("new_domain_search", "a.example.test bad_name `x`"),
        ])).unwrap();
        assert_eq!(got, Action::Add("eth0.dhcp".into(), Resolver {
            domain: Some("example.test".into()),
            search: vec!["a.example.test".into()],
            servers: vec!["198.19.0.1".into(), "::1".into(), "fe80::1%eth0".into()],
        }));
    }

    #[test]
    fn dhcp6_and_router_advertisements_use_their_own_options() {
        let got = action(&env(&[
            ("protocol", "dhcp6"), ("reason", "BOUND6"), ("if_up", "true"),
            ("new_domain_name_servers", "192.0.2.9"),
            ("new_dhcp6_name_servers", "2001:db8::53"),
        ])).unwrap();
        assert_eq!(got, Action::Add("eth0.dhcp6".into(), Resolver { servers: vec!["2001:db8::53".into()], ..Resolver::default() }));

        let got = action(&env(&[
            ("protocol", "ra"), ("reason", "ROUTERADVERT"), ("if_up", "false"),
            ("nd1_acquired", "100"), ("nd1_now", "110"),
            ("nd1_rdnss1_lifetime", "20"), ("nd1_rdnss1_servers", "2001:db8::1"),
            ("nd1_rdnss2_lifetime", "5"), ("nd1_rdnss2_servers", "2001:db8::2"),
            ("nd1_dnssl1_lifetime", "20"), ("nd1_dnssl1_search", "lan.test"),
        ])).unwrap();
        assert_eq!(got, Action::Add("eth0.ra".into(), Resolver {
            domain: None,
            search: vec!["lan.test".into()],
            servers: vec!["2001:db8::1".into()],
        }));
    }

    #[test]
    fn down_empty_unconfigured_and_hostile_inputs() {
        assert_eq!(action(&env(&[("protocol", "dhcp"), ("reason", "EXPIRE"), ("if_down", "true")])).unwrap(), Action::Remove("eth0.dhcp".into()));
        assert_eq!(action(&env(&[("protocol", "dhcp"), ("reason", "BOUND"), ("if_up", "true")])).unwrap(), Action::Remove("eth0.dhcp".into()));
        assert_eq!(action(&env(&[("reason", "CARRIER")])).unwrap(), Action::Nothing);
        let mut unconfigured = env(&[("reason", "BOUND"), ("if_up", "true")]);
        unconfigured.insert("if_configured".into(), "false".into());
        assert_eq!(action(&unconfigured).unwrap(), Action::Nothing);
        let mut traversal = env(&[("reason", "BOUND"), ("if_up", "true")]);
        traversal.insert("interface".into(), "../etc".into());
        assert!(action(&traversal).is_err());
        assert!(action(&env(&[("protocol", "x/../y"), ("reason", "BOUND"), ("if_up", "true")])).is_err());
    }

    #[test]
    fn resolv_conf_is_the_union_of_entries_and_follows_removal() {
        let dir = std::env::temp_dir().join(format!("nmbl-dhcp-hook-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (state, conf) = (dir.join("state"), dir.join("resolv.conf"));
        let add = |key: &str, servers: &[&str]| Action::Add(key.into(), Resolver {
            servers: servers.iter().map(|s| s.to_string()).collect(), ..Resolver::default()
        });
        apply(add("eth0.dhcp", &["198.19.0.1"]), &state, &conf).unwrap();
        apply(add("eth0.ra", &["2001:db8::1", "198.19.0.1"]), &state, &conf).unwrap();
        let text = fs::read_to_string(&conf).unwrap();
        assert_eq!(text.lines().filter(|l| l.starts_with("nameserver")).collect::<Vec<_>>(),
            ["nameserver 198.19.0.1", "nameserver 2001:db8::1"]);
        assert_eq!(fs::metadata(&conf).unwrap().permissions().mode() & 0o777, 0o644);
        apply(Action::Remove("eth0.dhcp".into()), &state, &conf).unwrap();
        apply(Action::Remove("eth0.ra".into()), &state, &conf).unwrap();
        assert!(!fs::read_to_string(&conf).unwrap().contains("nameserver"));
        fs::remove_dir_all(&dir).unwrap();
    }
}
