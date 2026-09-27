//! Orchestration: stage the container root, start the seccomp supervisor,
//! launch rootless podman with NMBL as PID 1 on a 640x360-sized pty (80x22
//! cells of an 8x16 font), relay the terminal, show the simulated framebuffer
//! in X11, and print the kexec handover when NMBL hands off.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use crate::Opts;
use crate::report;
use crate::scenario::Scenario;
use crate::seccomp;
use crate::sim::{self, Outcome, Sim};
use crate::tools::{DEVICES_JSON, Devices};

/// 640x360 px at an 8x16 cell = 80 columns x 22 rows.
const COLS: u16 = 80;
const ROWS: u16 = 22;

pub fn run(opts: &Opts) -> Result<i32, String> {
    let scenario = Scenario::load(&opts.scenario)?;
    let work = tempdir()?;
    let rootfs = work.join("rootfs");
    stage_rootfs(&scenario, &rootfs, opts)?;

    let sock_path = work.join("notify.sock");
    let listener = UnixListener::bind(&sock_path).map_err(|e| format!("bind {e}"))?;
    let profile = work.join("seccomp.json");
    write_profile(&profile, &sock_path, opts.graphical)?;

    let (pty_master, pty_slave_path) = open_pty()?;
    let mut child = spawn_podman(
        &scenario,
        &rootfs,
        &profile,
        &sock_path,
        &pty_slave_path,
        opts,
    )?;

    // Supervisor thread: owns the listener and the simulator.
    let (done_tx, done_rx) = mpsc::channel::<Result<(Outcome, Vec<String>), String>>();
    let release = scenario.kernel_release.clone();
    let (fb_w, fb_h) = scenario.framebuffer;
    let mut sim = Sim::new(scenario, release);
    let frames = Arc::new(AtomicU64::new(0));
    let viewer_stop = Arc::new(AtomicBool::new(false));
    let mut fb_memfd = None;
    // Keeps the simulated VT's pty master open for the whole run.
    let mut vt_master: Option<OwnedFd> = None;
    let mut viewer = None;
    if opts.graphical || opts.frame_dump.is_some() {
        let card = crate::drm::Card::new(fb_w, fb_h).map_err(|e| format!("drm memfd: {e}"))?;
        let memfd = card.memfd.try_clone().map_err(|e| e.to_string())?;
        fb_memfd = Some((memfd.try_clone().map_err(|e| e.to_string())?, fb_w, fb_h));
        // The simulated VT (/dev/tty1, the splash's keyboard) gets its own
        // pty: the X11 window types into its master, NMBL reads its slave, and
        // nothing else (podman's console relay) competes for the input.
        let (vt_m, vt_slave) = open_pty()?;
        vt_master = Some(vt_m);
        sim.display = Some(sim::Display::new(card, vt_slave, Arc::clone(&frames)));
        if opts.graphical && !opts.headless {
            let v = crate::x11view::Viewer {
                width: fb_w,
                height: fb_h,
                memfd,
                frames: Arc::clone(&frames),
                keys_out: std::fs::File::from(match &vt_master {
                    Some(m) => dup(m)?,
                    None => dup(&pty_master)?,
                }),
                stop: Arc::clone(&viewer_stop),
            };
            viewer = Some(std::thread::spawn(move || {
                if let Err(e) = v.run() {
                    eprintln!("nmbl-simbox: X11 viewer: {e}");
                }
            }));
        }
    }
    std::thread::spawn(move || {
        let r = supervise(&listener, sim);
        let _ = done_tx.send(r);
    });

    // Console relay: container pty <-> our stdout/stdin (plus scripted keys).
    let transcript = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let master_r = dup(&pty_master)?;
    {
        let transcript = Arc::clone(&transcript);
        let stop = Arc::clone(&stop);
        let headless = opts.headless;
        std::thread::spawn(move || relay_out(master_r, &transcript, &stop, headless));
    }
    let keys = Scenario::load(&opts.scenario)?.keys;
    let master_w = dup(&pty_master)?;
    {
        let transcript = Arc::clone(&transcript);
        std::thread::spawn(move || scripted_keys(master_w, keys, &transcript));
    }
    if !opts.headless {
        let master_in = dup(&pty_master)?;
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || relay_in(master_in, &stop));
    }

    let deadline = Instant::now() + Duration::from_secs(opts.timeout_secs);
    let outcome = loop {
        if let Ok(r) = done_rx.recv_timeout(Duration::from_millis(100)) {
            break r;
        }
        if let Ok(Some(status)) = child.try_wait() {
            // Give the supervisor a moment to deliver the final notification.
            if let Ok(r) = done_rx.recv_timeout(Duration::from_millis(500)) {
                break r;
            }
            break Err(format!(
                "container exited ({status}) without kexec or reboot"
            ));
        }
        if Instant::now() > deadline {
            break Err(format!("timed out after {}s", opts.timeout_secs));
        }
    };
    stop.store(true, Ordering::Relaxed);
    // The machine is gone: close the framebuffer window.
    viewer_stop.store(true, Ordering::Relaxed);
    if let Some(v) = viewer {
        let _ = v.join();
    }
    if let (Some(path), Some((fd, w, h))) = (&opts.frame_dump, &fb_memfd) {
        dump_ppm(path, fd, *w, *h)?;
    }
    let _ = child.kill();
    let _ = child.wait();
    drop(vt_master);

    let (outcome, trace) = match outcome {
        Ok(v) => v,
        Err(e) => {
            let t = transcript.lock().map(|t| t.clone()).unwrap_or_default();
            let tail = String::from_utf8_lossy(&t);
            let tail: String = tail
                .chars()
                .rev()
                .take(3000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(format!("{e}\n--- console tail ---\n{tail}"));
        }
    };
    if opts.trace {
        for l in &trace {
            eprintln!("[simbox] {l}");
        }
        if let Ok(t) = transcript.lock()
            && let Some(dir) = &opts.json
        {
            let _ = std::fs::write(dir.with_extension("console"), &*t);
        }
    }
    finish(outcome, opts)
}

