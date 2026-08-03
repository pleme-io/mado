//! L1b — the mado ⇄ tear ⇄ real-shell seam, driven as a matrix.
//!
//! `l1_engate_loop` proves the loop runs for one shell on one path. This
//! module widens that to the surface where the 2026-08-02 cursor-landing
//! hunt actually lived: an interactive shell's on-screen geometry is not
//! decided by any one component. frost and frostmourne keep no cursor
//! model — they ask `ESC[6n` and repaint at whatever row comes back — so
//! the picture the operator sees is a *negotiation* between the shell,
//! tear's PTY + `PaneGrid`, and mado's mirror `Terminal`. Every component
//! can be individually correct while the negotiation is wrong, which is
//! exactly what made that bug so slippery: seven separate unit-level
//! checks all passed.
//!
//! ## The seam invariant this module exists for
//!
//! **Two independent VT parsers consume the same byte stream.** tear-core
//! feeds `PaneGrid` (its own state machine, its own wrap/width rules) and
//! mado feeds `Terminal` through engate. Nothing in the type system makes
//! them agree, and only mado's answers the shell's queries. So a width or
//! wrap divergence between them does not surface as a rendering artifact —
//! it surfaces as the SHELL being told the wrong thing and painting its
//! next prompt in the wrong place. `assert_grids_agree` is therefore the
//! load-bearing row here, not a nicety.
//!
//! ## Shape
//!
//! Shells × interactions, every cell asserting the same typed invariant
//! set. [`Interaction`] derives its own `ALL`, so a new variant that no
//! one wired into `drive` is a non-exhaustive-match compile error rather
//! than a row silently missing from the matrix (★★ CLOSED-LOOP
//! MASS-SYNTHESIS).
//!
//! Local-only by construction, same gate as §L1: shells that are not
//! deployed on this machine SKIP with an eprintln, so CI runners without
//! frost/frostmourne stay green while cid-class workstations run the real
//! thing.

use std::sync::Arc;
use std::time::{Duration, Instant};

use engate_attach::Attach;
use parking_lot::RwLock;
use pleme_allvariants_derive::AllVariants;
use tear_core::InProcess;
use tear_core::engate_producer::PaneProducer;
use tear_types::{MultiplexerControl, PaneId, SessionSource};

use crate::engate_consumer::{ProbeCounters, ResponseWriter, TerminalSink};
use crate::terminal::Terminal;

/// Per-phase deadline. Generous vs the <100ms expectation so a cold shell
/// boot (rc load, git status on a large repo) never flakes a row.
const PHASE_DEADLINE: Duration = Duration::from_secs(5);

/// Poll `cond` every 20ms until it holds or `deadline` elapses.
fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cond()
}

/// A shell to run the matrix against.
///
/// Both are pleme-io shells and both are reedline-based, which is the
/// reason they belong here rather than in a mado-only test: reedline is
/// what makes the prompt geometry depend on a correct CPR answer. bash and
/// zsh track their own cursor and would pass the matrix even with mado's
/// answers scrambled — a green row against them would be worthless.
#[derive(Clone, Copy, Debug)]
struct ShellUnderTest {
    /// Binary name, resolved against the per-user Nix profile.
    bin: &'static str,
}

const SHELLS: &[ShellUnderTest] = &[
    ShellUnderTest { bin: "frostmourne" },
    ShellUnderTest { bin: "frost" },
];

impl ShellUnderTest {
    /// Absolute path to this shell, or `None` when it is not deployed
    /// here. `MADO_L1_SHELL` overrides the whole set with one binary so an
    /// operator can point the matrix at a work-in-progress build.
    fn resolve(self) -> Option<String> {
        if let Ok(sh) = std::env::var("MADO_L1_SHELL") {
            if !sh.trim().is_empty() {
                return Some(sh);
            }
        }
        let user = std::env::var("USER").ok()?;
        let candidate = format!("/etc/profiles/per-user/{user}/bin/{}", self.bin);
        std::path::Path::new(&candidate)
            .exists()
            .then_some(candidate)
    }
}

