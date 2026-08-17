//! Typed board-row composition — the visual grammar of the Ctrl-S board.
//!
//! This replaces a `push_str` pipeline that built each row as one opaque
//! `String`. Opaque strings cannot be ALIGNED, and alignment is the whole
//! point: a row is a record with fields, and the fields want to be columns.
//!
//! ── Why these choices, and not others ────────────────────────────────
//!
//! **Position is the channel we were not using.** Cleveland & McGill ranked
//! the elementary perceptual tasks by accuracy and put *position along a
//! common scale* at the top — ahead of length, angle, area and colour. The
//! board had colour (urgency tint), shape (○/◐) and text, but every field
//! began wherever the previous one happened to end, so there was no common
//! scale to judge against. Padding the body to one width converts "read
//! each row to find its age" into "run your eye down a column", which is
//! the single largest perceptual win available here and costs no glyphs at
//! all.
//!
//! **Erase redundant data-ink (Tufte).** The list is ALREADY SORTED by
//! rank. Position in the list therefore encodes severity — so stamping
//! `P2` / `critical` on every row re-encodes, in 2–8 characters of text,
//! what the ordering already says for free. [`Tier`] keeps a single
//! preattentive glyph for the top two tiers (worth it when the board is
//! filtered or scrolled and the ordering is no longer visible from where
//! you are looking) and prints NOTHING below that.
//!
//! **Keep the label with the icon.** The tempting compression — drop the
//! service word, since the row already opens with a per-source emoji — is
//! NOT supported. The icon literature is consistent that icon+label beats
//! icon-alone for anything but a small, over-learned set, and mado has ~25
//! source emoji. So `datadog:acme` keeps its word; it earns its width
//! by becoming a column instead of getting shorter.
//!
//! **One vocabulary across vendors.** Grafana says `critical`, Datadog
//! says `P1`, Jira says `Highest`. Those are three spellings of one idea,
//! and rendering each vendor's dialect makes the operator translate per
//! row. [`Tier`] is the single ladder they all land on.

use unicode_width::UnicodeWidthStr;

/// Severity, as ONE ladder every vendor's dialect maps onto.
///
/// Only the top two tiers render. Below them the ordering already says
/// everything the glyph would, and a mark on every row is a mark that
/// distinguishes nothing — the "erase redundant data-ink" rule applied to
/// a column rather than a chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// On fire now.
    Critical,
    /// Wants attention soon.
    High,
    /// Everything calmer than High.
    Calm,
}

impl Tier {
    /// Project the typed urgency the store already ranked by. The board
    /// never re-derives severity from a vendor string — the vendor string
    /// was mapped to `Urgency` at the source border, so re-reading it here
    /// would be a second, driftable copy of one decision.
    #[must_use]
    pub fn of(urgency: crate::suggest::Urgency) -> Self {
        match urgency {
            crate::suggest::Urgency::Critical => Tier::Critical,
            crate::suggest::Urgency::High => Tier::High,
            _ => Tier::Calm,
        }
    }

    /// The glyph, or `""` for [`Tier::Calm`].
    ///
    /// Filled vs hollow triangle is a SHAPE contrast, readable without
    /// colour — the row is already tinted by urgency, and encoding one
    /// variable twice (colour + shape) is the one redundancy worth paying
    /// for, because it is what keeps the board legible to a colour-blind
    /// reader and in a washed-out theme.
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            Tier::Critical => "\u{25b2}", // ▲ filled
            Tier::High => "\u{25b3}",     // △ hollow
            Tier::Calm => "",
        }
    }
}

/// Whether the row is merely waiting, or actively being worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Waiting for you (○).
    Latent,
    /// A session is open on it (◐).
    Working,
}

impl State {
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self {
            State::Latent => "\u{25cb}",  // ○
            State::Working => "\u{25d0}", // ◐
        }
    }
}

