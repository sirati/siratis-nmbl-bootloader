//! Nearest-neighbor cover-style image scaler.
//!
//! Scales an RGBA8 source so it fills the framebuffer while preserving
//! aspect ratio; overflow on the longer axis is cropped (cover-style,
//! like CSS `background-size: cover`). Sampling is strictly
//! nearest-neighbor — no interpolation, no extra dependency.
//!
//! The output is a tight RGBA8 buffer of `dst.w * dst.h * 4` bytes.
//! The framebuffer's actual scanline stride (`dst.stride`) is handled
//! by the compositor when blitting; this function never emits stride
//! padding so unit tests can compare against `Vec<u8>` literals.

use crate::splash::types::FramebufferDims;

/// Cover-scale an RGBA8 source into a tight `dst.w * dst.h * 4` RGBA8
/// buffer using nearest-neighbor sampling.
///
/// Returns an empty vector if either source dimension is zero, the
/// destination dimensions are zero, or the source buffer is shorter
/// than `src_w * src_h * 4` bytes. The splash caller treats any empty
/// return as "skip the background blit", which is the right fallback
/// when assets are malformed.
pub fn cover_scale_nearest(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: FramebufferDims,
) -> Vec<u8> {
    if src_w == 0 || src_h == 0 || dst.w == 0 || dst.h == 0 {
        return Vec::new();
    }

    let src_w_usize = src_w as usize;
    let src_h_usize = src_h as usize;
    let dst_w_usize = dst.w as usize;
    let dst_h_usize = dst.h as usize;

    // Source byte count check, with overflow guard.
    let src_pixels = match src_w_usize.checked_mul(src_h_usize) {
        Some(n) => n,
        None => return Vec::new(),
    };
    let src_bytes = match src_pixels.checked_mul(4) {
        Some(n) => n,
        None => return Vec::new(),
    };
    if src.len() < src_bytes {
        return Vec::new();
    }

    // Output byte count, with overflow guard.
    let dst_pixels = match dst_w_usize.checked_mul(dst_h_usize) {
        Some(n) => n,
        None => return Vec::new(),
    };
    let dst_bytes = match dst_pixels.checked_mul(4) {
        Some(n) => n,
        None => return Vec::new(),
    };

    // scale = max(dst.w / src_w, dst.h / src_h). inv_scale is what we
    // actually multiply by to walk source coordinates per dst step.
    let src_w_f = f64::from(src_w);
    let src_h_f = f64::from(src_h);
    let dst_w_f = f64::from(dst.w);
    let dst_h_f = f64::from(dst.h);

    let scale_x = dst_w_f / src_w_f;
    let scale_y = dst_h_f / src_h_f;
    let scale = if scale_x >= scale_y { scale_x } else { scale_y };
    if !(scale.is_finite()) || scale <= 0.0 {
        return Vec::new();
    }
    let inv_scale = 1.0 / scale;

    // Crop window dimensions in source coordinates.
    let crop_w_f = dst_w_f * inv_scale;
    let crop_h_f = dst_h_f * inv_scale;
    let src_x0 = (src_w_f - crop_w_f) * 0.5;
    let src_y0 = (src_h_f - crop_h_f) * 0.5;

    let mut out = vec![0u8; dst_bytes];

    for y in 0..dst.h {
        let sy_f = src_y0 + f64::from(y) * inv_scale;
        // Clamp to [0, src_h - 1]. Truncation matches the task's
        // "nearest-neighbor (truncation, no interpolation)" spec.
        let sy = clamp_index(sy_f, src_h_usize);
        let src_row_start = match sy.checked_mul(src_w_usize).and_then(|p| p.checked_mul(4)) {
            Some(n) => n,
            None => return Vec::new(),
        };
        let dst_row_start = match (y as usize)
            .checked_mul(dst_w_usize)
            .and_then(|p| p.checked_mul(4))
        {
            Some(n) => n,
            None => return Vec::new(),
        };

        for x in 0..dst.w {
            let sx_f = src_x0 + f64::from(x) * inv_scale;
            let sx = clamp_index(sx_f, src_w_usize);

            let s_off = match src_row_start.checked_add(sx.saturating_mul(4)) {
                Some(n) => n,
                None => return Vec::new(),
            };
            let d_off = match dst_row_start.checked_add((x as usize).saturating_mul(4)) {
                Some(n) => n,
                None => return Vec::new(),
            };

            // Bounds-checked four-byte copy. If either window is short
            // for any reason, bail out with an empty result rather
            // than emit a half-filled buffer.
            let src_pixel = match src.get(s_off..s_off.saturating_add(4)) {
                Some(p) => p,
                None => return Vec::new(),
            };
            let dst_pixel = match out.get_mut(d_off..d_off.saturating_add(4)) {
                Some(p) => p,
                None => return Vec::new(),
            };
            dst_pixel.copy_from_slice(src_pixel);
        }
    }

    out
}