fn finish(outcome: Outcome, opts: &Opts) -> Result<i32, String> {
    let color = !opts.headless;
    println!();
    match outcome {
        Outcome::Kexec(k) => {
            println!(
                "{}",
                if color {
                    "\x1b[1;32m*** NMBL executed kexec: the simulated machine hands off to the new kernel ***\x1b[0m"
                } else {
                    "*** NMBL executed kexec: the simulated machine hands off to the new kernel ***"
                }
            );
            if !opts.headless {
                println!("Press Enter to inspect what was passed to the kexec'd kernel.");
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
            }
            let r = report::render(&k, opts.reveal_keys, color);
            println!("{}", r.text);
            if let Some(path) = &opts.json {
                let json = serde_json::json!({
                    "outcome": "kexec",
                    "cmdline": r.facts.cmdline,
                    "init": r.facts.init,
                    "rollback_marker": r.facts.rollback_marker,
                    "kernel_len": r.facts.kernel_len,
                    "kernel_path": r.facts.kernel_path,
                    "initrd_len": r.facts.initrd_len,
                    "appended_files": r.facts.appended_files,
                    "keyfiles": r.facts.keyfiles,
                    "log_lines": r.facts.log_lines,
                });
                std::fs::write(path, json.to_string()).map_err(|e| e.to_string())?;
            }
            Ok(0)
        }
        other => {
            let name = match other {
                Outcome::Reboot => "reboot",
                Outcome::Halt => "halt",
                Outcome::PowerOff => "power-off",
                Outcome::Kexec(_) => "kexec",
            };
            println!("*** NMBL requested {name} ***");
            if let Some(path) = &opts.json {
                let json = serde_json::json!({ "outcome": name });
                std::fs::write(path, json.to_string()).map_err(|e| e.to_string())?;
            }
            Ok(0)
        }
    }
}

