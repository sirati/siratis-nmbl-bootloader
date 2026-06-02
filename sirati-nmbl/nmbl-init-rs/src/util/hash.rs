//! Streaming SHA-512 / SHA-256 hashers (gated `network-rescue`/`secure-boot`).
//!
//! NEW code (master-plan §A streaming-hash ruling / FIX-23 / FIX-63):
//! `download.rs` only ever streamed SHA-256 into a memfd sink and has neither
//! SHA-512 nor a file-path streamer. These mirror that crate's
//! `Sha256::new()/update()/finalize()` idiom but over a generic `Read` and a
//! `&Path`, so the secure-boot verify/measure paths can hash a kernel/initrd
//! over a single pinned fd without slurping it into RAM.
//!
//! Gated `any(feature = "network-rescue", feature = "secure-boot")` because it
//! `use`s the optional `sha2` dep — an ungated import would break the default
//! (feature-free) build. `util::hex` stays ungated (no `sha2`).

use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256, Sha512};

use crate::error::{NmblError, Result};

/// Chunk size for the streaming read loop. 64 KiB matches the typical pipe
/// buffer and keeps the working set tiny while amortizing syscall overhead.
const CHUNK: usize = 64 * 1024;

/// Stream `reader` through a SHA-512 hasher, returning the 64-byte digest.
/// Reads in fixed-size chunks so an arbitrarily large source never lands in
/// RAM in one piece (mirrors `download.rs`'s streaming sink).
pub fn sha512_reader<R: Read>(mut reader: R) -> Result<[u8; 64]> {
    let mut hasher = Sha512::new();
    let mut buf = [0u8; CHUNK];
    loop {
        let n = reader.read(&mut buf).map_err(|source| NmblError::Io {
            source,
            context: "sha512_reader: read".to_string(),
        })?;
        if n == 0 {
            break;
        }
        let chunk = buf.get(..n).unwrap_or(&buf);
        hasher.update(chunk);
    }
    Ok(hasher.finalize().into())
}

/// Open `path` read-only and stream it through [`sha512_reader`].
pub fn sha512_file(path: &Path) -> Result<[u8; 64]> {
    let file = File::open(path).map_err(|source| NmblError::Io {
        source,
        context: format!("sha512_file: open {}", path.display()),
    })?;
    sha512_reader(file)
}

/// Stream `reader` through a SHA-256 hasher, returning the 32-byte digest.
pub fn sha256_reader<R: Read>(mut reader: R) -> Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; CHUNK];
    loop {
        let n = reader.read(&mut buf).map_err(|source| NmblError::Io {
            source,
            context: "sha256_reader: read".to_string(),
        })?;
        if n == 0 {
            break;
        }
        let chunk = buf.get(..n).unwrap_or(&buf);
        hasher.update(chunk);
    }
    Ok(hasher.finalize().into())
}

/// Open `path` read-only and stream it through [`sha256_reader`].
pub fn sha256_file(path: &Path) -> Result<[u8; 32]> {
    let file = File::open(path).map_err(|source| NmblError::Io {
        source,
        context: format!("sha256_file: open {}", path.display()),
    })?;
    sha256_reader(file)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "tests can panic on assertion failure"
)]
mod tests {
    use super::*;
    use crate::util::hex::hex_lower;

    /// Wrap an in-memory slice as a `Read` source for the streaming KATs.
    fn cursor(bytes: &[u8]) -> std::io::Cursor<&[u8]> {
        std::io::Cursor::new(bytes)
    }

    // Canonical NIST/RFC vectors. Pinning these catches an accidental
    // algorithm swap or a chunk-boundary bug in the streaming loop.
    const SHA512_EMPTY: &str = "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc8\
3f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e";
    const SHA512_ABC: &str = "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a\
9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f";
    const SHA256_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const SHA256_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn sha512_reader_empty_matches_canonical() {
        assert_eq!(
            hex_lower(&sha512_reader(cursor(b"")).unwrap()),
            SHA512_EMPTY
        );
    }

    #[test]
    fn sha512_reader_abc_matches_canonical() {
        assert_eq!(
            hex_lower(&sha512_reader(cursor(b"abc")).unwrap()),
            SHA512_ABC
        );
    }

    #[test]
    fn sha256_reader_empty_matches_canonical() {
        assert_eq!(
            hex_lower(&sha256_reader(cursor(b"")).unwrap()),
            SHA256_EMPTY
        );
    }

    #[test]
    fn sha256_reader_abc_matches_canonical() {
        assert_eq!(
            hex_lower(&sha256_reader(cursor(b"abc")).unwrap()),
            SHA256_ABC
        );
    }

    /// A payload larger than `CHUNK` exercises the multi-iteration read loop
    /// and a sub-chunk final read; the digest must still match a one-shot
    /// hash of the same bytes.
    #[test]
    fn sha512_reader_spans_multiple_chunks() {
        let data = vec![0xa5u8; CHUNK * 2 + 123];
        let streamed = hex_lower(&sha512_reader(cursor(&data)).unwrap());
        let oneshot = {
            let mut h = Sha512::new();
            h.update(&data);
            hex_lower(&h.finalize())
        };
        assert_eq!(streamed, oneshot);
    }

    #[test]
    fn sha512_file_matches_reader() {
        let mut path = std::env::temp_dir();
        path.push(format!("nmbl-hash-kat-{}.bin", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let from_file = hex_lower(&sha512_file(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        assert_eq!(from_file, SHA512_ABC);
    }

    #[test]
    fn sha256_file_matches_reader() {
        let mut path = std::env::temp_dir();
        path.push(format!("nmbl-hash-kat256-{}.bin", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        let from_file = hex_lower(&sha256_file(&path).unwrap());
        let _ = std::fs::remove_file(&path);
        assert_eq!(from_file, SHA256_ABC);
    }

    #[test]
    fn sha512_file_missing_path_errors() {
        let path = Path::new("/nonexistent/nmbl/hash/kat/should/not/exist");
        assert!(sha512_file(path).is_err());
    }
}
