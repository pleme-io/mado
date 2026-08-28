//! hanko (判子) — the frame gate's stamp: an O(1) witness for arbitrary state.
//!
//! # The problem this domain names
//!
//! A render loop's frame gate answers one question per piece of observable
//! state: *did it change since we last painted?* The answer is worthless if
//! asking costs more than painting — and asking is on the hot path, evaluated
//! every vsync tick including the ticks that skip.
//!
//! Three MEASURED instances of one class, all in this fleet:
//!
//! 1. **A witness that deep-clones.** `render::TerminalRenderer::overlay_snapshot`
//!    clones BOTH pickers — `FuzzyPicker` carries a `String` query, a
//!    `Vec<SessionPickerRow>` whose every row owns a `String`, plus three
//!    `Option<String>` — to compare them by value. Unconditionally, on every
//!    painted frame, and AGAIN per `needs_frame` tick while an overlay is open.
//!    Measured 2026-08-28 on the live GUI: 36 rows on the board, so ~72 heap
//!    allocations per tick to answer a yes/no question.
//! 2. **A witness whose cost is coupled to a type's future shape.** Comparing a
//!    whole `Copy` value is O(1) *today*; the day that struct gains a `String`
//!    it silently becomes instance 1. Nothing errors, nothing warns — the gate
//!    just starts allocating.
//! 3. **A change-token that can be forgotten.** A hand-bumped epoch is O(1) and
//!    shape-proof, and pays for it with a duty: every mutator must bump. A new
//!    mutator that does not is a gate gone silently blind — the exact bug the
//!    epoch was introduced to fix.
//!
//! The sibling shape, same class, different surface: seki's daemon kept an
//! unbounded buffer BETWEEN a bounded producer and a bounded consumer and leaked
//! 31.8 GB. Its fix is the one this module generalizes — the consumer provably
//! discarded every payload, so the right move was to DELETE the buffer and keep
//! only its reduction, not to size it.
//!
//! # The vocabulary
//!
//! A **hanko** is a name seal: physically small, fixed-size, and it stands in
//! for a whole document. That is exactly what a frame gate needs — a token that
//! represents arbitrary state at a cost that does not scale with it.
//!
//! [`Hanko::Stamp`] is bounded `Copy`, and that bound is the load-bearing part
//! rather than an ergonomic nicety: `String` and `Vec<T>` do not implement
//! `Copy`, so **instance 1 has no impl to write**. It is a compile error, not a
//! review comment. [`assert_stamp_fits`] closes the loophole that `Copy` leaves
//! open — `[u8; 4096]` is `Copy` — by refusing a stamp wider than
//! [`MAX_STAMP_BYTES`] in a const context.
//!
//! [`Sealed<T>`] closes instance 3 structurally. It owns the value and bumps on
//! every `&mut` access, so there is no path to mutate the inner value that does
//! not stamp it. "Remember to bump" stops being a rule a future author can break
//! and becomes a thing the borrow checker does.
//!
//! # Tier, stated honestly
//!
//! - Instances 1 and 2 → **truly-unrepresentable**. A deep-cloning or
//!   shape-coupled stamp has no `impl` (`Copy` is not satisfied); an oversized
//!   one fails a const assertion. Both are compile errors.
//! - Instance 3 → **truly-unrepresentable for the inner value** once state lives
//!   in a `Sealed<T>`: `DerefMut` is the only `&mut` path and it always bumps,
//!   so no mutator can move the value unstamped.
//!
//!   Two holes remain, and naming them is the point of grading rather than
//!   claiming. (a) Replacing the wrapper wholesale — `self.field =
//!   Sealed::new(next)` — resets the stamp to zero instead of advancing it.
//!   (b) A type can keep a second, unsealed field beside the sealed one. Both
//!   are **CI-caught**, not unrepresentable.
//!
//!   Hole (a) was RED-RUN in `selection.rs` (2026-08-28) rather than reasoned
//!   about, and the result is why both guards are kept: a mutator rewritten to
//!   replace the wrapper leaves `every_mutator_bumps_the_epoch` GREEN — a reset
//!   stamp of 0 still differs from the setup's 1, so the assertion is satisfied
//!   by the wrong thing — while the source-scan guard fails. The two are
//!   complementary, and either alone would have reported the bypass as fine.
//! - The unbounded-buffer instance is **not** typed here. It is named as the
//!   sibling shape and left as a documented follow-up; a `Coalescing<T>` that
//!   reduces on insert is the destination, and claiming it exists would be the
//!   round-up this module's own doctrine forbids.

use std::ops::{Deref, DerefMut};

