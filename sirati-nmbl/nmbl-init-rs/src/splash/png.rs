//! PNG decode wrapper around the `png` crate.
//!
//! Every input is normalised to 8-bit RGBA8 so the rest of the
//! splash path can assume a single layout. Palette indices, low-bit
//! grayscale, 16-bit samples, and tRNS chunks are all collapsed into
//! one RGBA8 byte stream via `png::Transformations` plus a small
//! per-row fixup for color types that the crate does not expand all
//! the way for us (RGB without alpha, plain grayscale, grayscale +
//! alpha).

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use png::{ColorType, Transformations};

use crate::error::{NmblError, Result};

/// Decoded RGBA8 image. Pixels are row-major; no stride padding.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Decode the file at `path` into an [`Image`] containing tightly
/// packed 8-bit RGBA pixels.
///
/// All decode errors are folded into [`NmblError::Tui`] so the splash
/// caller can fall back to the tty UI on any failure.
pub fn decode_rgba(path: &Path) -> Result<Image> {
    let file = File::open(path).map_err(|e| NmblError::Tui {
        source: std::io::Error::other(format!(
            "splash::png: open {}: {e}",
            path.display()
        )),
    })?;
    decode_rgba_from_reader(BufReader::new(file))
}

/// Internal helper: works on any `Read`, which keeps the unit test
/// from having to touch the filesystem.
fn decode_rgba_from_reader<R: Read>(reader: R) -> Result<Image> {
    let mut decoder = png::Decoder::new(reader);
    // EXPAND : palette -> RGB, sub-8-bit gray -> 8-bit, tRNS -> alpha.
    // STRIP_16: 16-bit samples down to 8.
    // ALPHA  : palette -> RGBA (implies EXPAND).
    decoder.set_transformations(
        Transformations::EXPAND | Transformations::STRIP_16 | Transformations::ALPHA,
    );

    let mut reader = decoder.read_info().map_err(decode_err)?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(decode_err)?;

    let width = info.width;
    let height = info.height;

    // After EXPAND|ALPHA the crate may still hand us Rgb, Grayscale,
    // or GrayscaleAlpha; we convert those layouts to RGBA8 ourselves.
    // Palette images are expanded to Rgba by the ALPHA transformation
    // above; if the Indexed arm fires the transformation chain has
    // been changed and we no longer know how to interpret the buffer.
    let rgba = match info.color_type {
        ColorType::Rgba => {
            buf.truncate(info.buffer_size());
            buf
        }
        ColorType::Rgb => expand_rgb_to_rgba(&buf, width, height)?,
        ColorType::Grayscale => expand_gray_to_rgba(&buf, width, height)?,
        ColorType::GrayscaleAlpha => expand_gray_alpha_to_rgba(&buf, width, height)?,
        ColorType::Indexed => {
            return Err(NmblError::Tui {
                source: std::io::Error::other(
                    "splash::png: palette images must be expanded by png::Transformations::ALPHA \
                     before decode_rgba reaches them",
                ),
            });
        }
    };

    // Final shape check: width * height * 4 bytes, no padding.
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|p| p.checked_mul(4))
        .ok_or_else(|| NmblError::Tui {
            source: std::io::Error::other(format!(
                "splash::png: dimensions overflow ({width}x{height})"
            )),
        })?;
    if rgba.len() != expected {
        return Err(NmblError::Tui {
            source: std::io::Error::other(format!(
                "splash::png: decoded buffer is {} bytes, expected {expected}",
                rgba.len()
            )),
        });
    }

    Ok(Image {
        width,
        height,
        rgba,
    })
}

fn decode_err(e: png::DecodingError) -> NmblError {
    NmblError::Tui {
        source: std::io::Error::other(format!("splash::png: decode failed: {e}")),
    }
}

fn pixel_count(width: u32, height: u32) -> Result<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| NmblError::Tui {
            source: std::io::Error::other(format!(
                "splash::png: dimensions overflow ({width}x{height})"
            )),
        })
}

fn expand_rgb_to_rgba(src: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let n = pixel_count(width, height)?;
    let expected = n.checked_mul(3).ok_or_else(|| NmblError::Tui {
        source: std::io::Error::other("splash::png: RGB byte count overflow".to_string()),
    })?;
    if src.len() < expected {
        return Err(NmblError::Tui {
            source: std::io::Error::other(format!(
                "splash::png: RGB buffer too short ({} < {expected})",
                src.len()
            )),
        });
    }
    let mut out = Vec::with_capacity(n.saturating_mul(4));
    for px in src.chunks_exact(3).take(n) {
        // chunks_exact yields slices of length 3; safe to read with get().
        let r = px.first().copied().unwrap_or(0);
        let g = px.get(1).copied().unwrap_or(0);
        let b = px.get(2).copied().unwrap_or(0);
        out.extend_from_slice(&[r, g, b, 0xff]);
    }
    Ok(out)
}

fn expand_gray_to_rgba(src: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let n = pixel_count(width, height)?;
    if src.len() < n {
        return Err(NmblError::Tui {
            source: std::io::Error::other(format!(
                "splash::png: grayscale buffer too short ({} < {n})",
                src.len()
            )),
        });
    }
    let mut out = Vec::with_capacity(n.saturating_mul(4));
    for &v in src.iter().take(n) {
        out.extend_from_slice(&[v, v, v, 0xff]);
    }
    Ok(out)
}

fn expand_gray_alpha_to_rgba(src: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let n = pixel_count(width, height)?;
    let expected = n.checked_mul(2).ok_or_else(|| NmblError::Tui {
        source: std::io::Error::other(
            "splash::png: grayscale+alpha byte count overflow".to_string(),
        ),
    })?;
    if src.len() < expected {
        return Err(NmblError::Tui {
            source: std::io::Error::other(format!(
                "splash::png: grayscale+alpha buffer too short ({} < {expected})",
                src.len()
            )),
        });
    }
    let mut out = Vec::with_capacity(n.saturating_mul(4));
    for px in src.chunks_exact(2).take(n) {
        let v = px.first().copied().unwrap_or(0);
        let a = px.get(1).copied().unwrap_or(0xff);
        out.extend_from_slice(&[v, v, v, a]);
    }
    Ok(out)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    reason = "tests can panic on assertion failure; production lints are too strict for asserts"
)]
mod tests {
    use super::*;

    /// Canonical 70-byte 1x1 RGBA8 PNG storing a single opaque red
    /// pixel (R=255, G=0, B=0, A=255). Generated offline once and
    /// embedded so the test never touches the filesystem.
    const ONE_BY_ONE_RED_RGBA: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x00, 0x05, 0x00, 0x01, 0xff, 0x56, 0xc7, 0x2f, 0x0d, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn decode_one_by_one_red_rgba() {
        let img = decode_rgba_from_reader(ONE_BY_ONE_RED_RGBA).expect("1x1 red RGBA decodes");
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.rgba.as_slice(), &[0xff, 0x00, 0x00, 0xff]);
    }

    #[test]
    fn rejects_non_png_input() {
        // First four bytes are wrong on purpose; decoder must error.
        let garbage: &[u8] = b"this is not a png";
        assert!(decode_rgba_from_reader(garbage).is_err());
    }
}
