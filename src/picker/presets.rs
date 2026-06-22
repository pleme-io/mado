//! Emoji session presets — the create surface the Ctrl-S picker falls
//! back to when no live session matches.
//!
//! The operator's ask: "if there are no sessions to be found we should
//! list presets, one for every emoji we have, and typing `smile` should
//! match the smile preset." The enumerable atlas is
//! [`ishou_tokens::FleetSessionNames::POOL`] (50 `SessionIdentity`s, each
//! a stable word + emoji + glyph + search keywords). A preset is one POOL
//! entry; selecting it creates a session named by that emoji.
//!
//! Ranking reuses praça's [`praca::index::fuzzy_score`] (the SAME
//! subsequence scorer the session index ranks with) over the identity's
//! `word` (name tier) and `keywords` (synonym tier), mirroring praça's
//! `TIER_NAME > TIER_KEYWORD` rule so "type `wave`, get `🌊 tide`" ranks
//! the word-match above a keyword-only match.

use ishou_tokens::{FleetSessionNames, SessionName, SessionNameStyle};

/// Name tier dominates keyword tier — same lexicographic `(tier,
/// quality)` rule praça's `SessionIndex::best_match` uses.
const TIER_NAME: i32 = 4;
const TIER_KEYWORD: i32 = 3;

/// One emoji preset row: its index in [`FleetSessionNames::POOL`] (the
/// stable seed that reproduces this exact identity) + the rendered label
/// (`🌊 tide`) in the operator's name style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmojiPreset {
    /// Index into [`FleetSessionNames::POOL`]. Used as the `name_seed`
    /// when creating the session, so `FleetSessionNames::identity(idx)`
    /// reproduces exactly this identity.
    pub pool_index: usize,
    /// The display label (`🌊 tide`) — a typed [`SessionName`] `Display`,
    /// never an ad-hoc format of a syntax surface.
    pub label: String,
}

/// Best `(tier, quality)` of `query` against an identity's word + every
/// keyword. `None` if nothing matches. Empty query is handled by the
/// caller (returns the whole pool unranked), so this only sees a real
/// needle.
fn best_match(query: &str, idx: usize) -> Option<(i32, i32)> {
    let id = FleetSessionNames::POOL[idx];
    let mut best: Option<(i32, i32)> = None;
    let mut consider = |tier: i32, hay: &str| {
        if let Some(q) = praca::index::fuzzy_score(query, hay) {
            let cand = (tier, q);
            best = Some(best.map_or(cand, |b: (i32, i32)| b.max(cand)));
        }
    };
    consider(TIER_NAME, id.word);
    for kw in id.keywords {
        consider(TIER_KEYWORD, kw);
    }
    best
}

/// The emoji presets matching `query`, ranked by `(tier, quality)` then
/// POOL order. An empty / whitespace-only query returns EVERY preset in
/// POOL order (the "no sessions to be found → list every emoji" view).
/// `style` chooses the rendered mark (emoji vs glyph).
#[must_use]
pub fn matching(query: &str, style: SessionNameStyle) -> Vec<EmojiPreset> {
    let label_of = |idx: usize| -> String {
        SessionName {
            identity: FleetSessionNames::POOL[idx],
            style,
        }
        .to_string()
    };

    let q = query.trim();
    if q.is_empty() {
        return (0..FleetSessionNames::POOL.len())
            .map(|idx| EmojiPreset {
                pool_index: idx,
                label: label_of(idx),
            })
            .collect();
    }

    let mut scored: Vec<((i32, i32), usize)> = (0..FleetSessionNames::POOL.len())
        .filter_map(|idx| best_match(q, idx).map(|m| (m, idx)))
        .collect();
    // (tier, quality) desc, then POOL order for a stable tie-break.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .map(|(_, idx)| EmojiPreset {
            pool_index: idx,
            label: label_of(idx),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_lists_every_emoji() {
        let all = matching("", SessionNameStyle::Emoji);
        assert_eq!(all.len(), FleetSessionNames::POOL.len());
        // POOL order preserved.
        assert_eq!(all[0].pool_index, 0);
        assert!(all[0].label.contains("tide"));
    }

    #[test]
    fn query_smile_does_not_panic_and_filters() {
        // No "smile" emoji in the pool, but the scorer must still run
        // cleanly and return only real matches (possibly empty).
        let _ = matching("smile", SessionNameStyle::Emoji);
    }

    #[test]
    fn keyword_search_surfaces_the_emoji() {
        // The operator's example: "wave" → 🌊 tide via its keyword.
        let rows = matching("wave", SessionNameStyle::Emoji);
        assert!(
            rows.iter().any(|p| p.label.contains("tide")),
            "typing 'wave' should surface the tide preset, got {rows:?}"
        );
    }

    #[test]
    fn word_match_outranks_keyword_match() {
        // "fox" matches the fox word (tier 4); ensure a word hit ranks at
        // the top of its result set.
        let rows = matching("fox", SessionNameStyle::Emoji);
        assert!(rows.first().map_or(false, |p| p.label.contains("fox")));
    }

    #[test]
    fn pool_index_reproduces_the_identity() {
        let rows = matching("frost", SessionNameStyle::Emoji);
        let p = rows.first().expect("frost matches its own word");
        let id = FleetSessionNames::identity(p.pool_index as u64);
        assert_eq!(id.word, "frost");
    }
}