fn supervise(listener: &UnixListener, mut sim: Sim) -> Result<(Outcome, Vec<String>), String> {
    let l = seccomp::accept_listener(listener).map_err(|e| format!("seccomp listener: {e}"))?;
    while let Some(n) = l.recv() {
        let (reply, outcome) = sim.handle(&n);
        if l.still_valid(n.id) {
            l.reply(n.id, reply);
        }
        if let Some(o) = outcome {
            return Ok((o, std::mem::take(&mut sim.trace)));
        }
    }
    Err("container exited before NMBL kexec'd or rebooted".to_string())
}

/// Build the container root: the scenario initramfs, plus the simbox
/// stand-ins for blkid/cryptsetup, the device data they read, and the mount
/// points for the scenario trees.
fn stage_rootfs(s: &Scenario, rootfs: &Path, opts: &Opts) -> Result<(), String> {
    let src = s.path(&s.initramfs);
    copy_tree(&src, rootfs)?;
    let bin = rootfs.join("bin");
    std::fs::create_dir_all(&bin).map_err(|e| e.to_string())?;
    let me = std::env::current_exe().map_err(|e| e.to_string())?;
    for tool in ["blkid", "cryptsetup"] {
        let dst = bin.join(tool);
        let _ = std::fs::remove_file(&dst);
        std::fs::copy(&me, &dst).map_err(|e| format!("staging {tool}: {e}"))?;
    }
    if let Some(init) = &opts.init {
        let dst = rootfs.join("init");
        let _ = std::fs::remove_file(&dst);
        std::fs::copy(init, &dst).map_err(|e| format!("staging init: {e}"))?;
    }
    let mut devices = Devices::default();
    for b in &s.block {
        devices.blkid.insert(b.name.clone(), b.blkid.clone());
    }
    for l in &s.luks {
        let node = l
            .device
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        devices
            .luks
            .insert(node, (l.name.clone(), l.passphrase.clone()));
    }
    let simbox = rootfs.join(".simbox");
    std::fs::create_dir_all(&simbox).map_err(|e| e.to_string())?;
    std::fs::write(
        rootfs.join(DEVICES_JSON.trim_start_matches('/')),
        serde_json::to_string(&devices).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    for (_, inside) in sim::tree_mounts(s) {
        std::fs::create_dir_all(rootfs.join(inside.trim_start_matches('/')))
            .map_err(|e| e.to_string())?;
    }
    // Directories NMBL mounts onto (podman provides /proc,/sys,/dev,/tmp,/run).
    for d in ["mnt", "nmbl-log", "run", "tmp"] {
        std::fs::create_dir_all(rootfs.join(d)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The fake sysfs tree: `/sys/class/block/<dev>/dev`, the console, no TPM.
fn stage_sysfs(s: &Scenario, dir: &Path) -> Result<(), String> {
    let block = dir.join("class/block");
    std::fs::create_dir_all(&block).map_err(|e| e.to_string())?;
    for b in &s.block {
        let d = block.join(&b.name);
        std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        std::fs::write(d.join("dev"), format!("{}:{}\n", b.major, b.minor))
            .map_err(|e| e.to_string())?;
    }
    let tty = dir.join("class/tty/console");
    std::fs::create_dir_all(&tty).map_err(|e| e.to_string())?;
    std::fs::write(tty.join("active"), "ttyS0\n").map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir.join("class/misc")).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(dir.join("module")).map_err(|e| e.to_string())?;
    Ok(())
}

fn spawn_podman(
    s: &Scenario,
    rootfs: &Path,
    profile: &Path,
    sock: &Path,
    pty: &Path,
    opts: &Opts,
) -> Result<Child, String> {
    let work = rootfs.parent().ok_or("work dir")?;
    let sysfs = work.join("sys");
    stage_sysfs(s, &sysfs)?;
    let cmdline = work.join("cmdline");
    std::fs::write(&cmdline, format!("{}\n", s.cmdline)).map_err(|e| e.to_string())?;
    let modules = work.join("modules");
    std::fs::write(&modules, "").map_err(|e| e.to_string())?;
    let printk = work.join("printk");
    std::fs::write(&printk, "4\t4\t1\t7\n").map_err(|e| e.to_string())?;

    let mut cmd = Command::new("podman");
    cmd.args([
        "run",
        "--rm",
        "--cap-drop=ALL",
        "--network=none",
        "--security-opt",
        "no-new-privileges",
        "--security-opt",
    ])
    .arg(format!("seccomp={}", profile.display()))
    .arg("--annotation")
    .arg(format!("run.oci.seccomp.receiver={}", sock.display()))
    .args(["-i", "-t", "--env", "TERM=linux"])
    .arg("--mount")
    .arg(format!("type=bind,src={},dst=/sys,ro", sysfs.display()))
    .arg("--mount")
    .arg(format!(
        "type=bind,src={},dst=/proc/cmdline,ro",
        cmdline.display()
    ))
    .arg("--mount")
    .arg(format!(
        "type=bind,src={},dst=/proc/modules,ro",
        modules.display()
    ))
    .arg("--mount")
    .arg(format!(
        "type=bind,src={},dst=/proc/sys/kernel/printk",
        printk.display()
    ));
    // Each disk gets a private writable copy of its scenario tree, like a
    // real disk: NMBL creates mountpoints and writes state on it, and the
    // scenario sources (often in the read-only Nix store) stay untouched.
    for (i, (host, inside)) in sim::tree_mounts(s).into_iter().enumerate() {
        let copy = work.join(format!("disk{i}"));
        copy_tree(&host, &copy)?;
        cmd.arg("--mount")
            .arg(format!("type=bind,src={},dst={inside}", copy.display()));
    }
    cmd.arg("--rootfs").arg(rootfs).arg("/init");
    let _ = opts;
    // podman -t allocates the container pty; we give podman OUR pty slave as
    // its terminal so the container console is sized 80x22 and we own the
    // master side for relaying and scripted input.
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pty)
        .map_err(|e| format!("open pty slave: {e}"))?;
    let slave_out = slave.try_clone().map_err(|e| e.to_string())?;
    let slave_err = slave.try_clone().map_err(|e| e.to_string())?;
    // SAFETY: setsid in the child before exec so the pty becomes its
    // controlling terminal; only async-signal-safe calls.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.stdin(Stdio::from(slave))
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(slave_err))
        .spawn()
        .map_err(|e| format!("starting podman: {e} (is podman on PATH?)"))
}

fn write_profile(path: &Path, sock: &Path, graphical: bool) -> Result<(), String> {
    let mut names: Vec<&str> = sim::SYSCALLS.to_vec();
    if graphical {
        names.extend_from_slice(sim::GRAPHICAL_SYSCALLS);
    }
    let json = serde_json::json!({
        "defaultAction": "SCMP_ACT_ALLOW",
        "listenerPath": sock,
        "syscalls": [{ "names": names, "action": "SCMP_ACT_NOTIFY" }],
    });
    std::fs::write(path, json.to_string()).map_err(|e| e.to_string())
}

fn open_pty() -> Result<(OwnedFd, PathBuf), String> {
    // SAFETY: posix_openpt/grantpt/unlockpt/ptsname_r with checked results.
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if m < 0 {
            return Err("posix_openpt failed".to_string());
        }
        let master = OwnedFd::from_raw_fd(m);
        if libc::grantpt(m) != 0 || libc::unlockpt(m) != 0 {
            return Err("grantpt/unlockpt failed".to_string());
        }
        let mut buf = [0 as libc::c_char; 128];
        if libc::ptsname_r(m, buf.as_mut_ptr(), buf.len()) != 0 {
            return Err("ptsname failed".to_string());
        }
        let name = std::ffi::CStr::from_ptr(buf.as_ptr())
            .to_string_lossy()
            .into_owned();
        let ws = libc::winsize {
            ws_row: ROWS,
            ws_col: COLS,
            ws_xpixel: 640,
            ws_ypixel: 360,
        };
        libc::ioctl(m, libc::TIOCSWINSZ, &ws);
        Ok((master, PathBuf::from(name)))
    }
}