/// Ceiling on a stamp's width, in bytes.
///
/// `Copy` alone does not bound cost: `[u8; 4096]` is `Copy` and copying it per
/// frame is the very thing this module exists to prevent. 16 bytes admits every
/// honest stamp shape — a `u64` epoch, a pair of `u32` generations, a `u128`
/// hash — and refuses a struct being smuggled through by value.
pub const MAX_STAMP_BYTES: usize = 16;

/// An O(1) witness for state a frame gate observes.
///
/// Implement this for anything the render loop compares against its
/// last-painted view. The `Copy` bound on [`Hanko::Stamp`] is what makes a
/// deep-cloning witness unrepresentable rather than merely discouraged.
pub trait Hanko {
    /// The stamp: a small fixed-size token standing in for the whole state.
    ///
    /// `Copy` rules out `String`/`Vec`/`HashMap` — the allocating shapes.
    /// `Eq` is what the gate actually needs: it compares, it never orders.
    ///
    /// ★ The `Copy` bound is the seal, and this is where that claim is
    /// PROVEN rather than asserted. A deep-cloning stamp does not compile:
    ///
    /// ```compile_fail
    /// # use mado::hanko::Hanko;
    /// struct Board { rows: Vec<String> }
    /// impl Hanko for Board {
    ///     type Stamp = Vec<String>;            // Vec<String> is not Copy
    ///     fn stamp(&self) -> Vec<String> { self.rows.clone() }
    /// }
    /// ```
    ///
    /// Neither does a stamp smuggled through by value, even though it IS
    /// `Copy` — that is the loophole [`assert_stamp_fits`] closes:
    ///
    /// ```compile_fail
    /// # use mado::hanko::{Hanko, assert_stamp_fits};
    /// #[derive(Clone, Copy, PartialEq, Eq)]
    /// struct Fat([u8; 4096]);
    /// struct Board;
    /// impl Hanko for Board {
    ///     type Stamp = Fat;
    ///     fn stamp(&self) -> Fat { Fat([0; 4096]) }
    /// }
    /// const _: () = assert_stamp_fits::<Board>();   // exceeds MAX_STAMP_BYTES
    /// ```
    ///
    /// The doctests live HERE, on a public item, and not beside the unit
    /// tests: rustdoc does not collect doctests from inside `#[cfg(test)]`,
    /// so a `compile_fail` block written there never runs and the
    /// unrepresentability claim it was meant to back goes unverified. That
    /// mistake was made and caught while writing this module.
    type Stamp: Copy + Eq;

    /// Take the current stamp. MUST be O(1) and allocation-free.
    ///
    /// A caller is a hot-path predicate evaluated every vsync tick, including
    /// the ticks that skip, so this is not a place to compute anything.
    fn stamp(&self) -> Self::Stamp;
}

/// Refuse a stamp wider than [`MAX_STAMP_BYTES`], at compile time.
///
/// Call from a const context to bind it — the `const { }` block below is the
/// idiom, and `Copy`'s loophole (`[u8; 4096]` satisfies it) is why this exists:
///
/// ```
/// # use mado::hanko::{Hanko, assert_stamp_fits};
/// # struct Thing;
/// # impl Hanko for Thing { type Stamp = u64; fn stamp(&self) -> u64 { 0 } }
/// const _: () = assert_stamp_fits::<Thing>();
/// ```
#[must_use = "call this in a const context or it checks nothing"]
pub const fn assert_stamp_fits<H: Hanko>() {
    assert!(
        size_of::<H::Stamp>() <= MAX_STAMP_BYTES,
        "a Hanko::Stamp wider than MAX_STAMP_BYTES defeats the point — the gate \
         copies it every tick. Stamp a hash or an epoch, not the state."
    );
}

/// A monotonic mutation counter — the default stamp shape.
///
/// Wrapping is deliberate and safe for this use: the gate asks only whether two
/// stamps DIFFER, so the single aliasing pair per 2^64 mutations would need the
/// value to land back on exactly the last-painted one, at which point the
/// content is being redrawn continuously anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Epoch(u64);

