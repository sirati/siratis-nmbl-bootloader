//! `nmbl-erofs-receive`: install and activate a signed generation bundle
//! streamed on stdin. It runs as root behind a restricted SSH key and a sudo
//! rule, so it is a compiled program: the stream is parsed here, every
//! payload is hashed and verified in-process against the fixed public key,
//! and the generation is installed and activated with the same layout and
//! state transitions as `nmbl-erofsctl install` and `activate`.
//!
//! Stream (`NMBL-EROFS-BUNDLE-4`): one line each for magic, generation id,
//! image, signature and system sizes, config id, config and config-signature
//! sizes, kernel-signature, initrd-signature, rescue and network pairs, the
//! rescue-tools pair and the reboot flag, then the payloads in that order and
//! nothing after them.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha512};

use crate::domain::domain_for;
use crate::verify::verify_reader;

const MAGIC: &str = "NMBL-EROFS-BUNDLE-4";
const PREVIOUS_MAGIC: &str = "NMBL-EROFS-BUNDLE-3";
const MAX_PAYLOAD: u64 = 64 << 30;
const MAX_HEADER_LINE: usize = 256;
const SYSTEMCTL: &str = match option_env!("NMBL_SYSTEMCTL") {
    Some(path) => path,
    None => "/run/current-system/sw/bin/systemctl",
};
/// Members carried over from an upload besides the image and its signature.
const EXTRA_MEMBERS: [&str; 8] = [
    "kernel.sig",
    "initrd.sig",
    "rescue.sfs",
    "rescue.sfs.sig",
    "network.erofs",
    "network.erofs.sig",
    "rescue-tools.erofs",
    "rescue-tools.erofs.sig",
];
const REQUIRED_MEMBERS: [&str; 4] = ["kernel.sig", "initrd.sig", "rescue.sfs", "rescue.sfs.sig"];

type Result<T> = std::result::Result<T, String>;

fn io_error(context: impl std::fmt::Display) -> impl FnOnce(io::Error) -> String {
    move |error| format!("{context}: {error}")
}

/// The fixed-order header of a bundle.
#[derive(Debug, PartialEq, Eq)]
pub struct Header {
    pub id: String,
    pub image: u64,
    pub image_signature: u64,
    pub system: u64,
    pub config_id: String,
    pub config: u64,
    pub config_signature: u64,
    pub kernel: u64,
    pub kernel_signature: u64,
    pub initrd: u64,
    pub initrd_signature: u64,
    pub rescue: u64,
    pub rescue_signature: u64,
    pub network: u64,
    pub network_signature: u64,
    pub tools: u64,
    pub tools_signature: u64,
    pub reboot: bool,
}

fn read_line(input: &mut impl BufRead) -> Result<String> {
    let mut line = Vec::new();
    let read = input
        .by_ref()
        .take(MAX_HEADER_LINE as u64 + 1)
        .read_until(b'\n', &mut line)
        .map_err(io_error("read protocol header"))?;
    if read == 0 || line.last() != Some(&b'\n') {
        return Err(if line.len() > MAX_HEADER_LINE {
            "protocol header line is too long".into()
        } else {
            "truncated protocol header".into()
        });
    }
    line.pop();
    String::from_utf8(line).map_err(|_| "protocol header is not UTF-8".into())
}

fn valid_id(value: &str) -> bool {
    value.len() == 128 && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn size(value: &str) -> Result<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid payload size: {value}"));
    }
    let size: u64 = value.parse().map_err(|_| "payload is too large".to_string())?;
    if size > MAX_PAYLOAD {
        return Err("payload is too large".into());
    }
    Ok(size)
}

