//! Minimal argument parsing for `nmbl-sign` (no clap — the surface is small).
//!
//! Subcommands:
//!
//! ```text
//! nmbl-sign keygen --alg <ml-dsa-65|ml-dsa-87> --out-priv <f> --out-pub <f>
//! nmbl-sign keygen --alg <ml-dsa-65|ml-dsa-87> --stdio   (priv -> stdout, pub -> fd 3)
//! nmbl-sign sign   --key <priv-file> --domain <role> <input> [--out <sidecar>]
//! nmbl-sign sign   --key-stdin       --domain <role> <input> [--out <sidecar>]
//! nmbl-sign verify --key <public-file> --domain <role> <input> --sig <sidecar>
//! nmbl-sign sign-image …            (an alias of `sign`)
//! ```
//!
//! `--domain <role>` selects one of the eight frozen verifier roles (see
//! [`crate::domain::role_tokens`]). The parser is intentionally tiny and
//! flag-order-independent; every malformed invocation returns a [`SignError::Usage`]
//! that `main` prints alongside [`USAGE`].

use std::path::PathBuf;

use nmbl_init::sig::AlgId;

use crate::domain;
use crate::error::{Result, SignError};
pub use crate::sign::KeySource;

/// One-screen usage text, printed on a parse error or `--help`.
pub const USAGE: &str = "\
nmbl-sign — NMBL ML-DSA image signer

USAGE:
  nmbl-sign keygen --alg <ALG> --out-priv <FILE> --out-pub <FILE>
  nmbl-sign keygen --alg <ALG> --stdio
  nmbl-sign sign --key <PRIV> --domain <ROLE> <INPUT> [--out <SIDECAR>]
  nmbl-sign sign --key-stdin --domain <ROLE> <INPUT> [--out <SIDECAR>]
  nmbl-sign verify --key <PUB> --domain <ROLE> <INPUT> --sig <SIDECAR>
  nmbl-sign sign-image …   (alias of `sign`)

ALG:    ml-dsa-65 | ml-dsa-87
ROLE:   gen-kernel | gen-initrd | driver-image | staged-fragment |
        priority-file | rescue-sfs | boot-config | network-stage |
        generation-image
OUT:    sidecar path; defaults to <INPUT>.sig

--stdio      write the private key to stdout and the raw public key to
             fd 3; nothing touches disk (fails if fd 3 is not open)
--key-stdin  read the private key from stdin (at most 16 KiB); INPUT must
             then be a file path, never stdin

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
    /// Generate a keypair onto pipes: private container to stdout, raw public
    /// key to fd 3. Creates no files.
    KeygenStdio {
        /// Algorithm to generate.
        alg: AlgId,
    },
    /// Sign a file, producing a sidecar.
    Sign {
        /// Where the private key comes from (`--key <FILE>` or `--key-stdin`).
        key: KeySource,
        /// The resolved per-role domain byte string (frozen verifier const).
        domain: &'static [u8],
        /// The input file to sign.
        input: PathBuf,
        /// Explicit sidecar output path (else `<input>.sig`).
        out: Option<PathBuf>,
    },
    /// Verify a detached sidecar against one explicit trusted public key.
    Verify {
        key: PathBuf,
        domain: &'static [u8],
        input: PathBuf,
        signature: PathBuf,
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
        "verify" => parse_verify(rest),
        "-h" | "--help" | "help" => Ok(Command::Help),
        other => Err(SignError::Usage(format!("unknown subcommand `{other}`"))),
    }
}

fn parse_verify(args: &[String]) -> Result<Command> {
    let mut key = None;
    let mut domain = None;
    let mut signature = None;
    let mut input = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--key" => key = Some(PathBuf::from(next(&mut it, "--key")?)),
            "--domain" => domain = Some(parse_domain(next(&mut it, "--domain")?)?),
            "--sig" => signature = Some(PathBuf::from(next(&mut it, "--sig")?)),
            other if other.starts_with("--") => {
                return Err(SignError::Usage(format!(
                    "verify: unexpected flag `{other}`"
                )));
            }
            positional if input.is_none() => input = Some(PathBuf::from(positional)),
            _ => {
                return Err(SignError::Usage(
                    "verify: more than one input file given".into(),
                ));
            }
        }
    }
    Ok(Command::Verify {
        key: key.ok_or_else(|| SignError::Usage("verify: --key is required".into()))?,
        domain: domain.ok_or_else(|| SignError::Usage("verify: --domain is required".into()))?,
        input: input.ok_or_else(|| SignError::Usage("verify: an input file is required".into()))?,
        signature: signature.ok_or_else(|| SignError::Usage("verify: --sig is required".into()))?,
    })
}

