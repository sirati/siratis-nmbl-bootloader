//! Minimal argument parsing for `nmbl-sign` (no clap — the surface is small).
//!
//! Subcommands:
//!
//! ```text
//! nmbl-sign keygen --alg <ml-dsa-65|ml-dsa-87> --out-priv <f> --out-pub <f>
//! nmbl-sign sign   --key <priv-file> --domain <role> <input> [--out <sidecar>]
//! nmbl-sign sign-image …            (an alias of `sign`)
//! ```
//!
//! `--domain <role>` selects one of the six frozen verifier roles (see
//! [`crate::domain::role_tokens`]). The parser is intentionally tiny and
//! flag-order-independent; every malformed invocation returns a [`SignError::Usage`]
//! that `main` prints alongside [`USAGE`].

use std::path::PathBuf;

use nmbl_init::sig::AlgId;

use crate::domain;
use crate::error::{Result, SignError};

/// One-screen usage text, printed on a parse error or `--help`.
pub const USAGE: &str = "\
nmbl-sign — NMBL ML-DSA image signer

USAGE:
  nmbl-sign keygen --alg <ALG> --out-priv <FILE> --out-pub <FILE>
  nmbl-sign sign --key <PRIV> --domain <ROLE> <INPUT> [--out <SIDECAR>]
  nmbl-sign sign-image …   (alias of `sign`)

ALG:    ml-dsa-65 | ml-dsa-87
ROLE:   gen-kernel | gen-initrd | driver-image | staged-fragment |
        priority-file | rescue-sfs
OUT:    sidecar path; defaults to <INPUT>.sig

Writes detached NMBLSIG1 sidecars verified by nmbl-init's signature pipeline.";

/// The parsed command line: one of the two subcommands.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Generate a keypair.
    Keygen {
        /// Algorithm to generate.
        alg: AlgId,
        /// Where to write the private key.
        out_priv: PathBuf,
        /// Where to write the raw public-key bytes.
        out_pub: PathBuf,
    },
    /// Sign a file, producing a sidecar.
    Sign {
        /// Path to the private-key file.
        key: PathBuf,
        /// The resolved per-role domain byte string (frozen verifier const).
        domain: &'static [u8],
        /// The input file to sign.
        input: PathBuf,
        /// Explicit sidecar output path (else `<input>.sig`).
        out: Option<PathBuf>,
    },
    /// Print usage and exit zero.
    Help,
}

/// Parse `args` (WITHOUT the program name) into a [`Command`].
pub fn parse(args: &[String]) -> Result<Command> {
    let (sub, rest) = match args.split_first() {
        Some((s, r)) => (s.as_str(), r),
        None => return Err(SignError::Usage("missing subcommand".into())),
    };
    match sub {
        "keygen" => parse_keygen(rest),
        "sign" | "sign-image" => parse_sign(rest),
        "-h" | "--help" | "help" => Ok(Command::Help),
        other => Err(SignError::Usage(format!("unknown subcommand `{other}`"))),
    }
}

/// Parse the `keygen` subcommand flags.
fn parse_keygen(args: &[String]) -> Result<Command> {
    let mut alg: Option<AlgId> = None;
    let mut out_priv: Option<PathBuf> = None;
    let mut out_pub: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--alg" => alg = Some(parse_alg(next(&mut it, "--alg")?)?),
            "--out-priv" => out_priv = Some(PathBuf::from(next(&mut it, "--out-priv")?)),
            "--out-pub" => out_pub = Some(PathBuf::from(next(&mut it, "--out-pub")?)),
            other => return Err(SignError::Usage(format!("keygen: unexpected `{other}`"))),
        }
    }
    Ok(Command::Keygen {
        alg: alg.ok_or_else(|| SignError::Usage("keygen: --alg is required".into()))?,
        out_priv: out_priv
            .ok_or_else(|| SignError::Usage("keygen: --out-priv is required".into()))?,
        out_pub: out_pub.ok_or_else(|| SignError::Usage("keygen: --out-pub is required".into()))?,
    })
}

