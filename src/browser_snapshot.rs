//! Encode a rendered browser page (RGBA8) to a base64 PNG for the MCP
//! `browser_snapshot_get` surface.
//!
//! The GPU render (a surface's `DisplayList` → RGBA8 via
//! `browser_engine::render_display_list_to_rgba`) is the Environment side
//! effect, done on the render tick. THIS module is the pure, testable encode:
//! RGBA8 + dimensions → PNG bytes → base64. No GPU, no network — a test feeds a
//! synthetic RGBA buffer and asserts a valid PNG signature round-trips. Reuses
//! `data_encoding::BASE64` (mado's existing base64, per OSC-52) — no new dep.

/// Encode an RGBA8 pixel buffer (`width*height*4` bytes, row-major) to a base64
/// PNG string. Returns `None` on a zero dimension, a buffer whose length
/// doesn't match the claimed dimensions, or an encoder failure — never panics
/// on bad input.
#[must_use]
pub fn encode_png_base64(rgba: &[u8], width: u32, height: u32) -> Option<String> {
    if width == 0 || height == 0 {
        return None;
    }
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if rgba.len() != expected {
        return None;
    }
    use image::ImageEncoder;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .ok()?;
    Some(data_encoding::BASE64.encode(&png))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_2x2_rgba_encodes_to_a_valid_base64_png() {
        // 2×2 = 16 bytes RGBA (opaque white).
        let rgba = vec![255u8; 16];
        let b64 = encode_png_base64(&rgba, 2, 2).expect("encodes");
        let png = data_encoding::BASE64
            .decode(b64.as_bytes())
            .expect("valid base64");
        // The 8-byte PNG signature.
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        // Round-trips back to a 2×2 image with the same pixels.
        let img = image::load_from_memory(&png).expect("decodes");
        assert_eq!((img.width(), img.height()), (2, 2));
    }

    #[test]
    fn a_mismatched_buffer_length_is_rejected_not_panicked() {
        // Claims 2×2 (needs 16 bytes) but only 4 are provided.
        assert!(encode_png_base64(&[0u8; 4], 2, 2).is_none());
    }

    #[test]
    fn a_zero_dimension_is_rejected() {
        assert!(encode_png_base64(&[], 0, 4).is_none());
        assert!(encode_png_base64(&[], 4, 0).is_none());
    }
}