pub fn parse_header(input: &mut impl BufRead) -> Result<Header> {
    let mut first = Vec::with_capacity(16);
    for _ in 0..16 {
        first.push(read_line(input)?);
    }
    let field = |lines: &[String], index: usize| lines.get(index).cloned().unwrap_or_default();
    let magic = field(&first, 0);
    if magic == PREVIOUS_MAGIC {
        return Err(
            "sender speaks NMBL-EROFS-BUNDLE-3; NMBL-EROFS-BUNDLE-4 adds the rescue tools image".into(),
        );
    }
    if magic != MAGIC {
        return Err("invalid protocol magic".into());
    }
    let tools = read_line(input)?;
    let tools_signature = read_line(input)?;
    let reboot = read_line(input)?;
    let id = field(&first, 1);
    let config_id = field(&first, 5);
    if !valid_id(&id) {
        return Err("invalid generation id".into());
    }
    if !valid_id(&config_id) {
        return Err("invalid config id".into());
    }
    let sizes = [2, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]
        .iter()
        .map(|index| size(&field(&first, *index)))
        .collect::<Result<Vec<u64>>>()?;
    let at = |index: usize| sizes.get(index).copied().unwrap_or_default();
    let header = Header {
        id,
        image: at(0),
        image_signature: at(1),
        system: at(2),
        config_id,
        config: at(3),
        config_signature: at(4),
        kernel: at(5),
        kernel_signature: at(6),
        initrd: at(7),
        initrd_signature: at(8),
        rescue: at(9),
        rescue_signature: at(10),
        network: at(11),
        network_signature: at(12),
        tools: size(&tools)?,
        tools_signature: size(&tools_signature)?,
        reboot: match reboot.as_str() {
            "0" => false,
            "1" => true,
            _ => return Err("invalid reboot flag".into()),
        },
    };
    let required = [
        header.image,
        header.image_signature,
        header.config,
        header.config_signature,
        header.kernel,
        header.kernel_signature,
        header.initrd,
        header.initrd_signature,
        header.rescue,
        header.rescue_signature,
    ];
    if required.contains(&0) {
        return Err("empty payload or signature".into());
    }
    if (header.network == 0) != (header.network_signature == 0) {
        return Err("incomplete network payload".into());
    }
    if (header.tools == 0) != (header.tools_signature == 0) {
        return Err("incomplete rescue tools payload".into());
    }
    Ok(header)
}