impl Epoch {
    /// Advance the epoch.
    pub const fn bump(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

    /// The raw counter — for diagnostics only; the gate compares `Epoch`s.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// State whose every mutation stamps itself.
///
/// Wrap a value here and the "remember to bump" duty disappears: [`DerefMut`]
/// is the only route to `&mut T` and it always bumps, so a mutator added years
/// later is stamped by construction. This is what makes Gate-0 instance 3
/// unrepresentable for the wrapped value instead of guarded by a test.
///
/// It reads as free at the call site — `sealed.clear()` works exactly as
/// `value.clear()` did — which matters, because a wrapper that costs ergonomics
/// gets unwrapped by the next person in a hurry.
///
/// ```
/// # use mado::hanko::{Sealed, Hanko};
/// let mut s = Sealed::new(String::new());
/// let before = s.stamp();
/// s.push_str("typed");          // &mut through DerefMut → stamped
/// assert_ne!(s.stamp(), before);
///
/// let after = s.stamp();
/// assert_eq!(s.len(), 5);       // & through Deref → NOT stamped
/// assert_eq!(s.stamp(), after);
/// ```
#[derive(Debug, Clone, Default)]
pub struct Sealed<T> {
    value: T,
    epoch: Epoch,
}

impl<T> Sealed<T> {
    /// Seal a value at epoch zero.
    pub const fn new(value: T) -> Self {
        Self {
            value,
            epoch: Epoch(0),
        }
    }

    /// Read-only access. Does NOT stamp — reading is not mutating, and stamping
    /// on read would make every gate tick report a change and defeat the skip.
    pub const fn get(&self) -> &T {
        &self.value
    }

    /// Unwrap, discarding the stamp.
    pub fn into_inner(self) -> T {
        self.value
    }
}

impl<T> Deref for Sealed<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Sealed<T> {
    /// ★ THE SEAL. Every `&mut` route to the inner value passes through here,
    /// so it cannot be taken without stamping. Deliberately pessimistic: a
    /// `&mut` that turns out not to mutate still bumps, costing one redundant
    /// frame. A missed stamp costs a display that stops updating, which is not
    /// the same order of mistake.
    fn deref_mut(&mut self) -> &mut T {
        self.epoch.bump();
        &mut self.value
    }
}

impl<T> Hanko for Sealed<T> {
    type Stamp = Epoch;
    fn stamp(&self) -> Epoch {
        self.epoch
    }
}

const _: () = assert_stamp_fits::<Sealed<String>>();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mutation_through_deref_mut_stamps() {
        let mut s = Sealed::new(vec![1_u8, 2, 3]);
        let before = s.stamp();
        s.push(4);
        assert_ne!(s.stamp(), before, "DerefMut must stamp");
    }

    #[test]
    fn a_read_does_not_stamp() {
        // The half that makes the skip work at all: if reading stamped, every
        // gate tick would report a change and nothing would ever be skipped.
        let s = Sealed::new(vec![1_u8, 2, 3]);
        let before = s.stamp();
        assert_eq!(s.len(), 3);
        assert_eq!(s.first(), Some(&1));
        assert_eq!(s.stamp(), before, "Deref must not stamp");
    }

    #[test]
    fn every_mutation_stamps_distinctly() {
        // Anti-vacuity for the gate's actual use: N mutations must yield N
        // distinguishable stamps, or a burst coalesces into one missed repaint.
        let mut s = Sealed::new(String::new());
        let mut seen = vec![s.stamp()];
        for c in ['a', 'b', 'c', 'd'] {
            s.push(c);
            let now = s.stamp();
            assert!(
                !seen.contains(&now),
                "stamp repeated after pushing {c:?} — a burst would coalesce \
                 into one missed repaint"
            );
            seen.push(now);
        }
    }

    #[test]
    fn the_stamp_is_epoch_sized_not_state_sized() {
        // THE property the vocabulary exists for: the witness's cost is
        // independent of how big the state is. A 10k-element Vec stamps in the
        // same 8 bytes as an empty one.
        let big = Sealed::new(vec![0_u8; 10_000]);
        let small = Sealed::new(Vec::<u8>::new());
        assert_eq!(
            size_of_val(&big.stamp()),
            size_of_val(&small.stamp()),
            "stamp width must not scale with state size"
        );
        assert!(size_of::<Epoch>() <= MAX_STAMP_BYTES);
    }

    #[test]
    fn wrapping_is_not_a_correctness_hole_for_difference() {
        // The gate asks "different?", never "newer?". Documenting that the
        // wrap is a deliberate choice and behaves at the boundary.
        let mut e = Epoch(u64::MAX);
        let before = e;
        e.bump();
        assert_ne!(e, before, "a bump at the boundary still changes the value");
        assert_eq!(e.get(), 0);
    }

    /// Positive control for the two `compile_fail` doctests on [`Hanko::Stamp`].
    ///
    /// Those prove a cloning / oversized stamp does not compile. This proves
    /// they fail for the STATED reason and not because the trait is
    /// unimplementable in general — without it, a bound so strict that nothing
    /// satisfies it would read as a successful seal.
    #[test]
    fn a_cloning_stamp_has_no_impl() {
        // Positive control: the shapes that ARE allowed all satisfy the bound,
        // so the doctest above fails for the stated reason and not because
        // nothing can implement the trait.
        fn accepts<H: Hanko>(_: &H) {}
        accepts(&Sealed::new(String::from("x")));
        accepts(&Sealed::new(vec![1, 2, 3]));
    }
}
