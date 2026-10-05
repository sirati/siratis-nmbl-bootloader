use std::{io::Write, collections::BTreeSet, fs::OpenOptions, os::{fd::AsRawFd, unix::fs::{FileTypeExt, OpenOptionsExt}},
    process::{Command, Stdio}, thread, time::Duration};

fn serial(name: &str) -> bool {
    ["ttyS", "ttyAMA", "hvc", "ttyUSB", "ttyACM"].iter().any(|prefix|
        name.strip_prefix(prefix).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
}

// Linux VT ioctl requests take an integer third argument, not a pointer.
unsafe extern "C" { fn ioctl(fd: std::ffi::c_int, request: std::ffi::c_ulong, ...) -> std::ffi::c_int; }
fn vt_ioctl(file: &std::fs::File, request: std::ffi::c_ulong, argument: std::ffi::c_int) -> Result<(), String> {
    // SAFETY: the borrowed descriptor stays live and these Linux VT requests
    // accept exactly the supplied integer argument.
    if unsafe { ioctl(file.as_raw_fd(), request, argument) } < 0 {
        return Err(format!("VT ioctl {request:#x}: {}", std::io::Error::last_os_error()));
    }
    Ok(())
}
fn prepare_terminal(file: &std::fs::File, tty: &str) -> Result<(), String> {
    if tty == "tty1" {
        vt_ioctl(file, 0x5606, 1)?; // VT_ACTIVATE
        vt_ioctl(file, 0x5607, 1)?; // VT_WAITACTIVE
        vt_ioctl(file, 0x4b3a, 0)?; // KDSETMODE, KD_TEXT
        vt_ioctl(file, 0x4b45, 1)?; // KDSKBMODE, K_XLATE
    }
    // stty operates on this exact descriptor; never /dev/console. Restore
    // canonical input and echo for both virtual and serial terminals.
    let status = Command::new(env!("RESCUE_STTY")).arg("sane")
        .stdin(Stdio::from(file.try_clone().map_err(|e| e.to_string())?))
        .status().map_err(|e| e.to_string())?;
    if !status.success() { return Err(format!("terminal reset failed: {status}")); }
    Ok(())
}

fn shell(tty: &str, setsid: &str, bash: &str) -> Result<(), String> {
    // /dev/console cannot become a controlling terminal. Open the actual
    // device; setsid forks a new session and explicitly acquires it with
    // TIOCSCTTY before Bash establishes foreground process groups.
    let input = OpenOptions::new().read(true).write(true).custom_flags(0x20000 | 0x100)
        .open(format!("/dev/{tty}")).map_err(|e| e.to_string())?; // O_NOFOLLOW | O_NOCTTY
    if !input.metadata().map_err(|e| e.to_string())?.file_type().is_char_device() {
        return Err("console is not a character device".into());
    }
    prepare_terminal(&input, tty)?;
    let output = input.try_clone().map_err(|e| e.to_string())?;
    let error = input.try_clone().map_err(|e| e.to_string())?;
    let launcher = std::env::current_exe().map_err(|e| e.to_string())?;
    Command::new(setsid).args(["--fork", "--wait", "--ctty"]).arg(launcher)
        .args(["--shell-helper", bash])
        .stdin(Stdio::from(input)).stdout(Stdio::from(output)).stderr(Stdio::from(error))
        .env("TERM", if tty == "tty1" { "linux" } else { "vt100" })
        .status().map_err(|e| e.to_string())?;
    Ok(())
}

fn notify_ready() {
    let Ok(value) = std::env::var("NMBL_RESCUE_READY_FD") else { return; };
    let Ok(fd) = value.parse::<u32>() else {
        eprintln!("nmbl-rescue-console: invalid readiness descriptor");
        return;
    };
    if fd < 3 { eprintln!("nmbl-rescue-console: invalid readiness descriptor"); return; }
    // Duplicate the inherited pipe through procfs without taking ownership of
    // the descriptor shared by the launcher. Failed notification must never
    // kill an otherwise working operator shell.
    let result = OpenOptions::new().write(true).open(format!("/proc/self/fd/{fd}"))
        .and_then(|mut pipe| pipe.write_all(b"R"));
    if let Err(error) = result { eprintln!("nmbl-rescue-console: readiness notification: {error}"); }
}

fn spawn_shell(command: &mut Command, ready: impl FnOnce()) -> std::io::Result<std::process::Child> {
    // Command::spawn's exec-error pipe confirms actual Bash exec, rather
    // than merely successful setsid/helper launch.
    let child = command.spawn()?;
    ready();
    Ok(child)
}

fn shell_helper(bash: &str) -> Result<(), String> {
    let mut command = Command::new(bash);
    command.arg("-i").env_remove("NMBL_RESCUE_READY_FD");
    let mut child = spawn_shell(&mut command, notify_ready).map_err(|e| e.to_string())?;
    child.wait().map_err(|e| e.to_string())?;
    Ok(())
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--shell-helper" && args[2].starts_with('/') {
        if let Err(error) = shell_helper(&args[2]) {
            eprintln!("nmbl-rescue-console: shell exec: {error}");
            std::process::exit(1);
        }
        return;
    }
    if args.len() != 3 || !args[1..].iter().all(|p| p.starts_with('/')) {
        eprintln!("usage: nmbl-rescue-console ABSOLUTE_SETSID ABSOLUTE_BASH");
        std::process::exit(1);
    }
    let mut terminals = BTreeSet::from(["tty1".to_owned()]);
    if let Ok(active) = std::fs::read_to_string("/sys/class/tty/console/active") {
        terminals.extend(active.split_whitespace().filter(|name| serial(name)).map(str::to_owned));
    }
    let threads: Vec<_> = terminals.into_iter().map(|tty| {
        let setsid = args[1].clone();
        let bash = args[2].clone();
        thread::spawn(move || loop {
            if let Err(error) = shell(&tty, &setsid, &bash) {
                eprintln!("nmbl-rescue-console: {tty}: {error}");
            }
            // Exiting a shell must not terminate rescue or NMBL PID1.
            thread::sleep(Duration::from_secs(1));
        })
    }).collect();
    for thread in threads { let _ = thread.join(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn actual_exec_failure_never_announces_ready() {
        let notified = Cell::new(false);
        let mut command = Command::new("/nonexistent-nmbl-rescue-shell");
        assert!(spawn_shell(&mut command, || notified.set(true)).is_err());
        assert!(!notified.get());
    }
    #[test]
    fn actual_exec_success_announces_ready_before_shell_exit() {
        let notified = Cell::new(false);
        let mut command = Command::new(env!("RESCUE_TEST_BASH"));
        command.args(["-c", "exit 0"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = spawn_shell(&mut command, || notified.set(true)).unwrap();
        assert!(notified.get());
        assert!(child.wait().unwrap().success());
    }
}