/// A private directory that is removed, whatever happens, when dropped.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o600));
            }
        }
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn random_suffix() -> Result<String> {
    let mut bytes = [0u8; 6];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(io_error("read /dev/urandom"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create {}: {error}", path.display())),
    }
    let metadata = fs::symlink_metadata(path).map_err(io_error(path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(io_error(path.display()))
}

fn receive_file(input: &mut impl Read, size: u64, path: &Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error(path.display()))?;
    let copied = io::copy(&mut input.by_ref().take(size), &mut file).map_err(io_error(path.display()))?;
    if copied != size {
        return Err("truncated payload".into());
    }
    Ok(())
}

fn sha512_hex(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(io_error(path.display()))?;
    let mut hasher = Sha512::new();
    io::copy(&mut file, &mut hasher).map_err(io_error(path.display()))?;
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether two files have the same contents, compared in chunks like `cmp -s`.
fn same(left: &Path, right: &Path) -> bool {
    let (Ok(left), Ok(right)) = (File::open(left), File::open(right)) else {
        return false;
    };
    let (mut left, mut right) = (io::BufReader::new(left), io::BufReader::new(right));
    loop {
        let (Ok(a), Ok(b)) = (left.fill_buf(), right.fill_buf()) else {
            return false;
        };
        if a.is_empty() || b.is_empty() {
            return a.is_empty() && b.is_empty();
        }
        let n = a.len().min(b.len());
        if a.get(..n) != b.get(..n) {
            return false;
        }
        left.consume(n);
        right.consume(n);
    }
}

fn verify(public_key: &[u8], role: &str, input: &Path, signature: &Path) -> Result<()> {
    let domain = domain_for(role).ok_or_else(|| format!("unknown signing role {role}"))?;
    let sidecar = fs::read(signature).map_err(io_error(signature.display()))?;
    let mut reader = io::BufReader::new(File::open(input).map_err(io_error(input.display()))?);
    verify_reader(&mut reader, public_key, domain, &sidecar)
        .map_err(|error| format!("{}: {error}", input.display()))
}

fn pins_rescue_tools(config: &Path) -> Result<bool> {
    let text = fs::read(config).map_err(io_error(config.display()))?;
    Ok(text.split(|b| *b == b'\n').any(|line| line == b"[rescue.tools]"))
}

fn syncfs(path: &Path) -> Result<()> {
    let dir = File::open(path).map_err(io_error(path.display()))?;
    // SAFETY: syncfs only reads the descriptor, which `dir` keeps open.
    if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
        return Err(format!("sync {}: {}", path.display(), io::Error::last_os_error()));
    }
    Ok(())
}

fn install_copy(from: &Path, to: &Path) -> Result<()> {
    fs::copy(from, to).map_err(io_error(to.display()))?;
    fs::set_permissions(to, fs::Permissions::from_mode(0o444)).map_err(io_error(to.display()))?;
    File::open(to).and_then(|file| file.sync_all()).map_err(io_error(to.display()))
}

fn validate_generation(root: &Path, id: &str) -> Result<()> {
    if !valid_id(id) {
        return Err(format!("invalid generation id: {id}"));
    }
    let generation = root.join("generations").join(id);
    if !generation.join("nix.erofs").is_file() {
        return Err(format!("generation {id} has no image"));
    }
    if !generation.join("nix.erofs.sig").is_file() {
        return Err(format!("generation {id} has no signature"));
    }
    let actual = sha512_hex(&generation.join("nix.erofs"))?;
    if actual != id {
        return Err(format!("generation {id} content hash is {actual}"));
    }
    Ok(())
}

/// `nmbl-erofsctl install BUNDLE ROOT`: copy a verified bundle into
/// `generations/<id>` unless it is already there, then validate it.
fn install(bundle: &Path, root: &Path, id: &str) -> Result<()> {
    if sha512_hex(&bundle.join("nix.erofs"))? != id {
        return Err("bundle image hash does not match generation id".into());
    }
    let generations = root.join("generations");
    ensure_private_dir(root)?;
    ensure_private_dir(&generations)?;
    let target = generations.join(id);
    if !target.is_dir() {
        let incoming = generations.join(format!(".incoming-{id}.{}", std::process::id()));
        let _ = fs::remove_dir_all(&incoming);
        fs::DirBuilder::new().mode(0o700).create(&incoming).map_err(io_error(incoming.display()))?;
        let guard = Scratch(incoming.clone());
        for member in ["nix.erofs", "nix.erofs.sig", "system", "config.toml", "config.toml.sig"]
            .iter()
            .chain(EXTRA_MEMBERS.iter())
        {
            let source = bundle.join(member);
            if source.is_file() {
                install_copy(&source, &incoming.join(member))?;
            }
        }
        let generation_file = incoming.join("generation");
        fs::write(&generation_file, format!("{id}\n")).map_err(io_error(generation_file.display()))?;
        fs::set_permissions(&generation_file, fs::Permissions::from_mode(0o444))
            .map_err(io_error(generation_file.display()))?;
        File::open(&incoming).and_then(|dir| dir.sync_all()).map_err(io_error(incoming.display()))?;
        fs::rename(&incoming, &target).map_err(io_error(target.display()))?;
        std::mem::forget(guard);
        syncfs(&generations)?;
    }
    validate_generation(root, id)
}

/// The generation a state link (`active`, `previous`, ...) names, if valid.
fn target_id(root: &Path, link: &str) -> Option<String> {
    let path = root.join(link);
    if !fs::symlink_metadata(&path).ok()?.file_type().is_symlink() {
        return None;
    }
    let target = fs::read_link(&path).ok()?;
    let id = target.to_str()?.strip_prefix("generations/")?.to_owned();
    (valid_id(&id) && root.join("generations").join(&id).is_dir()).then_some(id)
}

fn replace_link(root: &Path, name: &str, id: &str) -> Result<()> {
    let temporary = root.join(format!(".{name}.new.{}", std::process::id()));
    let _ = fs::remove_file(&temporary);
    symlink(format!("generations/{id}"), &temporary).map_err(io_error(temporary.display()))?;
    fs::rename(&temporary, root.join(name)).map_err(io_error(root.join(name).display()))?;
    syncfs(root)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(format!("remove {}: {error}", path.display())),
        _ => Ok(()),
    }
}

/// `nmbl-erofsctl activate ID ROOT`: make a validated generation active,
/// keep the old one as `previous`, and mark the new one pending its first
/// successful boot unless it already is the tested generation.
fn activate(root: &Path, id: &str) -> Result<()> {
    let lock = File::open(root).map_err(io_error(root.display()))?;
    // SAFETY: flock only operates on the descriptor `lock` keeps open; the
    // lock is released when it is closed at the end of this function.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!("lock {}: {}", root.display(), io::Error::last_os_error()));
    }
    validate_generation(root, id)?;
    let old = target_id(root, "active");
    let tested = target_id(root, "tested");
    if let Some(old) = old.as_deref().filter(|old| *old != id) {
        replace_link(root, "previous", old)?;
    }
    replace_link(root, "active", id)?;
    if tested.as_deref() == Some(id) {
        remove_if_present(&root.join("pending"))?;
    } else {
        replace_link(root, "pending", id)?;
    }
    // Replacing an unhealthy generation must remain possible. Only a real
    // successful boot may set tested; supersede the old attempt so the new
    // generation receives its own first attempt on the next boot.
    remove_if_present(&root.join("attempted"))?;
    remove_if_present(&root.join("rollback-event"))?;
    syncfs(root)
}