/// One interaction to drive a booted seam through.
///
/// `ALL` is derived, never hand-listed — adding a variant without wiring
/// it into `drive` fails to compile.
#[derive(Clone, Copy, Debug, AllVariants)]
enum Interaction {
    /// Press Enter on an empty line N times. The 2026-08-02 report: each
    /// cycle must consume exactly one row.
    EnterCycle,
    /// Type characters and erase them again. Exercises the repaint path
    /// where the shell rewrites the prompt line in place on every
    /// keystroke — the right-prompt margin probe rides along here.
    TypeAndErase,
    /// Run a command and read its output back out of the grid.
    CommandRoundTrip,
    /// Shrink the pane under the shell. Both grids resize and the shell
    /// gets SIGWINCH; the seam must survive it.
    Resize,
    /// Fill the screen so the prompt sits on the last row and the next
    /// Enter scrolls. CPR answers are relative to the visible grid, so
    /// scroll is where an off-by-one is easiest to introduce.
    ScrollAtBottom,
}

/// A booted mado ⇄ tear ⇄ shell seam: real `InProcess` PTY, real engate
/// `Attach`, real `TerminalSink` with the production-shaped
/// `ResponseWriter`. Exactly the wiring `gui_tear_attach` builds, minus
/// the renderer and window.
struct Seam {
    inproc: Arc<InProcess>,
    pane: PaneId,
    terminal: Arc<RwLock<Terminal>>,
    probes: Arc<ProbeCounters>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Seam {
    fn boot(shell: &str, cols: usize, rows: usize) -> Self {
        let inproc = Arc::new(InProcess::new());
        let sid = inproc
            .new_session_with_source_and_size(
                "mado-seam",
                shell,
                &[],
                SessionSource::Named("mado-seam".into()),
                (cols as u16, rows as u16),
            )
            .expect("spawn real shell session");
        let pane = inproc
            .with_registry(|r| {
                r.sessions
                    .get(&sid)
                    .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
            })
            .expect("session must have a pane");

        let terminal: Arc<RwLock<Terminal>> =
            Arc::new(RwLock::new(Terminal::with_scrollback(cols, rows, 10_000)));
        let writer_control = Arc::clone(&inproc);
        let response_writer: ResponseWriter = Arc::new(move |bytes: &[u8]| {
            if let Err(e) = writer_control.send_keys(pane, bytes) {
                eprintln!("seam: VT query response write-back FAILED: {e}");
            }
        });
        let probes = Arc::new(ProbeCounters::default());
        let consumer = TerminalSink::new_with_probes(
            Arc::clone(&terminal),
            response_writer,
            Arc::clone(&probes),
        );

        // NO startup sleep before attach: the consumer must be live before
        // the shell paints its first prompt, so the startup CPR flows
        // through the sink exactly as it does in production.
        let attach = Attach::builder()
            .producer(PaneProducer::new(Arc::clone(&inproc), pane))
            .consumer(consumer)
            .build();
        let (attach, history) = attach.subscribe().expect("engate.subscribe");
        let attach = attach.replay(history).expect("engate.replay");
        let attach_live = attach.start_live();
        let thread = std::thread::Builder::new()
            .name("mado-seam-live".into())
            .spawn(move || {
                let _consumer = attach_live.run();
            })
            .expect("spawn engate live thread");

        Self {
            inproc,
            pane,
            terminal,
            probes,
            thread: Some(thread),
        }
    }

    fn send(&self, bytes: &[u8]) {
        self.inproc.send_keys(self.pane, bytes).expect("send keys");
    }

    /// Resize both halves the way the GUI's redraw-tick reconciler does:
    /// tear's `PaneGrid` + PTY (which SIGWINCHes the shell) and mado's
    /// mirror `Terminal` (which owns wrap math and the CPR answer).
    fn resize(&self, cols: usize, rows: usize) {
        self.inproc
            .pane_resize_absolute(self.pane, cols as u16, rows as u16)
            .expect("resize pane");
        self.terminal.write().resize(cols, rows);
    }

    /// mado's mirror grid as trimmed text rows.
    fn mado_rows(&self) -> Vec<String> {
        let t = self.terminal.read();
        t.visible_rows()
            .map(|row| {
                let mut s = String::new();
                for cell in row {
                    cell.write_to(&mut s);
                }
                s.trim_end().to_owned()
            })
            .collect()
    }

    fn mado_cursor(&self) -> (usize, usize) {
        let t = self.terminal.read();
        let c = t.cursor();
        (c.row, c.col)
    }

    /// tear's own grid as trimmed text rows — the independent parser.
    fn tear_rows(&self) -> Vec<String> {
        self.inproc
            .pane_snapshot(self.pane)
            .expect("pane snapshot")
            .to_text_rows()
            .into_iter()
            .map(|r| r.trim_end().to_owned())
            .collect()
    }

    fn tear_cursor(&self) -> (usize, usize) {
        let s = self.inproc.pane_snapshot(self.pane).expect("pane snapshot");
        (s.cursor_row, s.cursor_col)
    }

    /// Non-blank rows, paired with their trimmed width.
    fn occupied_rows(&self) -> Vec<(usize, usize)> {
        self.mado_rows()
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
            .map(|(i, l)| (i, l.chars().count()))
            .collect()
    }

    fn dump(&self) -> String {
        let mado = self.mado_rows();
        let tear = self.tear_rows();
        let mut out = format!(
            "mado cursor={:?}  tear cursor={:?}\n",
            self.mado_cursor(),
            self.tear_cursor()
        );
        for (i, (m, t)) in mado.iter().zip(tear.iter()).enumerate() {
            let flag = if m == t { ' ' } else { '!' };
            out.push_str(&format!("{flag} r{i:>2} mado={m:?}\n"));
            if m != t {
                out.push_str(&format!("      tear={t:?}\n"));
            }
        }
        out
    }

    /// **The seam invariant.** mado's mirror and tear's own grid parsed
    /// the same bytes, so they must hold the same picture and the same
    /// caret. A divergence here is not cosmetic: mado's copy is the one
    /// that answers the shell's `ESC[6n`, so once these drift apart the
    /// shell is being steered by a grid nobody is rendering.
    fn assert_grids_agree(&self, ctx: &str) {
        assert!(
            wait_until(PHASE_DEADLINE, || {
                self.mado_rows() == self.tear_rows() && self.mado_cursor() == self.tear_cursor()
            }),
            "{ctx}: mado's mirror grid and tear's PaneGrid disagree\n{}",
            self.dump()
        );
    }

    /// Every answered VT query made it back out through the writer. A
    /// persistent gap is the shape that killed reedline shells outright.
    fn assert_query_loop_closed(&self, ctx: &str) {
        assert!(
            self.probes.queries_seen() >= 1,
            "{ctx}: shell never issued an answered VT query — \
             the loop is dead before any real assertion"
        );
        assert_eq!(
            self.probes.responses_written(),
            self.probes.queries_seen(),
            "{ctx}: query↔answer gap — answers generated, then dropped"
        );
    }

    /// Settle the boot prompt, then `clear` so row 0 is a known origin.
    ///
    /// Waits for the prompt to be ON SCREEN, not merely for the first CPR
    /// to be answered. The first query fires while reedline is still
    /// coming up, so keys sent on that signal alone can land in a
    /// half-initialised line editor and vanish — a ~1-in-6 flake on a
    /// loaded machine, and exactly the kind of "process alive ≠ shell
    /// interactive" trap §L1 warns about.
    fn settle_to_clean_prompt(&self, ctx: &str) {
        assert!(
            wait_until(PHASE_DEADLINE, || self.probes.queries_seen() >= 1),
            "{ctx}: startup CPR never answered"
        );
        assert!(
            wait_until(PHASE_DEADLINE, || !self.occupied_rows().is_empty()),
            "{ctx}: shell never painted a boot prompt\n{}",
            self.dump()
        );
        self.send(b"clear\r");
        assert!(
            wait_until(PHASE_DEADLINE, || {
                let rows = self.occupied_rows();
                rows.len() == 1 && rows[0].0 == 0
            }),
            "{ctx}: `clear` never left a single prompt on row 0\n{}",
            self.dump()
        );
    }
}

impl Drop for Seam {
    fn drop(&mut self) {
        // Mirrors production embedded teardown: never `kill_session` (that
        // deadlocks — InProcess holds its registry lock while PtyHandle's
        // Drop blocks on Child::wait, and the pty-reader thread wants the
        // same lock). Ask the shell to exit; its EOF closes the engate
        // channel and `run()` returns.
        let _ = self.inproc.send_keys(self.pane, b"exit\r");
        if let Some(t) = self.thread.take() {
            let _ = wait_until(PHASE_DEADLINE, || t.is_finished());
        }
    }
}

/// Drive one interaction and assert what is specific to it. The shared
/// invariants (`assert_grids_agree`, `assert_query_loop_closed`) are
/// applied by the caller around every row.
fn drive(seam: &Seam, interaction: Interaction, ctx: &str) {
    const COLS: usize = 100;
    const ROWS: usize = 20;

    match interaction {
        Interaction::EnterCycle => {
            seam.settle_to_clean_prompt(ctx);
            const ENTERS: usize = 3;
            for n in 1..=ENTERS {
                seam.send(b"\r");
                assert!(
                    wait_until(PHASE_DEADLINE, || seam.occupied_rows().len() == n + 1),
                    "{ctx}: after Enter #{n} the screen does not hold {} prompts\n{}",
                    n + 1,
                    seam.dump()
                );
            }
            let occupied: Vec<usize> = seam.occupied_rows().iter().map(|(r, _)| *r).collect();
            assert_eq!(
                occupied,
                (0..=ENTERS).collect::<Vec<_>>(),
                "{ctx}: each Enter must consume exactly ONE row — two-row \
                 spacing is the 2026-08-02 drift shape\n{}",
                seam.dump()
            );
            let width = seam.occupied_rows().last().expect("a prompt").1;
            assert_eq!(
                seam.mado_cursor(),
                (ENTERS, width + 1),
                "{ctx}: caret must rest just past the last prompt, on the \
                 prompt's OWN row\n{}",
                seam.dump()
            );
        }

        Interaction::TypeAndErase => {
            seam.settle_to_clean_prompt(ctx);
            let base = seam.occupied_rows()[0].1;
            seam.send(b"echo");
            assert!(
                wait_until(PHASE_DEADLINE, || seam.mado_cursor() == (0, base + 1 + 4)),
                "{ctx}: 4 typed chars must advance the caret 4 cells on the \
                 prompt's row\n{}",
                seam.dump()
            );
            assert_eq!(
                seam.occupied_rows().len(),
                1,
                "{ctx}: typing must not spill onto a second row\n{}",
                seam.dump()
            );
            for _ in 0..4 {
                seam.send(b"\x7f");
            }
            assert!(
                wait_until(PHASE_DEADLINE, || seam.mado_cursor() == (0, base + 1)),
                "{ctx}: backspacing back to empty must restore the caret to \
                 the prompt's end cell\n{}",
                seam.dump()
            );
        }

        Interaction::CommandRoundTrip => {
            seam.settle_to_clean_prompt(ctx);
            seam.send(b"echo SEAM_MARKER\r");
            // Twice: once echoed on the prompt line, once as output.
            assert!(
                wait_until(PHASE_DEADLINE, || {
                    seam.mado_rows()
                        .iter()
                        .filter(|r| r.contains("SEAM_MARKER"))
                        .count()
                        >= 2
                }),
                "{ctx}: echo did not round-trip into the grid twice\n{}",
                seam.dump()
            );
            // The output line must be its own row, not appended to the
            // prompt's — the geometry half of a round-trip.
            let rows = seam.mado_rows();
            let output_row = rows
                .iter()
                .position(|r| r.trim() == "SEAM_MARKER")
                .unwrap_or_else(|| panic!("{ctx}: no standalone output row\n{}", seam.dump()));
            assert!(
                output_row >= 1,
                "{ctx}: output cannot precede the prompt that produced it\n{}",
                seam.dump()
            );
        }

        Interaction::Resize => {
            seam.settle_to_clean_prompt(ctx);
            seam.resize(COLS / 2, ROWS);
            // The shell repaints on SIGWINCH; both grids must land on the
            // new width and still agree.
            assert!(
                wait_until(PHASE_DEADLINE, || {
                    seam.tear_rows().len() == ROWS && !seam.occupied_rows().is_empty()
                }),
                "{ctx}: no prompt survived the resize\n{}",
                seam.dump()
            );
            let (row, col) = seam.mado_cursor();
            assert!(
                col < COLS / 2 && row < ROWS,
                "{ctx}: caret ({row},{col}) outside the resized {}×{ROWS} grid\n{}",
                COLS / 2,
                seam.dump()
            );
            seam.resize(COLS, ROWS);
        }

        Interaction::ScrollAtBottom => {
            seam.settle_to_clean_prompt(ctx);
            // Walk the prompt down to the last row ONE STEP AT A TIME,
            // waiting for each repaint to land. Blasting ROWS Enters at
            // once races the shell's own repaint loop: the harness reads a
            // half-drawn frame and the row count lies. (Observed directly
            // — 2/10 runs, with the caret parked at column 0 of a row the
            // prompt had not been written to yet.)
            for target in 1..ROWS {
                seam.send(b"\r");
                assert!(
                    wait_until(PHASE_DEADLINE, || {
                        seam.occupied_rows().last().map(|(r, _)| *r) == Some(target)
                    }),
                    "{ctx}: prompt did not step down to row {target}\n{}",
                    seam.dump()
                );
            }
            let width = seam.occupied_rows().last().expect("a prompt").1;
            assert_eq!(
                seam.mado_cursor(),
                (ROWS - 1, width + 1),
                "{ctx}: after scrolling, the caret must still sit just past \
                 the prompt on the LAST row — a CPR answer that is off by \
                 one across a scroll walks the prompt off the screen\n{}",
                seam.dump()
            );
        }
    }
}

/// Anti-vacuity gate for [`Seam::assert_grids_agree`].
///
/// That assertion compares two grids that, in every healthy run, are
/// identical — which is exactly the shape that rots into a check of
/// nothing. If `mado_rows`/`tear_rows` ever start reading the same source,
/// or both start returning empty, the matrix above would stay green while
/// asserting no seam at all.
///
/// So: put a glyph into mado's mirror that tear never saw, and require the
/// comparison to notice. Cheap, and it fails loudly the day the two
/// accessors collapse into one.
#[test]
fn seam_agreement_is_not_vacuous() {
    let Some(bin) = SHELLS[0].resolve() else {
        eprintln!("SKIP: seam_agreement_is_not_vacuous — no shell deployed here");
        return;
    };
    let seam = Seam::boot(&bin, 100, 20);
    seam.settle_to_clean_prompt("anti-vacuity");
    assert_eq!(
        seam.mado_rows(),
        seam.tear_rows(),
        "precondition: a healthy seam agrees"
    );

    // A byte straight into mado's mirror, bypassing tear entirely.
    seam.terminal.write().feed(b"\x1b[20;1HDIVERGENCE");
    assert_ne!(
        seam.mado_rows(),
        seam.tear_rows(),
        "the seam comparison must SEE a mado-only write — if it cannot, \
         `assert_grids_agree` is comparing something against itself and \
         every green cell in the matrix above means nothing"
    );
}

#[test]
fn shell_seam_matrix() {
    let mut ran = 0usize;
    let mut skipped: Vec<&str> = Vec::new();

    for shell in SHELLS {
        let Some(bin) = shell.resolve() else {
            skipped.push(shell.bin);
            continue;
        };
        for interaction in Interaction::ALL {
            let ctx = format!("{}/{:?}", shell.bin, interaction);
            let seam = Seam::boot(&bin, 100, 20);
            drive(&seam, *interaction, &ctx);
            // Shared invariants, applied to EVERY cell of the matrix.
            seam.assert_query_loop_closed(&ctx);
            seam.assert_grids_agree(&ctx);
            ran += 1;
        }
    }

    // Never let a shrinking matrix read as a green matrix. The row is
    // allowed to skip wholesale on a machine with no pleme-io shell, but
    // it always says how many cells actually ran and which shells were
    // dropped — a silent cap reads as "covered everything" when it isn't.
    eprintln!(
        "shell_seam_matrix: ran {ran} cell(s) = {} shell(s) × {} interaction(s)",
        SHELLS.len() - skipped.len(),
        Interaction::ALL.len()
    );
    if !skipped.is_empty() {
        eprintln!("  skipped (not deployed here): {}", skipped.join(", "));
    }
    if ran == 0 {
        eprintln!(
            "SKIP: shell_seam_matrix — no pleme-io shell resolved and no \
             MADO_L1_SHELL override (local-only row, mirrors §L1)"
        );
    }
}
