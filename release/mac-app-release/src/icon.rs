//! AppIcon `.icns` generation.
//!
//! Two paths, both typed:
//!   - [`IconSource::Svg`] — rasterize the source SVG to the 10 canonical
//!     iconset PNG slots with the pure-Rust `resvg` crate (the SAME rasterizer
//!     substrate's Nix `mkDarwinAppBundle` uses), then assemble the `.icns` with
//!     the Mac-native `iconutil` (the one unavoidable tool — there is no
//!     pure-Rust `.icns` writer on the host; png2icns isn't on a stock Mac).
//!   - [`IconSource::Icns`] — copy a pre-built `.icns` verbatim.
//!
//! Reuses gaveta-release's `sips`/`iconutil`-from-PNG shape, but takes the icon
//! from a real SVG instead of a generated placeholder — mado ships an authored
//! icon (`assets/mado-icon.svg`) and we honor it.

use std::path::Path;

use image::{ImageBuffer, Rgba};
use resvg::tiny_skia;
use resvg::usvg;

use crate::cmd::Tool;
use crate::error::{IoPathExt, ReleaseError};
use crate::spec::IconSource;

/// The 10 canonical macOS iconset slots: `(pixel_size, iconset_name)`.
const ICONSET_SLOTS: &[(u32, &str)] = &[
    (16, "icon_16x16.png"),
    (32, "icon_16x16@2x.png"),
    (32, "icon_32x32.png"),
    (64, "icon_32x32@2x.png"),
    (128, "icon_128x128.png"),
    (256, "icon_128x128@2x.png"),
    (256, "icon_256x256.png"),
    (512, "icon_256x256@2x.png"),
    (512, "icon_512x512.png"),
    (1024, "icon_512x512@2x.png"),
];

/// Build `<app>.icns` at `icns_out` from the spec's [`IconSource`], staging the
/// iconset under `iconset_dir`. For an SVG source the rasterization is pure
/// Rust; the final assembly uses `iconutil`.
///
/// # Errors
/// Returns a typed [`ReleaseError`] on SVG parse/render, PNG write, fs, or
/// `iconutil` failure.
pub fn build_icns(
    icon: &IconSource,
    iconset_dir: &Path,
    icns_out: &Path,
) -> Result<(), ReleaseError> {
    match icon {
        IconSource::Icns(src) => {
            if let Some(parent) = icns_out.parent() {
                std::fs::create_dir_all(parent).at(parent)?;
            }
            std::fs::copy(src, icns_out).at(icns_out)?;
            Ok(())
        }
        IconSource::Svg(svg) => build_icns_from_svg(svg, iconset_dir, icns_out),
    }
}

fn build_icns_from_svg(
    svg: &Path,
    iconset_dir: &Path,
    icns_out: &Path,
) -> Result<(), ReleaseError> {
    std::fs::create_dir_all(iconset_dir).at(iconset_dir)?;

    let svg_bytes = std::fs::read(svg).at(svg)?;
    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_data(&svg_bytes, &opts)
        .map_err(|e| ReleaseError::Svg(e.to_string()))?;

    for &(size, name) in ICONSET_SLOTS {
        let png = rasterize(&tree, size)?;
        let out = iconset_dir.join(name);
        png.save(&out)?;
    }

    if let Some(parent) = icns_out.parent() {
        std::fs::create_dir_all(parent).at(parent)?;
    }
    Tool::new("iconutil")
        .arg("-c")
        .arg("icns")
        .arg(iconset_dir)
        .arg("-o")
        .arg(icns_out)
        .run()
}

/// Rasterize the SVG tree to a square `size`×`size` RGBA PNG buffer, scaling to
/// fit the SVG's intrinsic size into the target box.
fn rasterize(
    tree: &usvg::Tree,
    size: u32,
) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>, ReleaseError> {
    let mut pixmap = tiny_skia::Pixmap::new(size, size)
        .ok_or_else(|| ReleaseError::Svg("zero-sized pixmap".to_owned()))?;

    let svg_size = tree.size();
    let scale = (size as f32 / svg_size.width()).min(size as f32 / svg_size.height());
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(tree, transform, &mut pixmap.as_mut());

    let buf: ImageBuffer<Rgba<u8>, Vec<u8>> =
        ImageBuffer::from_raw(size, size, pixmap.data().to_vec())
            .ok_or_else(|| ReleaseError::Svg("pixmap → image buffer mismatch".to_owned()))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iconset_has_ten_canonical_slots() {
        assert_eq!(ICONSET_SLOTS.len(), 10);
        // The @2x slots match the next size up in pixels.
        assert!(ICONSET_SLOTS.contains(&(32, "icon_16x16@2x.png")));
        assert!(ICONSET_SLOTS.contains(&(1024, "icon_512x512@2x.png")));
    }

    #[test]
    fn rasterizes_a_minimal_svg() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><rect width="100" height="100" fill="#88C0D0"/></svg>"##;
        let opts = usvg::Options::default();
        let tree = usvg::Tree::from_data(svg.as_bytes(), &opts).unwrap();
        let img = rasterize(&tree, 64).unwrap();
        assert_eq!(img.width(), 64);
        assert_eq!(img.height(), 64);
    }
}