fn absolute_outside_store(path: &Path) -> bool {
    path.is_absolute() && !path.as_os_str().as_encoded_bytes().starts_with(b"/nix/store")
}

/// Receive one bundle from `input` into `image_root`; returns the
/// generation id and whether the sender asked for a reboot.
pub fn receive(input: &mut impl BufRead, incoming: &Path, image_root: &Path, public_key: &Path) -> Result<(String, bool)> {
    if !absolute_outside_store(incoming) {
        return Err("invalid incoming root".into());
    }
    if !absolute_outside_store(image_root) {
        return Err("invalid image root".into());
    }
    if !public_key.is_file() {
        return Err("trusted public key is missing".into());
    }
    let header = parse_header(input)?;
    let id = header.id.clone();
    let public = fs::read(public_key).map_err(io_error(public_key.display()))?;

    ensure_private_dir(incoming)?;
    let tmp = incoming.join(format!(".receive-{id}.{}", random_suffix()?));
    fs::DirBuilder::new().mode(0o700).create(&tmp).map_err(io_error(tmp.display()))?;
    let scratch = Scratch(tmp);
    let tmp = scratch.0.as_path();

    let mut payloads = vec![
        (header.image, "nix.erofs"),
        (header.image_signature, "nix.erofs.sig"),
    ];
    if header.system > 0 {
        payloads.push((header.system, "system"));
    }
    payloads.extend([
        (header.config, "config.toml"),
        (header.config_signature, "config.toml.sig"),
        (header.kernel, "kernel"),
        (header.kernel_signature, "kernel.sig"),
        (header.initrd, "initrd"),
        (header.initrd_signature, "initrd.sig"),
        (header.rescue, "rescue.sfs"),
        (header.rescue_signature, "rescue.sfs.sig"),
    ]);
    if header.network > 0 {
        payloads.extend([(header.network, "network.erofs"), (header.network_signature, "network.erofs.sig")]);
    }
    if header.tools > 0 {
        payloads.extend([(header.tools, "rescue-tools.erofs"), (header.tools_signature, "rescue-tools.erofs.sig")]);
    }
    for (size, name) in &payloads {
        receive_file(input, *size, &tmp.join(name))?;
    }
    let mut extra = [0u8; 1];
    if input.read(&mut extra).map_err(io_error("read trailing data"))? != 0 {
        return Err("trailing protocol data".into());
    }
    fs::write(tmp.join("generation"), format!("{id}\n")).map_err(io_error("write generation"))?;

    if sha512_hex(&tmp.join("nix.erofs"))? != id {
        return Err("image hash does not match generation id".into());
    }
    if sha512_hex(&tmp.join("config.toml"))? != header.config_id {
        return Err("config hash does not match config id".into());
    }
    // A config that pins a rescue tools image must arrive with it, and only then.
    if pins_rescue_tools(&tmp.join("config.toml"))? {
        if header.tools == 0 {
            return Err("config pins a rescue tools image the bundle lacks".into());
        }
    } else if header.tools != 0 {
        return Err("bundle carries a rescue tools image its config does not pin".into());
    }
    let mut signed = vec![
        ("generation-image", "nix.erofs"),
        ("boot-config", "config.toml"),
        ("gen-kernel", "kernel"),
        ("gen-initrd", "initrd"),
        ("rescue-sfs", "rescue.sfs"),
    ];
    if header.network > 0 {
        signed.push(("network-stage", "network.erofs"));
    }
    if header.tools > 0 {
        signed.push(("rescue-tools", "rescue-tools.erofs"));
    }
    for (role, name) in &signed {
        verify(&public, role, &tmp.join(name), &tmp.join(format!("{name}.sig")))?;
    }
    for entry in fs::read_dir(tmp).map_err(io_error(tmp.display()))? {
        let entry = entry.map_err(io_error(tmp.display()))?;
        fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o444)).map_err(io_error(entry.path().display()))?;
    }

    let mut compared: Vec<&str> = REQUIRED_MEMBERS.to_vec();
    if header.network > 0 {
        compared.extend(["network.erofs", "network.erofs.sig"]);
    }
    if header.tools > 0 {
        compared.extend(["rescue-tools.erofs", "rescue-tools.erofs.sig"]);
    }
    let existing = image_root.join("generations").join(&id);
    if fs::symlink_metadata(&existing).is_ok() {
        if !existing.is_dir() {
            return Err("existing generation path is not a directory".into());
        }
        if !same(&tmp.join("nix.erofs"), &existing.join("nix.erofs")) {
            return Err("existing generation image differs".into());
        }
        verify(&public, "generation-image", &existing.join("nix.erofs"), &existing.join("nix.erofs.sig"))?;
        if !same(&tmp.join("config.toml"), &existing.join("config.toml")) {
            return Err("existing config differs".into());
        }
        verify(&public, "boot-config", &existing.join("config.toml"), &existing.join("config.toml.sig"))?;
        for member in &compared {
            if !same(&tmp.join(member), &existing.join(member)) {
                return Err(format!("existing {member} differs"));
            }
        }
    }

    install(tmp, image_root, &id)?;
    let installed = image_root.join("generations").join(&id);
    if !same(&tmp.join("config.toml"), &installed.join("config.toml")) {
        return Err("installed config differs".into());
    }
    verify(&public, "boot-config", &installed.join("config.toml"), &installed.join("config.toml.sig"))?;
    for member in &compared {
        if !same(&tmp.join(member), &installed.join(member)) {
            return Err(format!("installed {member} differs"));
        }
    }
    activate(image_root, &id)?;
    Ok((id, header.reboot))
}

