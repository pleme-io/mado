//! M1 structural invariant (docs/REMEDIATION-PLAN.md §M1 acceptance):
//! BOTH event loops drive `ux::InputEngine` — neither `main.rs` nor
//! `gui_tear_attach.rs` contains direct UX logic. Before M1 the two
//! files carried two copies of selection / copy-paste / mouse /
//! search / kitty / URL / focus / IME / font-zoom handling; this test
//! makes re-introducing a third copy a CI failure.
//!
//! Grep-style by design, hardened (review 2026-06-11): BOTH scans run
//! over comment-stripped source — banned tokens can't hide behind a
//! `//` line and, symmetrically, a REQUIRED engine call can't be
//! satisfied by a doc comment after the real call was deleted.
//! Failures aggregate matrix-style — one run reports every violation.

static MAIN_RS: &str = include_str!("../src/main.rs");
static TEAR_RS: &str = include_str!("../src/gui_tear_attach.rs");

/// Drop `//`-style comment tails so neither scan direction can be
/// satisfied (or tripped) by prose. String literals containing `//`
/// (URLs) lose their tails too — acceptable: none of the scanned
/// tokens can FALSE-NEGATIVE from that, and a token hidden inside a
/// string literal is not executable UX logic anyway.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// UX logic that must live ONLY in `src/ux/` (and the shared helper
/// modules it consumes). The adapter loops may call the engine; they
/// may not reimplement it. Tokens carry their trailing `(` (or are
/// checked brace-insensitively below) so type names in adapter
/// signatures don't false-positive.
const BANNED: &[(&str, &str)] = &[
    ("selection.lock(", "selection mutation belongs to ux::InputEngine"),
    ("clipboard.lock(", "clipboard locking belongs to ux::InputEngine"),
    ("dir_picker.lock(", "dir-picker routing belongs to ux::InputEngine"),
    ("search.lock(", "search-overlay state belongs to ux::InputEngine"),
    ("extract_text(", "Selection::extract_text consumption is engine-only"),
    ("detect_urls(", "URL detection / click-open is engine-only"),
    ("detect_urls_in_row(", "URL detection is engine-only"),
    ("madori_key_to_pty_bytes(", "key→PTY translation is engine-only"),
    ("kitty_encode_key(", "kitty CSI-u encoding is engine-only"),
    ("sanitize_paste(", "PasteGuard consumption is engine-only"),
    ("append_query(", "search-overlay query editing is engine-only"),
    ("backspace_query(", "search-overlay query editing is engine-only"),
    ("BoundedFontSize::new(", "font zoom flows through the engine's FontZoomTarget path"),
    // M4 drain markers — per-loop side-effect polling is the seam the
    // unified Terminal::drain_side_effects() closed; any take_* call
    // in an adapter is a second drifting copy being reborn.
    ("take_bell(", "bell polling flows through Terminal::drain_side_effects"),
    ("take_clipboard(", "OSC 52 polling flows through Terminal::drain_side_effects"),
    ("drain_notifications(", "notification polling flows through Terminal::drain_side_effects"),
    ("take_progress(", "progress polling flows through Terminal::drain_side_effects"),
    (".title()", "title diffing flows through Terminal::drain_side_effects change-edges"),
];

/// Engine calls every adapter must make — the allow-listed seam. The
/// local loop additionally drives `on_resize` (the tear loop
/// deliberately ignores Resized and lets the reconciler converge on
/// rendered truth).
const REQUIRED_BOTH: &[&str] = &[
    "InputEngine::attach_to_renderer(",
    ".on_key(",
    ".on_mouse_button(",
    ".on_mouse_moved(",
    ".on_mouse_scroll(",
    ".on_ime_commit(",
    ".on_focus(",
    ".on_redraw_tick(",
    // M4: ONE drain + ONE shared consumer per adapter frame.
    ".drain_side_effects(",
    "apply_side_effects(",
    // M4 stage 2: ONE watched-config delta driver per adapter frame
    // (ux::ConfigHotReload) — a loop that stops polling silently
    // regresses to boot-time-only config.
    ".poll_config_reload(",
];