fn dup(fd: &OwnedFd) -> Result<OwnedFd, String> {
    fd.try_clone().map_err(|e| e.to_string())
}

fn relay_out(master: OwnedFd, transcript: &Mutex<Vec<u8>>, stop: &AtomicBool, headless: bool) {
    let mut f = std::fs::File::from(master);
    let mut buf = [0u8; 4096];
    let mut out = std::io::stdout();
    while !stop.load(Ordering::Relaxed) {
        match f.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = buf.get(..n).unwrap_or_default();
                if let Ok(mut t) = transcript.lock() {
                    t.extend_from_slice(chunk);
                }
                if !headless {
                    let _ = out.write_all(chunk);
                    let _ = out.flush();
                }
            }
        }
    }
}

fn relay_in(master: OwnedFd, stop: &AtomicBool) {
    let mut f = std::fs::File::from(master);
    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    // Raw mode on our terminal so single keys reach NMBL.
    // SAFETY: termios on our own stdin.
    let saved = unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) == 0 {
            let saved = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(fd, libc::TCSANOW, &t);
            Some(saved)
        } else {
            None
        }
    };
    let mut buf = [0u8; 256];
    let mut input = stdin.lock();
    while !stop.load(Ordering::Relaxed) {
        match input.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let _ = f.write_all(buf.get(..n).unwrap_or_default());
            }
        }
    }
    if let Some(t) = saved {
        // SAFETY: restore our own terminal.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
    }
}

