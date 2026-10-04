use std::{collections::BTreeSet, fs::OpenOptions, os::unix::fs::{FileTypeExt, OpenOptionsExt},
    process::{Command, Stdio}, thread, time::Duration};

fn serial(name: &str) -> bool {
    ["ttyS", "ttyAMA", "hvc", "ttyUSB", "ttyACM"].iter().any(|prefix|
        name.strip_prefix(prefix).is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
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
    let output = input.try_clone().map_err(|e| e.to_string())?;
    let error = input.try_clone().map_err(|e| e.to_string())?;
    Command::new(setsid).args(["--fork", "--wait", "--ctty", bash, "-i"])
        .stdin(Stdio::from(input)).stdout(Stdio::from(output)).stderr(Stdio::from(error))
        .env("TERM", if tty == "tty1" { "linux" } else { "vt100" })
        .status().map_err(|e| e.to_string())?;
    Ok(())
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
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
