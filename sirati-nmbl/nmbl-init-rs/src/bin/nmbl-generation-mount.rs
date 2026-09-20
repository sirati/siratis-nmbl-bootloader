//! Static systemd-initrd helper for mounting a signed generation image.

use std::path::PathBuf;
use std::process::ExitCode;

use nmbl_init::config::Config;
use nmbl_init::error::format_chain;
use nmbl_init::generation_mount::mount_verified_generation;

const USAGE: &str = "usage: nmbl-generation-mount CONFIG IMAGE SIGNATURE TARGET DEVICE_LINK";

#[derive(Debug, PartialEq, Eq)]
struct Args {
    config: PathBuf,
    image: PathBuf,
    signature: PathBuf,
    target: PathBuf,
    device_link: PathBuf,
}

fn parse_args<I, S>(values: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<std::ffi::OsString>,
{
    let values: Vec<PathBuf> = values
        .into_iter()
        .map(|value| value.into().into())
        .collect();
    let [config, image, signature, target, device_link] = values.as_slice() else {
        return Err(USAGE.to_string());
    };
    Ok(Args {
        config: config.clone(),
        image: image.clone(),
        signature: signature.clone(),
        target: target.clone(),
        device_link: device_link.clone(),
    })
}

fn run(args: &Args) -> nmbl_init::error::Result<()> {
    let config = Config::load(&args.config)?;
    mount_verified_generation(
        &config,
        &args.image,
        &args.signature,
        &args.target,
        &args.device_link,
    )
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "nmbl-generation-mount: {}",
                format_chain(&error as &dyn std::error::Error)
            );
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests may fail immediately while parsing fixed arguments"
)]
mod tests {
    use super::*;

    #[test]
    fn accepts_exact_contract() {
        let args = parse_args(["config", "image", "signature", "target", "device"])
            .expect("valid helper arguments");
        assert_eq!(args.target, PathBuf::from("target"));
        assert_eq!(args.device_link, PathBuf::from("device"));
    }

    #[test]
    fn rejects_missing_or_extra_values() {
        assert!(parse_args(["config", "image"]).is_err());
        assert!(parse_args(["a", "b", "c", "d", "e", "f"]).is_err());
    }
}
