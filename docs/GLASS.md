# Glass and window chrome

A translucent window with full-strength text, a rounded shape, a hairline rim,
and a band mado draws itself. Every default changes nothing: an opaque window
renders the same bytes as before.

## Settings

| Key | Type | Default | Meaning |
|---|---|---|---|
| `appearance.opacity` | f32 | 1.0 | Below 1.0 asks for a composited surface when the window is created. Live across values below 1.0; crossing 1.0 needs a restart. |
| `appearance.glass.frost.enabled` | bool | false | Faint rime from the corners and edges. Active only on a composited surface. |
| `appearance.glass.frost.intensity` | f32 | 0.16 | Peak opacity of a hairline at the window edge, 0 to 1. |
| `appearance.glass.frost.reach` | f32 | 28.0 | How far in from the edge, logical px. |
| `appearance.glass.frost.color` | string | `#D8DEE9` | sRGB hex. One colour. |
| `appearance.glass.frost.seed` | u32 | 1 | The same seed grows the same rime. |
| `window.corner_radius` | f32 | 0.0 | Logical px. Corners go fully transparent. |
| `window.rim.width` | f32 | 0.0 | Logical px, inside the edge. 0 draws no rim. |
| `window.rim.color` | string | `#ECEFF4` | sRGB hex. |
| `window.rim.opacity` | f32 | 0.22 | 0 to 1. |
| `window.chrome.enabled` | bool | false | Draw mado's band. |
| `window.chrome.height` | f32 | 30.0 | Logical px. |
| `window.chrome.edge_width` | f32 | 6.0 | Resize grab, logical px. 0 turns resizing off. |
| `window.chrome.button_width` | f32 | 40.0 | Logical px per button. |
| `window.chrome.tint` | f32 | 0.18 | Peak tint at the top edge, fading to nothing at the terminal. |
| `window.chrome.title` | bool | true | Show the terminal title when it renders cleanly. |

Corner radius and rim are active only on a composited surface, and not while
maximized or fullscreen. `appearance.background_blur` has no consumer: it does
nothing on any platform. A glass pane is sharp, so it is not part of this.

## The frame

`render` leases one target, `out_view`. Every pass that used to write the
surface writes it. The last pass, `glass::GlassPipeline::resolve`, is the only
one that writes the surface. With no alpha granted there is no lease, `out_view`
is the surface, and the frame is today's frame.

Inside the lease every pass blends in linear light and the destination is
optically premultiplied, which is what `ALPHA_BLENDING` with straight sources
produces. The resolve converts each pixel for the compositor:

1. Draw the rim over the scene.
2. Raise alpha to the brightest channel. Bloom, glow and snow add light with no
   alpha; this keeps that light and never stores an invalid pixel.
3. Scale alpha by the rounded shape's coverage.
4. Store `sRGB(rgb / a) * a` on a premultiplied surface, `rgb / a` on a
   post-multiplied one.

Step 4 is why the pass exists. Wayland's default is premultiplied in electrical
values and mutter blends sRGB content in that space, so a linear premultiply
through an sRGB surface stores too much light: nord0 at 50% stores 31 where 23
is right, and a white glyph edge stores more light than its alpha.

Metal's `PostMultiplied` is `CAMetalLayer.opaque = false`, which Core Animation
composites as premultiplied, so mado treats it as premultiplied there.

### What else changes on glass only

- A cell on the terminal's default background paints no rect. `apply_theme`
  makes the pen background the theme colour, so an opaque window paints those
  cells in that colour and nobody can tell. On glass they would be opaque
  blocks behind the text.
- Hidden text and the off phase of blinking text draw no glyph. They are drawn
  in the background colour on an opaque window.
- Grain and aurora scale with the pane's alpha, and window depth tints with the
  premultiplied background. Every catalog effect passes alpha through, so the
  pane stays see-through.

## Frost

`frost.rs` grows hairlines from a seed: fronds fan in from each corner and sit
sparsely along each side, and side branches fork at 60 degrees, the axes of an
ice crystal. A line stops at `reach` from the nearest edge and at the terminal
area, which is the grid plus 2 logical px, so frost lives in the margin: the
padding and the band. Give it room with `window.padding`. Alpha fades from the
edge inward. The at most 2048 segments render once into a single-channel mask,
where the strongest line wins, so a crossing never brightens past `intensity`.
The mask is regrown only when the window, the grid or a key changes.

A pass after the clear lays the mask on the pane, shading only the four margin
strips, so everything else paints over it, and the resolve applies the rounded
shape. There is no clock and no frame demand: an idle window stays parked.

## The band

The generic half is in madori (`chrome.rs`): `ChromeLayout::hit_test`, the window
operations and `AppEvent::WindowState`. The terminal half is here: drawing in
`render.rs`, routing in `ux/engine.rs`, pure helpers in `window_chrome.rs`.

`InputEngine` asks the layout first, before the modal board and before the
terminal, so a drag or a resize never selects text.

| Region | Press | Release | Pointer |
|---|---|---|---|
| Edge or corner | resize | | resize, on its axis |
| Band | move; a second press within 400 ms toggles maximize; right press opens the compositor's window menu | | default |
| Button | arm | act, if still over it | default |
| Interior | the terminal | the terminal | the terminal's |

A press that began in the terminal keeps its release, wherever it lands.

The band stays off whenever something else decorates the window. winit's
`Window::is_decorated` is true on Wayland in xdg-decoration server mode, so
omoya's own bar turns it off with no config. Maximized keeps the band and drops
the edges, corners and rim. Fullscreen drops the band.

A title shows only if it has a letter or a digit and shapes with no missing
glyph, so the window's snowflake is never a stray mark in the band.

## Proof

`cargo test` holds the maths, the hit-testing, the routing and a parse and
validate of both shaders. `cargo test --features gpu_tests` adds pixel readback,
including the shader against the pure maths and every catalog effect on glass.
Neither sees a compositor. Pixels on a real seat are checked in a VM.