/// One board row as TYPED COLUMNS, not a pre-concatenated string.
#[derive(Debug, Clone)]
pub struct BoardRow {
    pub state: State,
    /// Source emoji + title — the identifying text, which must be read.
    pub body: String,
    /// `service:account`, when the row came from an external tenant.
    pub origin: Option<String>,
    pub tier: Tier,
    /// Source-specific context (repo, sprint, breaching value). Kept as its
    /// own column rather than folded into `body`: it is the field most
    /// often absent, and a column that is sometimes empty still aligns the
    /// ones that are not.
    pub detail: Option<String>,
    /// Times re-observed; `<2` prints nothing (a first sighting is the
    /// norm, and marking the norm marks nothing).
    pub repeats: u32,
    /// Pre-formatted age (`3w`), or `None` while the row is still fresh.
    pub age: Option<String>,
}

/// Widest body column we will pad to.
///
/// Without a clamp a single long title drags every other row's columns
/// off to the right — one outlier taxing the whole board. Past this the
/// outlier stays ragged and everything else keeps its alignment: the
/// column serves the many, not the longest.
const MAX_BODY_WIDTH: usize = 56;

/// Render rows with their trailing fields aligned into columns.
///
/// Padding is by DISPLAY WIDTH, not `len()` or `chars().count()` — every
/// row opens with an emoji, and emoji are double-width. Counting chars
/// puts each row's columns one cell left of the last, which produces a
/// ragged edge that looks like a bug in the alignment rather than what it
/// is: the wrong unit.
#[must_use]
pub fn render_aligned(rows: &[BoardRow]) -> Vec<String> {
    let width = rows
        .iter()
        .map(|r| UnicodeWidthStr::width(r.body.as_str()))
        .max()
        .unwrap_or(0)
        .min(MAX_BODY_WIDTH);
    rows.iter().map(|r| render_one(r, width)).collect()
}

fn render_one(row: &BoardRow, body_width: usize) -> String {
    let mut out = String::with_capacity(body_width + 32);
    out.push_str(row.state.glyph());
    out.push(' ');
    out.push_str(&row.body);

    // Anything to the right of the body? If not, stop — trailing padding
    // is invisible ink that still costs bytes and can wrap a narrow pane.
    let has_meta = row.origin.is_some()
        || row.tier != Tier::Calm
        || row.detail.is_some()
        || row.repeats >= 2
        || row.age.is_some();
    if !has_meta {
        return out;
    }

    let used = UnicodeWidthStr::width(row.body.as_str());
    // The column is `body_width + 1`, NOT `body_width`: the widest row
    // still needs its one separating space, and if that space is added
    // *on top of* a pad computed to `body_width`, the widest row alone
    // lands one cell right of every other — an off-by-one that reads as
    // "alignment is broken" precisely on the row you notice most. Pad to
    // the shared column; `.max(1)` then only bites for a body past the
    // clamp, which is the documented ragged case.
    let pad = (body_width + 1).saturating_sub(used).max(1);
    for _ in 0..pad {
        out.push(' ');
    }

    let mut sep = false;
    let mut field = |out: &mut String, s: &str, sep: &mut bool| {
        if *sep {
            out.push(' ');
        }
        out.push_str(s);
        *sep = true;
    };
    if let Some(o) = &row.origin {
        field(&mut out, o, &mut sep);
    }
    let t = row.tier.glyph();
    if !t.is_empty() {
        field(&mut out, t, &mut sep);
    }
    if let Some(d) = &row.detail {
        field(&mut out, d, &mut sep);
    }
    if row.repeats >= 2 {
        let mut r = String::from("\u{d7}"); // ×
        r.push_str(&row.repeats.to_string());
        field(&mut out, &r, &mut sep);
    }
    if let Some(a) = &row.age {
        field(&mut out, a, &mut sep);
    }
    out
}

// ── Lane health ──────────────────────────────────────────────────────

/// Why a source lane is not reporting, as one glyph.
///
/// The footer used to spell this: `13 lanes blind (never once succeeded):
/// tend-repos timed out · cargo-warnings needs config …` — 30 characters
/// of grammar before the first fact, repeated every frame. The glyph rides
/// ON the lane name, so the name and its ailment are one visual chunk
/// (Gestalt proximity) instead of two words the reader must pair up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ailment(pub crate::suggest::SourceStatus);

