//! `nmbl-host-tools` — host-side trust-root tooling for NMBL.
//!
//! The library half of `nmbl-sign`, the HOST-platform ML-DSA image signer. It
//! produces the exact `NMBLSIG1` detached sidecars the in-initramfs
//! `nmbl-init` verifier (`nmbl_init::sig`) checks, reusing that crate's SINGLE
//! sidecar-format definition (`sig::wire`, `AlgId`/`HashId`, the `DOMAIN_*` role
//! consts) so the signer's encoder is the literal inverse of the verifier's
//! decoder (FIX-25). No format bytes are redefined here.
//!
//! Modules:
//! - [`cli`] — tiny argument parser (subcommands + flags).
//! - [`domain`] — `--domain <role>` → frozen verifier domain const.
//! - [`error`] — the operator-facing [`error::SignError`].
//! - [`keyfile`] — public/private key-file I/O (`Zeroizing` secrets).
//! - [`keygen`] — the `keygen` subcommand.
//! - [`sign`] — the `sign`/`sign-image` subcommand (digest → sidecar).
//! - [`run`] — dispatch a parsed [`cli::Command`].

pub mod cli;
pub mod domain;
pub mod error;
pub mod keyfile;
pub mod keygen;
pub mod sign;

use error::Result;

/// Execute a parsed command. Returns `Ok(())` on success; the binary maps an
/// error to a non-zero exit. [`cli::Command::Help`] prints usage and succeeds.
pub fn run(cmd: cli::Command) -> Result<()> {
    match cmd {
        cli::Command::Keygen {
            alg,
            out_priv,
            out_pub,
        } => keygen::run(alg, &out_priv, &out_pub),
        cli::Command::Sign {
            key,
            domain,
            input,
            out,
        } => sign::run(&input, &key, domain, out.as_deref()).map(|_| ()),
        cli::Command::Help => {
            println!("{}", cli::USAGE);
            Ok(())
        }
    }
}
