//! Every pass that draws grid content must be CONTAINED.
//!
//! ── ★ WHY THIS IS A SOURCE-TEXT TEST AND NOT A TYPE ─────────────────────────
//!
//! `garasu::PanePass` makes an escaping draw unrepresentable *for a holder of a
//! `PanePass`* — it publishes no route to the raw pass, so nothing inside an
//! `in_pane` closure can paint outside its rect. What no type can enforce is
//! that a pass is ENTERED at all. `encoder.begin_render_pass(...)` followed by
//! `pass.draw(..)` is ordinary, compiles, and clips nothing.
//!
//! That is exactly how `mado_images` shipped: it drew Kitty image placements
//! straight onto the attachment with no scissor, so an image scrolled partly
//! off — or sized from a client-supplied cell count — painted over the tab bar
//! and the window chrome. Nothing clipped it, because fleet-wide
//! `set_scissor_rect` appeared in exactly one non-garasu file and only inside
//! comments.
//!
//! Tier: **only-mitigated (C4)**, ceiling stated rather than implied — this
//! reads `src/render.rs` as text. A pass added in another module is invisible to
//! it, and a sufficiently creative refactor can satisfy the string check while
//! escaping the intent. It catches the regression that actually happens: someone
//! simplifies the containment away because the longhand looked more direct.

/// The image pass must route through `LayeredPass::in_pane`.
#[test]
fn the_kitty_image_pass_is_clipped_to_a_pane() {
    let src = include_str!("../src/render.rs");

    let start = src
        .find("fn draw_kitty_images(")
        .expect("draw_kitty_images moved — update this guard, do not delete it");
    // The next `\n    fn ` at method indentation ends the body.
    let rest = &src[start..];
    let end = rest[1..].find("\n    fn ").map_or(rest.len(), |i| i + 1);
    let body = &rest[..end];

    // Strip comment lines: the prose above and inside legitimately discusses
    // the very tokens being checked.
    let code: String = body
        .lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        code.contains("in_pane("),
        "draw_kitty_images no longer enters a pane. Kitty placements would \
         paint over the tab bar and window chrome again — that is the defect \
         this routing removed, and it is invisible until an image is scrolled \
         partly off screen."
    );
    assert!(
        code.contains("LayeredPass::new"),
        "draw_kitty_images no longer constructs a LayeredPass"
    );

    // ANTI-VACUITY: prove the body was actually located and is substantive.
    // Without this, a `find` that silently matched an empty region would make
    // every assertion above pass over an empty string.
    assert!(
        code.contains("mado_images") && code.len() > 500,
        "the guard scanned the wrong region ({} bytes) — it did not find the \
         pass label it expects",
        code.len()
    );
}
