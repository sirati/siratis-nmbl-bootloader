//! Trusted frontend adapter: validates an approved digest manifest and signs locally.
use crate::error::{Result, SignError};
use crate::{domain, keyfile, sign};
use base64::{Engine, engine::general_purpose::STANDARD};
use fips204::traits::{SerDes, Signer};
use fips204::{ml_dsa_65, ml_dsa_87};
use nmbl_init::sig::AlgId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use zeroize::Zeroizing;
const MAX_INPUT: u64 = 65536;
const REQUIRED: [&str; 5] = [
    "generation-image",
    "boot-config",
    "gen-kernel",
    "gen-initrd",
    "rescue-sfs",
];
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request<'a> {
    public_key_sha256: &'a str,
    // Borrow directly from the zeroized input; no unprotected secret String allocation.
    key_base64: &'a str,
    artifacts: Vec<Artifact<'a>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact<'a> {
    role: &'a str,
    sha512: &'a str,
    size: u64,
}
#[derive(Serialize)]
struct Response {
    signatures: Vec<Signature>,
}
#[derive(Serialize)]
struct Signature {
    role: String,
    sha512: String,
    size: u64,
    signature_base64: String,
}
fn invalid(message: &str) -> SignError {
    SignError::Usage(message.into())
}
fn hex<const N: usize>(text: &str) -> Result<[u8; N]> {
    if text.len() != N * 2
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid("digest must be exact lowercase hexadecimal"));
    }
    let mut result = [0; N];
    for (dst, pair) in result.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
        let mut bytes = pair.iter().copied();
        let a = bytes.next().ok_or_else(|| invalid("truncated digest"))?;
        let b = bytes.next().ok_or_else(|| invalid("truncated digest"))?;
        *dst = digit(a) * 16 + digit(b);
    }
    Ok(result)
}
fn public_key(key: &keyfile::PrivateKeyFile) -> Result<Vec<u8>> {
    macro_rules! derive {
        ($alg:ident) => {{
            let mut bytes = Zeroizing::new([0; $alg::SK_LEN]);
            if bytes.len() != key.sk.len() {
                return Err(invalid("invalid private key length"));
            }
            bytes.copy_from_slice(&key.sk);
            let private = Zeroizing::new(
                $alg::PrivateKey::try_from_bytes(*bytes)
                    .map_err(|_| invalid("invalid private key"))?,
            );
            Ok(private.get_public_key().into_bytes().to_vec())
        }};
    }
    match key.alg {
        AlgId::MlDsa65 => derive!(ml_dsa_65),
        AlgId::MlDsa87 => derive!(ml_dsa_87),
    }
}
/// No output is emitted until every role, digest and key binding has validated.
pub fn run(reader: &mut impl Read, writer: &mut impl Write) -> Result<()> {
    let mut input = Zeroizing::new(Vec::with_capacity(MAX_INPUT as usize + 1));
    reader
        .take(MAX_INPUT + 1)
        .read_to_end(&mut input)
        .map_err(|e| SignError::io("read signing request", e))?;
    if input.len() as u64 > MAX_INPUT {
        return Err(invalid("signing request exceeds 64 KiB"));
    }
    // This contract contains ASCII tokens/base64 only. Reject escapes before
    // serde can decode secret text into its non-zeroizing scratch buffer.
    if input.contains(&b'\\') {
        return Err(invalid(
            "escaped strings are not accepted in signing requests",
        ));
    }
    let request: Request<'_> =
        serde_json::from_slice(&input).map_err(|_| invalid("invalid signing request JSON"))?;
    let fingerprint = hex::<32>(request.public_key_sha256)?;
    if !(5..=6).contains(&request.artifacts.len()) {
        return Err(invalid("expected five or six signing roles"));
    }
    let mut seen = BTreeSet::new();
    let mut digests = Vec::new();
    for artifact in &request.artifacts {
        if (!REQUIRED.contains(&artifact.role) && artifact.role != "network-stage")
            || !seen.insert(artifact.role)
        {
            return Err(invalid("unknown or duplicate signing role"));
        }
        digests.push(hex::<64>(artifact.sha512)?);
    }
    if !REQUIRED.iter().all(|role| seen.contains(role)) {
        return Err(invalid("missing required signing role"));
    }
    let mut raw = Zeroizing::new(vec![0; request.key_base64.len()]);
    let length = STANDARD
        .decode_slice(request.key_base64, &mut raw)
        .map_err(|_| invalid("invalid private key base64"))?;
    raw.truncate(length);
    let key = keyfile::parse_private(&raw)?;
    let actual: [u8; 32] = Sha256::digest(public_key(&key)?).into();
    if actual != fingerprint {
        return Err(invalid("signing public key fingerprint mismatch"));
    }
    let mut signatures = Vec::new();
    for (artifact, digest) in request.artifacts.iter().zip(digests) {
        let domain =
            domain::domain_for(artifact.role).ok_or_else(|| invalid("invalid signing role"))?;
        signatures.push(Signature {
            role: artifact.role.into(),
            sha512: artifact.sha512.into(),
            size: artifact.size,
            signature_base64: STANDARD.encode(sign::sidecar_for_digest(&digest, &key, domain)?),
        });
    }
    serde_json::to_writer(&mut *writer, &Response { signatures })
        .map_err(|_| invalid("cannot write signatures JSON"))?;
    writer
        .write_all(b"\n")
        .map_err(|e| SignError::io("write signatures", e))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn request() -> (serde_json::Value, Vec<u8>) {
        let mut private = Zeroizing::new(Vec::new());
        let mut public = Vec::new();
        crate::keygen::run_to(AlgId::MlDsa65, &mut *private, &mut public).unwrap();
        let fingerprint = Sha256::digest(&public)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let artifacts = REQUIRED
            .iter()
            .map(|role| {
                serde_json::json!({
                    "role": role, "sha512": "12".repeat(64), "size": 123
                })
            })
            .collect::<Vec<_>>();
        (
            serde_json::json!({"public_key_sha256":fingerprint,"key_base64":STANDARD.encode(&*private),"artifacts":artifacts}),
            public,
        )
    }
    fn execute(value: &serde_json::Value) -> Result<Vec<u8>> {
        let input = Zeroizing::new(serde_json::to_vec(value).unwrap());
        let mut output = Vec::new();
        run(&mut input.as_slice(), &mut output)?;
        Ok(output)
    }
    #[test]
    fn all_sidecars_verify_with_real_nmbl_verifier() {
        let (mut req, public) = request();
        req["artifacts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"role":"network-stage", "sha512":"12".repeat(64), "size":5}));
        let output: serde_json::Value = serde_json::from_slice(&execute(&req).unwrap()).unwrap();
        let key = nmbl_init::sig::BakedKey::from_pubkey(&public, AlgId::MlDsa65).unwrap();
        for entry in output["signatures"].as_array().unwrap() {
            let bytes = STANDARD
                .decode(entry["signature_base64"].as_str().unwrap())
                .unwrap();
            let sidecar = nmbl_init::sig::SigSidecar::parse(&bytes).unwrap();
            let role = entry["role"].as_str().unwrap();
            nmbl_init::sig::verify_digest(
                &[0x12; 64],
                domain::domain_for(role).unwrap(),
                &sidecar,
                std::slice::from_ref(&key),
                nmbl_init::sig::VerifyPolicy::Enforce,
            )
            .unwrap();
            assert!(
                nmbl_init::sig::verify_digest(
                    &[0x13; 64],
                    domain::domain_for(role).unwrap(),
                    &sidecar,
                    std::slice::from_ref(&key),
                    nmbl_init::sig::VerifyPolicy::Enforce
                )
                .is_err()
            );
        }
    }
    #[test]
    fn rejects_manifest_and_key_binding_errors_without_output() {
        let (request, _) = request();
        let mut cases = Vec::new();
        let mut bad = request.clone();
        bad["public_key_sha256"] = "00".repeat(32).into();
        cases.push(bad);
        let mut bad = request.clone();
        bad["artifacts"][1]["role"] = "generation-image".into();
        cases.push(bad);
        let mut bad = request.clone();
        bad["artifacts"].as_array_mut().unwrap().pop();
        cases.push(bad);
        let mut bad = request.clone();
        bad["artifacts"][1]["role"] = "network-stage".into();
        cases.push(bad);
        let mut bad = request.clone();
        bad["artifacts"][0]["sha512"] = "AB".repeat(64).into();
        cases.push(bad);
        let mut bad = request.clone();
        bad["key_base64"] = "malformed SECRET".into();
        cases.push(bad);
        let mut bad = request.clone();
        bad["key_base64"] = STANDARD.encode(b"NMBLSK01").into();
        cases.push(bad);
        let mut bad = request.clone();
        bad["unexpected"] = "SECRET".into();
        cases.push(bad);
        for bad in cases {
            let bytes = Zeroizing::new(serde_json::to_vec(&bad).unwrap());
            let mut output = Vec::new();
            let error = run(&mut bytes.as_slice(), &mut output).unwrap_err();
            assert!(output.is_empty());
            assert!(!error.to_string().contains("SECRET"));
        }
    }
    #[test]
    fn bounded_and_truncated_input_is_rejected() {
        for bytes in [vec![b' '; 65537], b"{\"key_base64\":\"SECRET".to_vec()] {
            let mut output = Vec::new();
            let error = run(&mut bytes.as_slice(), &mut output).unwrap_err();
            assert!(output.is_empty());
            assert!(!error.to_string().contains("SECRET"));
        }
    }
    #[test]
    fn read_failure_is_propagated() {
        struct Failure;
        impl Read for Failure {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("read failed"))
            }
        }
        let mut output = Vec::new();
        assert!(run(&mut Failure, &mut output).is_err());
        assert!(output.is_empty());
    }
}