/// Clamp a source coordinate to `[0, max_exclusive - 1]` using floor
/// (truncation toward negative infinity), then cast to usize. The
/// caller guarantees `max_exclusive >= 1`.
fn clamp_index(coord: f64, max_exclusive: usize) -> usize {
    if !coord.is_finite() || coord <= 0.0 {
        return 0;
    }
    let limit = max_exclusive.saturating_sub(1);
    // floor() on a finite non-negative f64 fits in u64 for any
    // realistic framebuffer/image; cast via u64 then clamp.
    let floored = coord.floor();
    if floored >= u64::MAX as f64 {
        return limit;
    }
    let as_u64 = floored as u64;
    let as_usize = if as_u64 > usize::MAX as u64 {
        usize::MAX
    } else {
        as_u64 as usize
    };
    if as_usize > limit {
        limit
    } else {
        as_usize
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "tests can panic on assertion failure; production lints are too strict for asserts"
)]
mod tests {
    use super::*;

    fn fb(w: u32, h: u32) -> FramebufferDims {
        FramebufferDims {
            w,
            h,
            stride: w.saturating_mul(4),
        }
    }

    /// Build a 4x4 RGBA8 image where each row has a distinct colour
    /// band. Row 0 = red, row 1 = green, row 2 = blue, row 3 = white.
    fn four_row_bands() -> Vec<u8> {
        let bands: [[u8; 4]; 4] = [
            [0xff, 0x00, 0x00, 0xff],
            [0x00, 0xff, 0x00, 0xff],
            [0x00, 0x00, 0xff, 0xff],
            [0xff, 0xff, 0xff, 0xff],
        ];
        let mut out = Vec::with_capacity(4 * 4 * 4);
        for row in bands.iter() {
            for _ in 0..4 {
                out.extend_from_slice(row);
            }
        }
        out
    }

    #[test]
    fn empty_when_src_zero() {
        let v = cover_scale_nearest(&[], 0, 0, fb(8, 6));
        assert!(v.is_empty());
    }

    #[test]
    fn empty_when_dst_zero() {
        let src = four_row_bands();
        let v = cover_scale_nearest(&src, 4, 4, fb(0, 0));
        assert!(v.is_empty());
    }

    #[test]
    fn cover_scale_4x4_to_8x6_bands() {
        // 4x4 source scaled to 8x6 dst with cover semantics.
        //   scale_x = 8/4 = 2.0, scale_y = 6/4 = 1.5  ->  scale = 2.0
        //   inv_scale = 0.5
        //   crop_w = 8 * 0.5 = 4 (full src width)
        //   crop_h = 6 * 0.5 = 3 (crop 0.5 src rows top and bottom)
        //   src_y0 = (4 - 3) / 2 = 0.5
        //
        // Dst row y -> source y = floor(0.5 + y * 0.5):
        //   y=0 -> 0  (row 0, red)
        //   y=1 -> 1  (row 1, green)
        //   y=2 -> 1  (row 1, green)
        //   y=3 -> 2  (row 2, blue)
        //   y=4 -> 2  (row 2, blue)
        //   y=5 -> 3  (row 3, white)
        let src = four_row_bands();
        let out = cover_scale_nearest(&src, 4, 4, fb(8, 6));
        assert_eq!(out.len(), 8 * 6 * 4);

        let expected_rows: [[u8; 4]; 6] = [
            [0xff, 0x00, 0x00, 0xff],
            [0x00, 0xff, 0x00, 0xff],
            [0x00, 0xff, 0x00, 0xff],
            [0x00, 0x00, 0xff, 0xff],
            [0x00, 0x00, 0xff, 0xff],
            [0xff, 0xff, 0xff, 0xff],
        ];
        for (y, want) in expected_rows.iter().enumerate() {
            for x in 0..8 {
                let off = (y * 8 + x) * 4;
                let pixel = &out[off..off + 4];
                assert_eq!(
                    pixel, want,
                    "row {y} col {x}: got {pixel:?}, want {want:?}",
                );
            }
        }
    }

    #[test]
    fn cover_scale_handles_short_source() {
        // src buffer claims to be 4x4 RGBA but is only 8 bytes long.
        // We must return empty rather than read out of bounds.
        let short = vec![0u8; 8];
        let out = cover_scale_nearest(&short, 4, 4, fb(8, 6));
        assert!(out.is_empty());
    }
}