#[test]
fn neither_event_loop_contains_direct_ux_logic() {
    let mut failures: Vec<String> = Vec::new();
    for (name, raw) in [("src/main.rs", MAIN_RS), ("src/gui_tear_attach.rs", TEAR_RS)] {
        let src = strip_line_comments(raw);
        for (token, why) in BANNED {
            if src.contains(token) {
                failures.push(format!("{name} contains `{token}` — {why}"));
            }
        }
        // MouseReport construction — whitespace-insensitive (the
        // brace-spacing form `MouseReport {` vs `MouseReport{` must
        // not matter) and alias-resistant at the import site.
        let squashed: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
        if squashed.contains("MouseReport {") || squashed.contains("MouseReport{") {
            failures.push(format!(
                "{name} constructs MouseReport — mouse-report emission is engine-only"
            ));
        }
        if src.contains("mouse_report::") {
            failures.push(format!(
                "{name} reaches into ux::mouse_report — mouse-report emission is engine-only"
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} UX-logic leaks back into the adapter loops:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}

/// THE copy-on-release adapter law (operator report 2026-06-12): the
/// tear adapter must NEVER deliver a side-effect title by
/// short-circuiting the `match event` — that early `return
/// EventResponse{ set_title }` ate the `LeftRelease` that copies. The
/// drain's title is now a deferred side-channel folded onto the
/// event's own response via `with_title(`. This forcing function bans
/// the early-return shape (the drain must not be immediately followed
/// by a title-only return) AND requires the fold to be present, so the
/// regression cannot silently reappear.
#[test]
fn tear_adapter_drain_never_short_circuits_the_event() {
    let src = strip_line_comments(TEAR_RS);
    let squashed: String = src.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut failures: Vec<String> = Vec::new();

    // The deferred-fold must exist: the title rides onto the event's
    // response, never replaces it.
    if !squashed.contains("with_title(") {
        failures.push(
            "src/gui_tear_attach.rs no longer folds the drained title via with_title( — \
             the drain may have regressed to an early-return-on-title that drops the event"
                .into(),
        );
    }
    // The exact regression shape: the drain's `apply_side_effects(...)`
    // result returned directly as a title-only EventResponse. The fold
    // assigns it to `drained_title` instead; a `return EventResponse {
    // set_title: Some(title)` adjacent to the drain is the bug.
    if squashed.contains("apply_side_effects( effects, renderer,")
        && squashed.contains("return EventResponse { set_title: Some(title)")
    {
        failures.push(
            "src/gui_tear_attach.rs returns a title-only EventResponse — the drain is \
             dropping same-tick input events again (the copy-on-release regression)"
                .into(),
        );
    }
    assert!(
        failures.is_empty(),
        "{} copy-on-release adapter violations:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}

#[test]
fn both_call_sites_drive_the_engine() {
    let mut failures: Vec<String> = Vec::new();
    for (name, raw) in [("src/main.rs", MAIN_RS), ("src/gui_tear_attach.rs", TEAR_RS)] {
        let src = strip_line_comments(raw);
        for marker in REQUIRED_BOTH {
            if !src.contains(marker) {
                failures.push(format!("{name} does not drive `{marker}`"));
            }
        }
    }
    let main_src = strip_line_comments(MAIN_RS);
    let tear_src = strip_line_comments(TEAR_RS);
    // Local loop owns the Resized push; tear deliberately does not.
    if !main_src.contains(".on_resize(") {
        failures.push("src/main.rs does not drive `.on_resize(`".into());
    }
    // Both loops route injected/keybound actions through one
    // dispatch chokepoint (tear: the kanshou injection drain).
    if !tear_src.contains(".apply_action(") {
        failures.push("src/gui_tear_attach.rs does not drive `.apply_action(`".into());
    }
    assert!(
        failures.is_empty(),
        "{} missing engine seams:\n  - {}",
        failures.len(),
        failures.join("\n  - ")
    );
}
