//! The stage-2 rescue image: opened once, checked against the digest the
//! runtime config pins, and then loop-bound over that same descriptor.
//!
//! The rescue is staged. Stage 1 is NMBL itself: its initramfs carries the
//! one filesystem module the image needs (`erofs` for the full-system
//! rescue) and its runtime config names the image, its format and its
//! SHA-512. Stage 2 is the image: the whole recovery system, including its
//! network drivers and network profile. Every check here reads the bytes
//! through the single [`Stage2Image::file`] descriptor that
//! [`super::disk`] later binds to a loop device, so a path swapped after the
//! check cannot change what is mounted.

use std::fs::File;
use std::io;
use std::path::PathBuf;

use crate::config::Config;
use crate::error::{NmblError, Result};

/// The located rescue image and the result of opening it once.
pub struct Stage2Image {
    pub path: PathBuf,
    /// The pinned descriptor (or why it could not be opened; signature
    /// policy decides whether that refuses or falls through).
    pub file: io::Result<File>,
}

/// Locate and open the configured rescue image exactly once.
pub fn open(config: &Config) -> Result<Stage2Image> {
    let path = super::locate_sfs(config)?;
    let file = File::open(&path);
    Ok(Stage2Image { path, file })
}

/// Enforce a configured SHA-512 pin over `file`. `known` is the digest the
/// signature check already streamed over the same descriptor, so a signed
/// image is read only once. Without a pin this is a no-op; a malformed pin
/// or a binary that cannot hash fails closed.
pub(crate) fn check_pin(
    expected: Option<&str>,
    file: &File,
    known: Option<[u8; 64]>,
    what: &str,
) -> Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let expected = parse_pin(expected, what)?;
    let actual = match known {
        Some(digest) => digest,
        None => digest_of(file, what)?,
    };
    if actual == expected {
        Ok(())
    } else {
        Err(NmblError::Rescue {
            stage: "image-digest",
            source: Box::new(NmblError::ConfigInvalid {
                reason: format!(
                    "{what} does not match the SHA-512 pinned by the boot configuration \
                     (got {})",
                    crate::util::hex::hex_lower(&actual)
                ),
                context: "rescue stage-2 integrity".to_string(),
            }),
        })
    }
}

fn parse_pin(pin: &str, what: &str) -> Result<[u8; 64]> {
    crate::util::hex::decode_fixed::<64>(pin).ok_or_else(|| NmblError::Rescue {
        stage: "image-digest",
        source: Box::new(NmblError::ConfigInvalid {
            reason: format!("the SHA-512 pinned for {what} is not 128 hex digits"),
            context: "rescue stage-2 integrity".to_string(),
        }),
    })
}

#[cfg(any(feature = "secure-boot", feature = "rescue-stages"))]
fn digest_of(file: &File, what: &str) -> Result<[u8; 64]> {
    use std::os::fd::AsFd;
    let (digest, _len) =
        crate::util::hash::sha512_fd(file.as_fd()).map_err(|source| NmblError::Rescue {
            stage: "image-digest",
            source: Box::new(NmblError::Io {
                source: io::Error::other(source.to_string()),
                context: format!("hashing {what}"),
            }),
        })?;
    Ok(digest)
}

#[cfg(not(any(feature = "secure-boot", feature = "rescue-stages")))]
fn digest_of(_file: &File, what: &str) -> Result<[u8; 64]> {
    Err(NmblError::Rescue {
        stage: "image-digest",
        source: Box::new(NmblError::ConfigInvalid {
            reason: format!(
                "the boot configuration pins {what}, but this NMBL was built without \
                 digest support (feature rescue-stages)"
            ),
            context: "rescue stage-2 integrity".to_string(),
        }),
    })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests assert on contract failures"
)]
mod tests {
    use super::*;
    use std::io::Write;

    #[cfg(any(feature = "secure-boot", feature = "rescue-stages"))]
    fn sha512_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha512};
        crate::util::hex::hex_lower(&Sha512::digest(bytes))
    }

    fn image(bytes: &[u8]) -> (tempfile::NamedTempFile, File) {
        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(bytes).expect("write image");
        let file = File::open(tmp.path()).expect("open image");
        (tmp, file)
    }

    #[test]
    fn no_pin_accepts_any_image() {
        let (_tmp, file) = image(b"anything");
        check_pin(None, &file, None, "rescue image").expect("unpinned");
    }

    #[test]
    fn malformed_pin_fails_closed() {
        let (_tmp, file) = image(b"anything");
        for pin in ["", "abc", &"g".repeat(128)] {
            assert!(
                check_pin(Some(pin), &file, None, "rescue image").is_err(),
                "{pin:?}"
            );
        }
    }

    #[cfg(any(feature = "secure-boot", feature = "rescue-stages"))]
    #[test]
    fn pin_accepts_the_exact_image_and_refuses_any_other() {
        let (_tmp, file) = image(b"stage two");
        let good = sha512_hex(b"stage two");
        check_pin(Some(&good), &file, None, "rescue image").expect("matching image");
        check_pin(Some(&good.to_uppercase()), &file, None, "rescue image")
            .expect("hex case is not significant");

        let (_tmp2, other) = image(b"stage two, older but validly signed");
        match check_pin(Some(&good), &other, None, "rescue image") {
            Err(NmblError::Rescue { stage, .. }) => assert_eq!(stage, "image-digest"),
            other => panic!("substituted image accepted: {other:?}"),
        }
    }

    #[cfg(any(feature = "secure-boot", feature = "rescue-stages"))]
    #[test]
    fn pin_reuses_the_signature_digest() {
        // The digest handed in is authoritative: it was streamed over the
        // same descriptor by the signature check.
        let (_tmp, file) = image(b"bytes on disk");
        let pinned = sha512_hex(b"bytes the signature check saw");
        let mut seen = [0u8; 64];
        seen.copy_from_slice(&crate::util::hex::decode_fixed::<64>(&pinned).expect("hex"));
        check_pin(Some(&pinned), &file, Some(seen), "rescue image").expect("reused digest");
    }

    #[cfg(not(any(feature = "secure-boot", feature = "rescue-stages")))]
    #[test]
    fn pin_without_digest_support_fails_closed() {
        let (_tmp, file) = image(b"stage two");
        assert!(check_pin(Some(&"0".repeat(128)), &file, None, "rescue image").is_err());
    }
}