fn scripted_keys(
    master: OwnedFd,
    keys: Vec<crate::scenario::KeyInput>,
    transcript: &Mutex<Vec<u8>>,
) {
    let mut f = std::fs::File::from(master);
    let mut seen_upto = 0usize;
    for k in keys {
        if let Some(needle) = &k.after {
            loop {
                let found = transcript.lock().ok().and_then(|t| {
                    let tail = t.get(seen_upto..)?;
                    let text = String::from_utf8_lossy(tail);
                    text.find(needle.as_str())
                        .map(|i| seen_upto + i + needle.len())
                });
                if let Some(end) = found {
                    seen_upto = end;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        std::thread::sleep(Duration::from_millis(k.delay_ms));
        let _ = f.write_all(unescape(&k.text).as_bytes());
    }
}

/// `\r`, `\n`, `\e`, `\t`, `\\` escapes in scenario key text.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('r') => out.push('\r'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('e') => out.push('\x1b'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

/// Write the simulated XRGB framebuffer as a binary PPM.
fn dump_ppm(path: &Path, fd: &OwnedFd, w: u32, h: u32) -> Result<(), String> {
    let len = (w * h * 4) as usize;
    let mut buf = vec![0u8; len];
    // SAFETY: pread from our memfd into an owned buffer.
    unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), len, 0) };
    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in buf.chunks_exact(4) {
        if let [b, g, r, _] = px {
            out.extend_from_slice(&[*r, *g, *b]);
        }
    }
    std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}

fn tempdir() -> Result<PathBuf, String> {
    let base = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let dir = base.join(format!("nmbl-simbox.{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}

fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let status = Command::new("cp")
        .args(["-a", "--no-preserve=ownership"])
        .arg(format!("{}/.", src.display()))
        .arg(dst)
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!("copying {} failed", src.display()));
    }
    let _ = Command::new("chmod").args(["-R", "u+w"]).arg(dst).status();
    Ok(())
}

#[cfg(test)]
#[allow(clippy::panic, reason = "tests assert")]
mod tests {
    use super::unescape;

    #[test]
    fn unescapes_key_text() {
        assert_eq!(unescape("pw\\r"), "pw\r");
        assert_eq!(unescape("\\e[B"), "\x1b[B");
    }
}