/// Parse the `sign`/`sign-image` subcommand flags + positional input.
fn parse_sign(args: &[String]) -> Result<Command> {
    let mut key: Option<PathBuf> = None;
    let mut domain: Option<&'static [u8]> = None;
    let mut out: Option<PathBuf> = None;
    let mut input: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--key" => key = Some(PathBuf::from(next(&mut it, "--key")?)),
            "--domain" => domain = Some(parse_domain(next(&mut it, "--domain")?)?),
            "--out" => out = Some(PathBuf::from(next(&mut it, "--out")?)),
            other if other.starts_with("--") => {
                return Err(SignError::Usage(format!("sign: unexpected flag `{other}`")));
            }
            positional => {
                if input.is_some() {
                    return Err(SignError::Usage(
                        "sign: more than one input file given".into(),
                    ));
                }
                input = Some(PathBuf::from(positional));
            }
        }
    }
    Ok(Command::Sign {
        key: key.ok_or_else(|| SignError::Usage("sign: --key is required".into()))?,
        domain: domain.ok_or_else(|| SignError::Usage("sign: --domain is required".into()))?,
        input: input.ok_or_else(|| SignError::Usage("sign: an input file is required".into()))?,
        out,
    })
}

/// Pull the value following a flag, or a usage error if it is missing.
fn next(it: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String> {
    it.next()
        .cloned()
        .ok_or_else(|| SignError::Usage(format!("{flag} needs a value")))
}

/// Parse an `--alg` token into an [`AlgId`].
fn parse_alg(token: String) -> Result<AlgId> {
    match token.as_str() {
        "ml-dsa-65" | "ML-DSA-65" => Ok(AlgId::MlDsa65),
        "ml-dsa-87" | "ML-DSA-87" => Ok(AlgId::MlDsa87),
        other => Err(SignError::Usage(format!(
            "unknown --alg `{other}` (expected ml-dsa-65 | ml-dsa-87)"
        ))),
    }
}

/// Parse a `--domain` role token into its frozen verifier domain const.
fn parse_domain(token: String) -> Result<&'static [u8]> {
    domain::domain_for(&token).ok_or_else(|| {
        SignError::Usage(format!(
            "unknown --domain `{token}` (expected one of: {})",
            domain::role_tokens()
        ))
    })
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests assert on known vectors and may panic on failure"
)]
mod tests {
    use super::*;
    use nmbl_init::sig::DOMAIN_RESCUE_SFS;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn parses_keygen() {
        let cmd = parse(&argv(&[
            "keygen",
            "--alg",
            "ml-dsa-87",
            "--out-priv",
            "/k/sk",
            "--out-pub",
            "/k/pk",
        ]))
        .unwrap();
        assert_eq!(
            cmd,
            Command::Keygen {
                alg: AlgId::MlDsa87,
                out_priv: PathBuf::from("/k/sk"),
                out_pub: PathBuf::from("/k/pk"),
            }
        );
    }

    #[test]
    fn parses_sign_with_default_out() {
        let cmd = parse(&argv(&[
            "sign",
            "--key",
            "/k/sk",
            "--domain",
            "rescue-sfs",
            "/img/rescue.sfs",
        ]))
        .unwrap();
        assert_eq!(
            cmd,
            Command::Sign {
                key: PathBuf::from("/k/sk"),
                domain: DOMAIN_RESCUE_SFS,
                input: PathBuf::from("/img/rescue.sfs"),
                out: None,
            }
        );
    }

    #[test]
    fn sign_image_is_an_alias() {
        let cmd = parse(&argv(&[
            "sign-image",
            "--key",
            "/k/sk",
            "--domain",
            "gen-kernel",
            "/img/k",
            "--out",
            "/s/k.sig",
        ]))
        .unwrap();
        match cmd {
            Command::Sign { out, .. } => assert_eq!(out, Some(PathBuf::from("/s/k.sig"))),
            other => panic!("expected Sign, got {other:?}"),
        }
    }

    #[test]
    fn unknown_domain_is_usage_error() {
        let err = parse(&argv(&["sign", "--key", "k", "--domain", "bogus", "f"])).unwrap_err();
        assert!(matches!(err, SignError::Usage(_)));
    }

    #[test]
    fn missing_subcommand_is_error() {
        assert!(matches!(parse(&[]), Err(SignError::Usage(_))));
    }

    #[test]
    fn help_token_yields_help() {
        assert_eq!(parse(&argv(&["--help"])).unwrap(), Command::Help);
    }
}