impl Ailment {
    /// One glyph per failure mode. Chosen to be guessable rather than
    /// clever: a clock for slow, a key for locked out, a wrench for
    /// unconfigured, a cross for broken.
    #[must_use]
    pub fn glyph(self) -> &'static str {
        match self.0 {
            crate::suggest::SourceStatus::TimedOut => "\u{23f1}", // ⏱
            crate::suggest::SourceStatus::AuthMissing => "\u{1f511}", // 🔑
            crate::suggest::SourceStatus::Unconfigured => "\u{1f527}", // 🔧
            crate::suggest::SourceStatus::Error => "\u{2717}",    // ✗
            // A healthy lane is never in this footer; render nothing
            // rather than inventing a mark for it.
            crate::suggest::SourceStatus::Ok => "",
        }
    }
}

/// Compose the lane-health footer.
///
/// `blind` lanes have NEVER succeeded — each is a standing declaration to
/// go fix, so each is named. `degraded` lanes have worked before and are
/// expected to work again, so they collapse to a count: naming them every
/// frame would crowd out the names that actually mean something.
///
/// Returns `None` when there is nothing to say, so a healthy board spends
/// zero rows on its own health.
#[must_use]
pub fn health_line(
    blind: &[(&'static str, crate::suggest::SourceStatus)],
    degraded: usize,
) -> Option<String> {
    if blind.is_empty() && degraded == 0 {
        return None;
    }
    const NAMED: usize = 3;
    let mut line = String::from("\u{26a0} "); // ⚠
    for (i, (slug, status)) in blind.iter().take(NAMED).enumerate() {
        if i > 0 {
            line.push(' ');
        }
        line.push_str(slug);
        line.push_str(Ailment(*status).glyph());
    }
    if blind.len() > NAMED {
        if !blind.is_empty() {
            line.push(' ');
        }
        line.push('+');
        line.push_str(&(blind.len() - NAMED).to_string());
    }
    if degraded > 0 {
        if !blind.is_empty() {
            line.push_str("  ");
        }
        // ◑ = works sometimes. Distinct from ◐ (a row being worked on):
        // different half, different meaning, and they never co-occur in
        // one line.
        line.push_str(&degraded.to_string());
        line.push('\u{25d1}');
    }
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::{SourceStatus, Urgency};

    fn row(body: &str, origin: Option<&str>) -> BoardRow {
        BoardRow {
            state: State::Latent,
            body: body.to_string(),
            origin: origin.map(ToString::to_string),
            tier: Tier::Calm,
            detail: None,
            repeats: 0,
            age: None,
        }
    }

    /// THE point of the type: the metadata of every row starts at the same
    /// column, so the eye reads down instead of hunting across.
    #[test]
    fn metadata_starts_at_one_column_for_every_row() {
        let rows = vec![
            row("short", Some("datadog:acme")),
            row("a much longer title here", Some("grafana:rio")),
            row("mid length", Some("jira:proj")),
        ];
        let out = render_aligned(&rows);
        let cols: Vec<usize> = out
            .iter()
            .map(|line| {
                let at = line.find(':').expect("each row has an origin");
                // Width of everything before the origin token.
                let head = &line[..at];
                let start = head.rfind(' ').unwrap();
                UnicodeWidthStr::width(&line[..=start])
            })
            .collect();
        assert!(
            cols.windows(2).all(|w| w[0] == w[1]),
            "origins must share a column, got {cols:?} in {out:#?}"
        );
    }

    /// Emoji are DOUBLE-WIDTH. Padding by `chars().count()` puts each
    /// emoji row one cell left of the others — the bug that makes an
    /// aligned column look broken. Pinned with real source emoji.
    #[test]
    fn alignment_is_by_display_width_not_char_count() {
        let rows = vec![
            row("\u{1f415} datadog thing", Some("datadog:acme")), // 🐕
            row("plain ascii thing", Some("grafana:rio")),
        ];
        let out = render_aligned(&rows);
        let starts: Vec<usize> = out
            .iter()
            .map(|l| UnicodeWidthStr::width(&l[..l.find(':').unwrap()]))
            .collect();
        assert_eq!(
            starts[0], starts[1],
            "emoji row must not drift; a char-count pad would break here: {out:#?}"
        );
    }

    /// A single monster title must not drag every other row's columns
    /// rightward — the clamp keeps the column serving the many.
    #[test]
    fn one_long_title_does_not_tax_the_whole_board() {
        let long = "x".repeat(200);
        let rows = vec![row(&long, Some("a:b")), row("short", Some("c:d"))];
        let out = render_aligned(&rows);
        let short_line = &out[1];
        assert!(
            UnicodeWidthStr::width(short_line.as_str()) < 80,
            "clamped, not dragged to 200: {short_line}"
        );
        // The outlier still renders, and still separates from its meta.
        assert!(out[0].contains(" a:b"), "{}", out[0]);
    }

    /// Calm rows print no tier glyph, and a first sighting prints no
    /// count: a mark carried by every row distinguishes nothing.
    #[test]
    fn only_the_exceptional_is_marked() {
        assert_eq!(Tier::of(Urgency::Normal).glyph(), "");
        assert_eq!(Tier::of(Urgency::Low).glyph(), "");
        assert_eq!(Tier::of(Urgency::Idle).glyph(), "");
        assert_eq!(Tier::of(Urgency::Critical).glyph(), "\u{25b2}");
        assert_eq!(Tier::of(Urgency::High).glyph(), "\u{25b3}");

        let mut r = row("t", None);
        r.repeats = 1;
        assert!(
            !render_aligned(&[r.clone()])[0].contains('\u{d7}'),
            "×1 is noise"
        );
        r.repeats = 6;
        assert!(
            render_aligned(&[r])[0].contains("\u{d7}6"),
            "a repeat offender wears its count"
        );
    }

    /// A row with nothing to its right gets no trailing padding — invisible
    /// ink that still costs bytes and can wrap a narrow pane.
    #[test]
    fn a_bare_row_carries_no_trailing_padding() {
        let rows = vec![row("a very long title indeed", None), row("x", None)];
        let out = render_aligned(&rows);
        assert_eq!(out[1], "\u{25cb} x", "no pad when there is no metadata");
        assert!(!out[0].ends_with(' '));
    }

    /// The footer fuses each lane to its ailment (one chunk), names only
    /// the actionable few, and counts the rest.
    #[test]
    fn health_line_is_symbolic_and_bounded() {
        assert_eq!(health_line(&[], 0), None, "a healthy board says nothing");

        let line = health_line(
            &[
                ("tend-repos", SourceStatus::TimedOut),
                ("cargo-warnings", SourceStatus::Unconfigured),
                ("flux-failing", SourceStatus::Error),
                ("k8s-unhealthy", SourceStatus::Error),
                ("aws-health", SourceStatus::AuthMissing),
            ],
            4,
        )
        .unwrap();

        assert!(line.contains("tend-repos\u{23f1}"), "{line}");
        assert!(line.contains("cargo-warnings\u{1f527}"), "{line}");
        assert!(line.contains("flux-failing\u{2717}"), "{line}");
        assert!(
            line.contains("+2"),
            "un-named blind lanes are counted: {line}"
        );
        assert!(
            line.contains("4\u{25d1}"),
            "degraded collapse to a count: {line}"
        );
        // The prose is gone.
        assert!(!line.contains("lanes"), "{line}");
        assert!(!line.contains("never once succeeded"), "{line}");
        assert!(!line.contains("degraded"), "{line}");
        // And it is dramatically shorter than the sentence it replaces.
        assert!(
            UnicodeWidthStr::width(line.as_str()) < 80,
            "footer must fit a pane: {line}"
        );
    }

    /// Degraded-only: no blind lanes means no stray separator.
    #[test]
    fn degraded_only_footer_has_no_leading_gap() {
        let line = health_line(&[], 3).unwrap();
        assert_eq!(line, "\u{26a0} 3\u{25d1}");
    }
}