/// Parse the `keygen` subcommand flags.
fn parse_keygen(args: &[String]) -> Result<Command> {
    let mut alg: Option<AlgId> = None;
    let mut out_priv: Option<PathBuf> = None;
    let mut out_pub: Option<PathBuf> = None;
    let mut stdio = false;

    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--alg" => alg = Some(parse_alg(next(&mut it, "--alg")?)?),
            "--out-priv" => out_priv = Some(PathBuf::from(next(&mut it, "--out-priv")?)),
            "--out-pub" => out_pub = Some(PathBuf::from(next(&mut it, "--out-pub")?)),
            "--stdio" => stdio = true,
            other => return Err(SignError::Usage(format!("keygen: unexpected `{other}`"))),
        }
    }
    let alg = alg.ok_or_else(|| SignError::Usage("keygen: --alg is required".into()))?;
    if stdio {
        if out_priv.is_some() || out_pub.is_some() {
            return Err(SignError::Usage(
                "keygen: --stdio cannot be combined with --out-priv/--out-pub".into(),
            ));
        }
        return Ok(Command::KeygenStdio { alg });
    }
    Ok(Command::Keygen {
        alg,
        out_priv: out_priv
            .ok_or_else(|| SignError::Usage("keygen: --out-priv is required".into()))?,
        out_pub: out_pub.ok_or_else(|| SignError::Usage("keygen: --out-pub is required".into()))?,
    })
}

/// Parse the `sign`/`sign-image` subcommand flags + positional input.
fn parse_sign(args: &[String]) -> Result<Command> {
    let mut key: Option<PathBuf> = None;
    let mut key_stdin = false;
    let mut domain: Option<&'static [u8]> = None;
    let mut out: Option<PathBuf> = None;
    let mut input: Option<PathBuf> = None;

    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--key" => key = Some(PathBuf::from(next(&mut it, "--key")?)),
            "--key-stdin" => key_stdin = true,
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
    let key = match (key, key_stdin) {
        (Some(_), true) => {
            return Err(SignError::Usage(
                "sign: --key and --key-stdin are mutually exclusive".into(),
            ));
        }
        (Some(path), false) => {
            if is_stdin_path(&path) {
                return Err(SignError::Usage(
                    "sign: use --key-stdin to read the key from stdin".into(),
                ));
            }
            KeySource::File(path)
        }
        (None, true) => KeySource::Stdin,
        (None, false) => {
            return Err(SignError::Usage(
                "sign: --key <FILE> or --key-stdin is required".into(),
            ));
        }
    };
    let input = input.ok_or_else(|| SignError::Usage("sign: an input file is required".into()))?;
    if key == KeySource::Stdin && is_stdin_path(&input) {
        return Err(SignError::Usage(
            "sign: with --key-stdin the input must be a file path, not stdin".into(),
        ));
    }
    Ok(Command::Sign {
        key,
        domain: domain.ok_or_else(|| SignError::Usage("sign: --domain is required".into()))?,
        input,
        out,
    })
}