/// The `nmbl-erofs-receive INCOMING_ROOT IMAGE_ROOT PUBLIC_KEY` entry point.
pub fn main_with(args: Vec<OsString>) -> Result<()> {
    let [incoming, image_root, public_key] = <[OsString; 3]>::try_from(args)
        .map_err(|_| "usage: nmbl-erofs-receive INCOMING_ROOT IMAGE_ROOT PUBLIC_KEY".to_string())?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let (id, reboot) = receive(
        &mut input,
        &PathBuf::from(incoming),
        &PathBuf::from(image_root),
        &PathBuf::from(public_key),
    )?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{id}").and_then(|()| stdout.flush()).map_err(io_error("write generation id"))?;
    if reboot {
        let status = std::process::Command::new(SYSTEMCTL)
            .arg("reboot")
            .env_clear()
            .status()
            .map_err(io_error(SYSTEMCTL))?;
        if !status.success() {
            return Err(format!("{SYSTEMCTL} reboot: {status}"));
        }
    }
    Ok(())
}

/// Arguments as raw OS strings, so a non-UTF-8 path is not a panic.
pub fn args() -> Vec<OsString> {
    std::env::args_os().skip(1).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header_text(magic: &str, lines: &[&str]) -> Vec<u8> {
        let mut text = format!("{magic}\n");
        for line in lines {
            text.push_str(line);
            text.push('\n');
        }
        text.into_bytes()
    }

    fn good_lines(id: &str) -> Vec<String> {
        let mut lines = vec![id.to_owned(), "5".into(), "4".into(), "0".into(), id.to_owned()];
        lines.extend(["3", "2", "1", "1", "1", "1", "1", "1", "0", "0", "0", "0", "1"].map(String::from));
        lines
    }

    fn set(lines: &mut [String], index: usize, value: &str) {
        if let Some(line) = lines.get_mut(index) {
            *line = value.to_owned();
        }
    }

    fn parse(magic: &str, lines: &[String]) -> Result<Header> {
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        parse_header(&mut io::Cursor::new(header_text(magic, &refs)))
    }

    #[test]
    fn a_complete_header_parses() -> Result<()> {
        let id = "a".repeat(128);
        let header = parse(MAGIC, &good_lines(&id))?;
        assert_eq!(header.id, id);
        assert_eq!((header.image, header.image_signature, header.config), (5, 4, 3));
        assert!(header.reboot);
        Ok(())
    }

    #[test]
    fn malformed_headers_are_refused() {
        let id = "a".repeat(128);
        let mut cases: Vec<(Vec<String>, &str)> = Vec::new();
        let mut bad_id = good_lines(&id);
        set(&mut bad_id, 0, &"A".repeat(128));
        cases.push((bad_id, "invalid generation id"));
        let mut bad_size = good_lines(&id);
        set(&mut bad_size, 1, "-1");
        cases.push((bad_size, "invalid payload size"));
        let mut huge = good_lines(&id);
        set(&mut huge, 1, &(MAX_PAYLOAD + 1).to_string());
        cases.push((huge, "too large"));
        let mut empty = good_lines(&id);
        set(&mut empty, 2, "0");
        cases.push((empty, "empty payload"));
        let mut network = good_lines(&id);
        set(&mut network, 13, "7");
        cases.push((network, "incomplete network"));
        let mut tools = good_lines(&id);
        set(&mut tools, 15, "7");
        cases.push((tools, "incomplete rescue tools"));
        let mut reboot = good_lines(&id);
        set(&mut reboot, 17, "yes");
        cases.push((reboot, "invalid reboot flag"));
        let mut short = good_lines(&id);
        short.truncate(10);
        cases.push((short, "truncated protocol header"));
        for (lines, expected) in cases {
            let error = parse(MAGIC, &lines).err().unwrap_or_default();
            assert!(error.contains(expected), "{expected}: {error}");
        }
        let previous = parse(PREVIOUS_MAGIC, &good_lines(&id)).err().unwrap_or_default();
        assert!(previous.contains("NMBL-EROFS-BUNDLE-3"), "{previous}");
        assert!(parse("NMBL-EROFS-BUNDLE-5", &good_lines(&id)).is_err());
    }

    #[test]
    fn header_lines_are_bounded() {
        let long = format!("{}\n", "9".repeat(MAX_HEADER_LINE + 10));
        let error = read_line(&mut io::Cursor::new(long.into_bytes())).err().unwrap_or_default();
        assert!(error.contains("too long"), "{error}");
        let error = read_line(&mut io::Cursor::new(b"no newline".to_vec())).err().unwrap_or_default();
        assert!(error.contains("truncated"), "{error}");
    }

    #[test]
    fn roots_must_be_absolute_and_outside_the_store() {
        assert!(absolute_outside_store(Path::new("/var/lib/nmbl-incoming")));
        assert!(!absolute_outside_store(Path::new("relative")));
        assert!(!absolute_outside_store(Path::new("/nix/store/x")));
        assert!(!absolute_outside_store(Path::new("/nix/storefoo")));
    }
}
