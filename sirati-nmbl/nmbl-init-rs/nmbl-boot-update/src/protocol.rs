use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::transaction::{self, Limits};
use crate::{Error, Result};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    bundle: String,
}

pub fn request(socket: &Path, bundle: &Path, public_key: &Path) -> Result<String> {
    // The unprivileged side checks first. The service repeats this from its own
    // independently opened descriptors after authenticating the connection.
    crate::validate::bundle(bundle, public_key)?;
    let mut stream =
        UnixStream::connect(socket).map_err(|e| Error::io("connect update service", e))?;
    let request = WireRequest {
        bundle: bundle.to_string_lossy().into_owned(),
    };
    serde_json::to_writer(&mut stream, &request)
        .map_err(|e| Error::Invalid(format!("encode request: {e}")))?;
    stream
        .write_all(b"\n")
        .map_err(|e| Error::io("send request", e))?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .map_err(|e| Error::io("read response", e))?;
    if let Some(message) = response.strip_prefix("OK ") {
        return Ok(message.trim().into());
    }
    Err(Error::Invalid(response.trim().to_owned()))
}

pub fn serve(
    socket: &Path,
    spool: &Path,
    boot_roots: &[PathBuf],
    public_key: &Path,
    allowed_uid: u32,
) -> Result<()> {
    let listener = listen(socket)?;
    for connection in listener.incoming() {
        let mut stream = connection.map_err(|e| Error::io("accept update client", e))?;
        let result = handle(&mut stream, spool, boot_roots, public_key, allowed_uid);
        let line = match result {
            Ok(message) => format!("OK {message}\n"),
            Err(error) => format!("ERROR {error}\n"),
        };
        stream
            .write_all(line.as_bytes())
            .map_err(|e| Error::io("write response", e))?;
    }
    Ok(())
}

/// Serve one authenticated request. Kept as a production entry point so the
/// protocol test exercises the exact peer checks and privileged handler.
pub fn serve_one(
    socket: &Path,
    spool: &Path,
    boot_roots: &[PathBuf],
    public_key: &Path,
    allowed_uid: u32,
) -> Result<()> {
    let listener = listen(socket)?;
    let (mut stream, _) = listener
        .accept()
        .map_err(|e| Error::io("accept update client", e))?;
    let result = handle(&mut stream, spool, boot_roots, public_key, allowed_uid);
    let line = match result {
        Ok(message) => format!("OK {message}\n"),
        Err(error) => format!("ERROR {error}\n"),
    };
    stream
        .write_all(line.as_bytes())
        .map_err(|e| Error::io("write response", e))
}

fn listen(socket: &Path) -> Result<UnixListener> {
    remove_old_socket(socket)?;
    let listener = UnixListener::bind(socket).map_err(|e| Error::io("bind update socket", e))?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o660))
        .map_err(|e| Error::io("set socket permissions", e))?;
    Ok(listener)
}

fn handle(
    stream: &mut UnixStream,
    spool: &Path,
    boot_roots: &[PathBuf],
    public_key: &Path,
    allowed_uid: u32,
) -> Result<String> {
    let peer_pid = authenticate_peer(stream, allowed_uid)?;
    let mut line = String::new();
    BufReader::new(&mut *stream)
        .take(64 * 1024)
        .read_line(&mut line)
        .map_err(|e| Error::io("read update request", e))?;
    let request: WireRequest = serde_json::from_str(line.trim_end())
        .map_err(|e| Error::Invalid(format!("invalid request: {e}")))?;
    // Recheck the live peer at the privilege boundary after the complete,
    // bounded request has arrived. The first check cannot authorize later
    // work on behalf of a replaced or vanished process.
    if authenticate_peer(stream, allowed_uid)? != peer_pid {
        return Err(Error::Invalid(
            "update peer changed while reading request".into(),
        ));
    }
    let bundle = confined_bundle(spool, Path::new(&request.bundle))?;
    if boot_roots.is_empty() {
        return Err(Error::Invalid("at least one boot root is required".into()));
    }
    let mut outcomes = Vec::with_capacity(boot_roots.len());
    for root in boot_roots {
        // Each mirror receives and verifies one complete set before the next
        // mirror is touched, so a failed later mirror leaves a bootable copy.
        outcomes.push(transaction::install(
            &bundle,
            public_key,
            root,
            &Limits::default(),
        )?);
    }
    Ok(format!("{outcomes:?}"))
}

fn authenticate_peer(stream: &UnixStream, allowed_uid: u32) -> Result<i32> {
    let credentials = rustix::net::sockopt::get_socket_peercred(stream)
        .map_err(|e| Error::io("read peer credentials", e.into()))?;
    if credentials.uid.as_raw() != allowed_uid {
        return Err(Error::Invalid("peer UID is not authorized".into()));
    }
    let pid = credentials.pid.as_raw_nonzero().get();
    require_same_executable(pid)?;
    Ok(pid)
}

fn require_same_executable(peer_pid: i32) -> Result<()> {
    let ours =
        fs::metadata("/proc/self/exe").map_err(|e| Error::io("stat service executable", e))?;
    let peer_path = PathBuf::from(format!("/proc/{peer_pid}/exe"));
    let peer = fs::metadata(peer_path).map_err(|e| Error::io("stat peer executable", e))?;
    if ours.dev() != peer.dev() || ours.ino() != peer.ino() {
        return Err(Error::Invalid("peer is not the same update binary".into()));
    }
    Ok(())
}

fn confined_bundle(spool: &Path, requested: &Path) -> Result<PathBuf> {
    let spool = spool
        .canonicalize()
        .map_err(|e| Error::io("resolve spool", e))?;
    let requested = requested
        .canonicalize()
        .map_err(|e| Error::io("resolve bundle", e))?;
    if requested.parent() != Some(spool.as_path()) {
        return Err(Error::Invalid(
            "bundle must be one direct child of the update spool".into(),
        ));
    }
    Ok(requested)
}

fn remove_old_socket(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            fs::remove_file(path).map_err(|e| Error::io("remove stale update socket", e))
        }
        Ok(_) => Err(Error::Invalid("update socket path is occupied".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io("inspect update socket", e)),
    }
}