/// Textual spellings of the process's stdin. `sign::run_from_source` also
/// compares device/inode at run time, which catches every other alias.
fn is_stdin_path(path: &std::path::Path) -> bool {
    matches!(
        path.to_str(),
        Some("-" | "/dev/stdin" | "/dev/fd/0" | "/proc/self/fd/0")
    )
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
    use nmbl_init::sig::{DOMAIN_NETWORK_STAGE, DOMAIN_RESCUE_SFS};

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
                key: KeySource::File(PathBuf::from("/k/sk")),
                domain: DOMAIN_RESCUE_SFS,
                input: PathBuf::from("/img/rescue.sfs"),
                out: None,
            }
        );
    }

    #[test]
    fn parses_network_stage_domain() {
        let cmd = parse(&argv(&[
            "sign",
            "--key",
            "/k/sk",
            "--domain",
            "network-stage",
            "/img/network.erofs",
        ]))
        .unwrap();
        assert_eq!(
            cmd,
            Command::Sign {
                key: KeySource::File(PathBuf::from("/k/sk")),
                domain: DOMAIN_NETWORK_STAGE,
                input: PathBuf::from("/img/network.erofs"),
                out: None,
            }
        );
    }

    #[test]
    fn parses_verify() {
        let cmd = parse(&argv(&[
            "verify",
            "--key",
            "/k/pub",
            "--domain",
            "boot-config",
            "--sig",
            "/b/config.sig",
            "/b/config",
        ]))
        .unwrap();
        match cmd {
            Command::Verify {
                key,
                domain,
                input,
                signature,
            } => {
                assert_eq!(key, PathBuf::from("/k/pub"));
                assert_eq!(domain, nmbl_init::sig::DOMAIN_BOOT_CONFIG);
                assert_eq!(input, PathBuf::from("/b/config"));
                assert_eq!(signature, PathBuf::from("/b/config.sig"));
            }
            _ => panic!("expected verify command"),
        }
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
    fn parses_keygen_stdio() {
        let cmd = parse(&argv(&["keygen", "--alg", "ml-dsa-65", "--stdio"])).unwrap();
        assert_eq!(
            cmd,
            Command::KeygenStdio {
                alg: AlgId::MlDsa65
            }
        );
    }

    #[test]
    fn keygen_stdio_rejects_output_files() {
        let err = parse(&argv(&[
            "keygen",
            "--alg",
            "ml-dsa-65",
            "--stdio",
            "--out-priv",
            "/k/sk",
        ]))
        .unwrap_err();
        assert!(matches!(err, SignError::Usage(_)));
    }

    #[test]
    fn parses_sign_key_stdin() {
        let cmd = parse(&argv(&[
            "sign",
            "--key-stdin",
            "--domain",
            "boot-config",
            "/b/config",
            "--out",
            "/b/c.sig",
        ]))
        .unwrap();
        assert_eq!(
            cmd,
            Command::Sign {
                key: KeySource::Stdin,
                domain: nmbl_init::sig::DOMAIN_BOOT_CONFIG,
                input: PathBuf::from("/b/config"),
                out: Some(PathBuf::from("/b/c.sig")),
            }
        );
    }

    #[test]
    fn key_stdin_with_stdin_input_is_rejected() {
        for input in ["-", "/dev/stdin", "/dev/fd/0", "/proc/self/fd/0"] {
            let err = parse(&argv(&[
                "sign",
                "--key-stdin",
                "--domain",
                "gen-kernel",
                input,
            ]))
            .unwrap_err();
            assert!(
                matches!(&err, SignError::Usage(m) if m.contains("not stdin")),
                "{input}: {err}"
            );
        }
    }

    #[test]
    fn key_and_key_stdin_are_exclusive() {
        let err = parse(&argv(&[
            "sign",
            "--key",
            "/k/sk",
            "--key-stdin",
            "--domain",
            "gen-kernel",
            "/img/k",
        ]))
        .unwrap_err();
        assert!(matches!(&err, SignError::Usage(m) if m.contains("mutually exclusive")));
    }

    #[test]
    fn key_path_naming_stdin_is_rejected() {
        let err = parse(&argv(&[
            "sign",
            "--key",
            "/dev/stdin",
            "--domain",
            "gen-kernel",
            "/i",
        ]))
        .unwrap_err();
        assert!(matches!(&err, SignError::Usage(m) if m.contains("--key-stdin")));
    }

    #[test]
    fn sign_without_key_is_usage_error() {
        let err = parse(&argv(&["sign", "--domain", "gen-kernel", "/i"])).unwrap_err();
        assert!(matches!(err, SignError::Usage(_)));
    }

    #[test]
    fn help_token_yields_help() {
        assert_eq!(parse(&argv(&["--help"])).unwrap(), Command::Help);
    }
}
