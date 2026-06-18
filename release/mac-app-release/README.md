# mac-app-release

The **shared, app-agnostic core** for shipping a macOS app the easy way:
assemble a signed, icon-bearing, drag-installable `.app` + DMG with a
sortable-exact / moving-latest release tag — all typed, **no shell-string
composition** (every unavoidable Mac-native tool — `codesign` / `hdiutil` /
`ditto` / `iconutil` — runs through the typed `cmd::Tool` builder).

This crate is the **extraction** of the reusable pieces that were duplicated
between two repos:

- `pleme-io/gaveta-client` → `crates/gaveta-release` (a menu-bar app + a
  3-binary/daemon bundle layout + an armázem-S3 publish)
- `pleme-io/mado` → `release/mado-release` (a windowed terminal + a
  single-binary layout + a GitHub-Releases publish)

Per the pleme-io ★ PRIME DIRECTIVE ("duplication is a bug; extract the shared
library"), the ~6 modules both tools shared now live here ONCE.

## What's in here (shared)

| Module | Responsibility |
|---|---|
| `cmd::Tool` | the typed `std::process::Command` builder (the sanctioned subprocess exception) |
| `spec::AppSpec` | the typed config a consumer drives the whole pipeline with |
| `plist_model::InfoPlist` | typed `Info.plist` → XML via the `plist` crate (TYPED EMISSION) |
| `icon` | SVG → 10-slot iconset → `.icns` (pure-Rust `resvg` + `iconutil`), or copy a pre-built `.icns` |
| `assemble` | assemble `.app` (pure-fs layout) → ad-hoc codesign → DMG + zip |
| `tag::ReleaseTags` | the sortable-exact (`…-r<n>-<sha>`) + moving-latest autobump tags |
| `git` | short-sha + toplevel helpers |
| `error::ReleaseError` | the typed error surface for every step above |

The **windowed-vs-menu-bar** difference is exactly **one typed field** —
`AppSpec.ls_ui_element` (`None` ⇒ a normal windowed app with a Dock icon, the
default; `Some(true)` ⇒ a menu-bar/agent app). It is never a forked code path.

## What's NOT in here (the consumer owns)

- **How the binary is built** — `cargo build` (mado) vs `swift build` + cargo
  (gaveta) vs a Nix derivation. The consumer hands `assemble::build_dmg` an
  already-built primary binary path.
- **The publish backend** — mado publishes to **GitHub Releases** (via the `gh`
  CLI); gaveta publishes to **armázem S3** (+ a Sparkle appcast). Each consumer
  keeps its own `publish` module + its own publish error type. This crate has
  no opinion on where bytes go.

## Using it (a consumer)

```rust
use mac_app_release::{AppSpec, Layout, assemble};

let spec = AppSpec::new("Mado", "io.pleme.mado", "mado")
    .version("0.1.9")
    .icon_svg("assets/mado-icon.svg");   // windowed: no .ls_ui_element(true)

let layout = Layout::under(repo_root);
let artifacts = assemble::build_dmg(&spec, &layout, &built_mado_binary)?;
// artifacts.dmg / .zip / .app — then hand them to YOUR publish backend.
```

A menu-bar app is the same call plus `.ls_ui_element(true)` and any
`.extra_binary(src, "Resources/bin/<daemon>")` placements.

## How `gaveta-release` adopts this (the other repo)

`gaveta-release` collapses to a thin consumer once it depends on this crate:

1. **Delete** its `cmd.rs`, `error.rs`, `layout.rs`, `icon.rs`, the `.app`
   assembly + codesign + DMG/zip parts of `assemble.rs`, and the `UploadKeys`
   tag-computation in `publish.rs` — all superseded by this crate.
2. **Keep** its armázem-S3 upload + Sparkle `appcast.rs`/`sparkle.rs` modules
   (the gaveta-specific publish backend), and replace its `plist_model.rs` +
   bundle assembly with:
   ```rust
   let spec = mac_app_release::AppSpec::new("Gaveta", "io.pleme.gaveta", "GavetaApp")
       .ls_ui_element(true)                                   // menu-bar app
       .icon_svg("assets/gaveta-icon.svg")
       .extra_binary(client_bin, "MacOS/gaveta-client")       // CLI seam
       .extra_binary(syncd_bin, "Resources/bin/gaveta-syncd"); // daemon
   let arts = mac_app_release::assemble::build_dmg(&spec, &layout, &app_bin)?;
   // then gaveta's existing armázem publish over arts.dmg
   ```
3. Add the Sparkle `SU*` plist keys via `AppSpec`'s extra-plist surface (a
   follow-up: `mac-app-release` can grow an `extra_plist: BTreeMap<String,
   PlistValue>` field the same way substrate's `mkDarwinAppBundle` has
   `extraPlist`, so the Sparkle keys stay typed). Until then gaveta keeps its
   own `InfoPlist` for the Sparkle keys and uses this crate for everything else.

### Where this crate lives + the publish path

Today it lives in **mado's repo** (`pleme-io/mado/release/mac-app-release`),
in an isolated Cargo workspace under `release/` so it does NOT enter mado's
substrate-built root crate. This is the "simplest for now" home the directive
allowed. The clean long-term home is its own small repo
(`pleme-io/mac-app-release`) published to crates.io via substrate's
`rust-library.nix` + AUTO-RELEASE, so gaveta can `mac-app-release = "0.1"`
instead of a cross-repo path dep. Promotion is a copy of `release/mac-app-release/`
into a new repo + a `cargo-auto-release` shim — no code change.

**Until that repo exists,** gaveta can either `path = "../mado/release/mac-app-release"`
(if the repos are checked out as siblings, matching gaveta-client's existing
cross-repo path-dep pattern) or vendor a copy with a TODO pointing here. The
real extraction (this crate) is done; only its *distribution home* is the open
follow-up.
