#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use nmbl_boot_update::{prepare, protocol, validate};
use nmbl_host_tools::keyfile;

fn main() -> ExitCode {
    match run(std::env::args().skip(1).collect()) {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("nmbl-boot-update: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Vec<String>) -> nmbl_boot_update::Result<String> {
    match args.as_slice() {
        [command, slot_arg, source, output, private, public] if command == "prepare" => {
            let slot = slot_arg.chars().next()
                .filter(|_| slot_arg.chars().count() == 1)
                .ok_or_else(|| nmbl_boot_update::Error::Invalid("invalid slot".into()))?;
            if private == "-" {
                // PRIVATE_KEY `-`: read the key once from stdin (bounded,
                // zeroized on drop); it signs the whole bundle and never
                // touches disk.
                let key = keyfile::read_private_from(&mut std::io::stdin().lock())
                    .map_err(|e| nmbl_boot_update::Error::Invalid(format!("read private key: {e}")))?;
                prepare::prepare_with_key(
                    slot, &PathBuf::from(source), &PathBuf::from(output),
                    &key, &PathBuf::from(public),
                )?;
            } else {
                prepare::prepare(
                    slot, &PathBuf::from(source), &PathBuf::from(output),
                    &PathBuf::from(private), &PathBuf::from(public),
                )?;
            }
            Ok("bundle prepared and verified".into())
        }
        [command, bundle, public] if command == "check" => {
            validate::bundle(&PathBuf::from(bundle), &PathBuf::from(public))?;
            Ok("bundle valid".into())
        }
        [command, socket, bundle, public] if command == "request" => protocol::request(
            &PathBuf::from(socket), &PathBuf::from(bundle), &PathBuf::from(public),
        ),
        [command, socket, spool, public, uid_arg, roots @ ..]
            if command == "serve" && !roots.is_empty() => {
            let uid = uid_arg.parse().map_err(|_| {
                nmbl_boot_update::Error::Invalid("invalid UID".into())
            })?;
            let roots = roots.iter().map(PathBuf::from).collect::<Vec<_>>();
            protocol::serve(
                &PathBuf::from(socket), &PathBuf::from(spool),
                &roots, &PathBuf::from(public), uid,
            )?;
            Ok("service stopped".into())
        }
        _ => Err(nmbl_boot_update::Error::Invalid(
            "usage: nmbl-boot-update prepare A|B SOURCE OUTPUT PRIVATE_KEY|- PUBLIC_KEY | check BUNDLE PUBLIC_KEY | request SOCKET BUNDLE PUBLIC_KEY | serve SOCKET SPOOL PUBLIC_KEY UID BOOT_ROOT...".into(),
        )),
    }
}
