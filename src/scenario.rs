//! Scenario harness for headless mado.
//!
//! A *scenario* is a YAML file declaring a terminal session, a
//! sequence of input steps, and a set of assertions on the resulting
//! grid state. The runner spawns an in-process [`Session`] (no MCP,
//! no winit, no GPU), replays the steps, snapshots the grid, and
//! asserts. Each scenario becomes a permanent regression test the
//! moment it lands.
//!
//! ## Why this exists (Constructive Substrate Engineering)
//!
//! Every bug reported by an operator looking at a mado window has
//! always been "I saw weird thing X" — by the time it reaches code,
//! the byte stream that produced X is gone. With scenarios, the
//! exact byte stream becomes a typed artifact. The fix lands with
//! its test as a single commit, and the substrate gets *strictly*
//! stronger: there is no fix-and-forget path. Pattern lifts into
//! `garasu-introspect` (or `kaname-app-introspect`) when the
//! second GPU app needs it — see
//! `pleme-io/theory/HEADLESS-INTROSPECTION.md`.
//!
//! ## YAML shape
//!
//! ```yaml
//! name: basic-prompt
//! description: |
//!   Shell starts, prompt appears, basic ASCII renders correctly.
//! cols: 80
//! rows: 10
//! shell: /bin/sh
//! # Optional: env, cwd, title — same shape as TermSpec.
//! steps:
//!   - send: "echo hello\n"
//!   - wait_for_text: "hello"
//!     timeout_ms: 500
//! expect:
//!   text_contains:
//!     - "echo hello"
//!     - "hello"
//!   cursor:
//!     visible: true
//!   cells:
//!     - row: 1
//!       col: 0
//!       ch: "h"
//! ```
//!
//! ## Runner contract
//!
//! - Each scenario spawns a fresh `Session` — no cross-test state.
//! - Default global timeout is 5s; any single `wait_for_text` step
//!   can override via its own `timeout_ms`.
//! - `send` strings honour the same `\n / \r / \t / \xHH / \e`
//!   escapes as the MCP `send_keys` tool.
//! - On failure, the runner prints a structured cell-by-cell diff
//!   showing exactly which assertions failed against which cells.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::session::{GridSnapshot, SessionRegistry};
use crate::term_spec::TermSpec;

/// Top-level YAML scenario. `serde(deny_unknown_fields)` keeps the
/// format honest — a typo doesn't silently drop a step / assertion.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    #[serde(default = "default_shell")]
    pub shell: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Runner mode. `pty` (default) spawns the `shell` in a real PTY
    /// and feeds bytes through it — the path that catches input-side
    /// line-discipline behaviour. `direct` skips the PTY entirely and
    /// feeds each `send` step directly to a fresh `Terminal::feed` —
    /// the right path when the scenario captures a raw byte stream
    /// (e.g. via `mado record`) that's meant to land in the vte
    /// parser without shell or PTY re-interpretation.
    #[serde(default)]
    pub runner: Runner,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub expect: Expect,
}

/// Runner backend for a scenario. PTY by default — captures the full
/// PTY ↔ shell ↔ terminal stack. `Direct` for byte-stream replays
/// where the PTY would re-interpret the input.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Runner {
    #[default]
    Pty,
    Direct,
}

const fn default_cols() -> u16 { 80 }
const fn default_rows() -> u16 { 24 }
fn default_shell() -> String { "/bin/sh".to_string() }

/// One step in a scenario. Internally tagged on `kind` so the YAML
/// stays unambiguous and serde_yaml_ng deserializes consistently
/// across newtype, struct-variant, and unit shapes.
///
/// YAML example:
///
/// ```yaml
/// steps:
///   - kind: send
///     text: "echo hi\n"
///   - kind: wait_for_text
///     text: "hi"
///     timeout_ms: 500
///   - kind: wait_ms
///     ms: 100
///   - kind: resize
///     cols: 100
///     rows: 30
///   - kind: reset
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// Write raw bytes to the PTY master. Supports the same escape
    /// decoding as the MCP `send_keys` tool: `\n` `\r` `\t` `\\`
    /// `\e` `\xHH` `\0`.
    Send { text: String },
    /// Poll the grid until `to_text()` contains the substring, or
    /// until `timeout_ms` elapses (default 1000ms). On timeout the
    /// scenario fails with a diff showing the last-observed grid.
    WaitForText {
        text: String,
        #[serde(default = "default_wait_ms")]
        timeout_ms: u64,
    },
    /// Fixed delay, useful for "let the shell settle".
    WaitMs { ms: u64 },
    /// Resize the PTY+grid mid-scenario.
    Resize { cols: u16, rows: u16 },
    /// Send `\x1bc` (full reset). Equivalent to a fresh terminal.
    Reset,
}

const fn default_wait_ms() -> u64 { 1000 }

/// Assertions checked after every step completes.
///
/// All fields are optional — a scenario that only cares about
/// "didn't panic" can ship `expect: {}`. Tests pass when every
/// declared assertion passes; an empty assertion set passes.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// Every string in this list must appear in `snapshot.to_text()`.
    #[serde(default)]
    pub text_contains: Vec<String>,
    /// Inverse: every string must *not* appear. Useful for "popup
    /// fully cleared after Esc" assertions.
    #[serde(default)]
    pub text_not_contains: Vec<String>,
    /// Expected cursor state. Each field optional.
    #[serde(default)]
    pub cursor: CursorAssert,
    /// Specific-cell assertions. Each entry pins one cell.
    #[serde(default)]
    pub cells: Vec<CellAssert>,
    /// Expected grid dimensions after replay. Useful when a
    /// scenario tests a `resize` step.
    #[serde(default)]
    pub cols: Option<usize>,
    #[serde(default)]
    pub rows: Option<usize>,
    /// L3 visual-golden hash (lower-case hex blake3 of the
    /// RGBA8 pixels produced by a headless render of the final
    /// terminal state). Locked baseline pinned the moment a
    /// scenario lands; any single-pixel change to the rendered
    /// output flips the hash and fires the test.
    ///
    /// Only enforced when the `gpu_tests` cargo feature is
    /// active (the runner needs a real wgpu adapter). Without
    /// the feature, the field is parsed and silently skipped
    /// so CI runners without a GPU don't mis-fail.
    ///
    /// Recording: leave empty, run the scenario, copy the hash
    /// from the failure message into the YAML, re-run to lock.
    #[serde(default)]
    pub frame_hash: Option<String>,
    /// Surface dimensions for the L3 golden render (physical
    /// pixels). Default 800×600 — picked to leave generous
    /// margin around the scenario's grid without making the
    /// hash sensitive to tiny dim tweaks.
    #[serde(default = "default_frame_width")]
    pub frame_width: u32,
    #[serde(default = "default_frame_height")]
    pub frame_height: u32,
}

const fn default_frame_width() -> u32 { 800 }
const fn default_frame_height() -> u32 { 600 }

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CursorAssert {
    #[serde(default)]
    pub row: Option<usize>,
    #[serde(default)]
    pub col: Option<usize>,
    #[serde(default)]
    pub visible: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CellAssert {
    pub row: usize,
    pub col: usize,
    #[serde(default)]
    pub ch: Option<String>,
    #[serde(default)]
    pub fg: Option<[u8; 3]>,
    #[serde(default)]
    pub bg: Option<[u8; 3]>,
    /// Bit-OR of CellAttrs bits (see `terminal::CellAttrs::bits`).
    #[serde(default)]
    pub attrs: Option<u8>,
    /// Cell width: 1 = normal, 2 = wide char, 0 = continuation.
    #[serde(default)]
    pub width: Option<u8>,
}

/// Load a scenario from a YAML file.
pub fn load(path: &Path) -> Result<Scenario> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let scenario: Scenario = serde_yaml_ng::from_str(&raw)
        .with_context(|| format!("parse {}", path.display()))?;
    Ok(scenario)
}

/// Run a scenario synchronously inside an ad-hoc tokio runtime.
///
/// This is the `#[test]` entrypoint. Each test gets its own runtime
/// + session — no shared state. The runtime is single-threaded
/// because the session's reader pump is the only async consumer
/// and it lives inside `tokio::spawn`.
pub fn run_sync(path: &Path) -> Result<()> {
    let scenario = load(path)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(async { run_with_path(&scenario, Some(path)).await })
}

/// Run a scenario in an existing tokio runtime.
pub async fn run(scenario: &Scenario) -> Result<()> {
    run_with_path(scenario, None).await
}

/// Run a scenario, with optional path-back-reference so the L3
/// golden auto-recorder (MADO_GOLDEN_UPDATE=1) can rewrite the
/// YAML in place when a frame_hash needs to be (re-)captured.
pub async fn run_with_path(scenario: &Scenario, path: Option<&Path>) -> Result<()> {
    match scenario.runner {
        Runner::Pty => run_pty(scenario, path).await,
        Runner::Direct => run_direct(scenario, path),
    }
}

/// L3 visual-golden hash check. Builds a headless GPU render
/// pipeline against the scenario's final terminal state, hashes
/// the resulting RGBA8 pixels via garasu's blake3 frame_hash,
/// compares against the locked YAML golden.
///
/// Gated on the `gpu_tests` feature so CI runners without a real
/// wgpu adapter don't mis-fail. Without the feature, this is a
/// no-op — the YAML field is still parsed (so authors can record
/// goldens during local dev) but enforcement is off.
///
/// Returns `Ok(())` when the hash matches or the feature is off;
/// `Err` with the actual hex hash in the message on mismatch so
/// operators can copy-paste into YAML to update the golden.
#[cfg(feature = "gpu_tests")]
fn check_frame_hash_golden(
    scenario: &Scenario,
    final_state: FinalState<'_>,
    yaml_path: Option<&Path>,
) -> Result<()> {
    let updating = std::env::var("MADO_GOLDEN_UPDATE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // When the field is absent we normally skip — but if the
    // operator is bulk-recording (MADO_GOLDEN_UPDATE=1), treat
    // absence as "please insert" so one test run captures every
    // scenario's golden at once.
    let want = match scenario.expect.frame_hash.as_deref() {
        Some(w) => w.to_string(),
        None if updating => String::new(),
        None => return Ok(()),
    };
    use garasu::headless::{HeadlessTarget, frame_hash};
    use crate::config::CursorStyle;
    use crate::render::TerminalRenderer;
    use crate::terminal::Color;
    use madori::{RenderCallback, RenderContext};
    use std::sync::Arc;

    let term = match final_state {
        FinalState::Owned(t) => Arc::new(parking_lot::RwLock::new(t)),
        FinalState::Shared(t) => t,
        FinalState::_Phantom(_) => unreachable!(),
    };
    let gpu = pollster::block_on(garasu::GpuContext::new())
        .context("scenario L3 golden: gpu init")?;
    let target = HeadlessTarget::new(
        &gpu,
        scenario.expect.frame_width,
        scenario.expect.frame_height,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    );
    let mut renderer = TerminalRenderer::new(
        term,
        14.0,
        "monospace".into(),
        "monospace".into(),
        "monospace".into(), // font_symbols
        0.0,
        CursorStyle::Block,
        false, // no blink — deterministic
        500,
        wgpu::Color { r: 0.180, g: 0.204, b: 0.251, a: 1.0 },
        Color::WHITE,
    );
    renderer.init(&gpu);
    let mut text = garasu::TextRenderer::new(
        &gpu.device,
        &gpu.queue,
        wgpu::TextureFormat::Bgra8UnormSrgb,
    );
    let mut ctx = RenderContext {
        gpu: &gpu,
        text: &mut text,
        surface_view: target.view(),
        width: target.width(),
        height: target.height(),
        scale_factor: 1.0,
        elapsed: 0.0,
        dt: 0.0,
    };
    renderer.render(&mut ctx);
    let _ = gpu.device.poll(wgpu::PollType::Wait);
    let pixels = target.read_pixels_rgba8(&gpu);
    let got = frame_hash(&pixels).to_hex().to_string();
    if got == want {
        return Ok(());
    }
    // Auto-recorder: when MADO_GOLDEN_UPDATE=1 + we know the
    // YAML path, rewrite the file's `frame_hash:` line in place
    // with the new hash. Lets `MADO_GOLDEN_UPDATE=1 cargo test
    // --test scenarios --features gpu_tests` re-record every
    // scenario's golden in one pass instead of paste-and-pray.
    if updating {
        if let Some(p) = yaml_path {
            match rewrite_frame_hash_in_yaml(p, &got) {
                Ok(()) => {
                    eprintln!(
                        "scenario {:?}: MADO_GOLDEN_UPDATE — rewrote {} \
                         with new frame_hash {}",
                        scenario.name, p.display(), got
                    );
                    return Ok(());
                }
                Err(e) => {
                    bail!(
                        "scenario {:?}: MADO_GOLDEN_UPDATE failed to rewrite \
                         {}: {e}\n  (would-be hash: {got})",
                        scenario.name, p.display()
                    );
                }
            }
        }
    }
    bail!(
        "L3 frame_hash mismatch for scenario {:?}:\n  want: {}\n  got:  {}\n\n\
         To accept the new hash: re-run with MADO_GOLDEN_UPDATE=1 in the env. \
         Otherwise the rendered output regressed pixel-exactly relative to the \
         locked baseline.",
        scenario.name, want, got
    );
}

/// Surgical rewrite of the `frame_hash:` line in a scenario YAML.
/// Looks for the first line starting with `frame_hash:` (any
/// leading whitespace is preserved) and replaces the value with
/// the new hash, quoted. Leaves every other character untouched —
/// no full YAML round-trip, no comment loss, no field reorder.
///
/// If the field is missing (because the operator pre-emptied with
/// `frame_hash: ""` but the line doesn't exist), the function
/// inserts it as the first child of the `expect:` block.
#[cfg(feature = "gpu_tests")]
fn rewrite_frame_hash_in_yaml(path: &Path, new_hash: &str) -> Result<()> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let mut out = String::with_capacity(raw.len() + 80);
    let mut hash_replaced = false;
    let mut expect_seen = false;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if !hash_replaced && trimmed.starts_with("frame_hash:") {
            let indent = &line[..line.len() - trimmed.len()];
            out.push_str(indent);
            out.push_str("frame_hash: \"");
            out.push_str(new_hash);
            out.push('"');
            out.push('\n');
            hash_replaced = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if !hash_replaced && trimmed == "expect:" {
            expect_seen = true;
        }
    }
    if !hash_replaced {
        if !expect_seen {
            bail!(
                "no `frame_hash:` line and no `expect:` block found in {}; \
                 cannot auto-insert",
                path.display()
            );
        }
        // Insert a frame_hash line as the first child of expect:.
        // Find expect: again and inject after it. Simpler: do
        // string replace on the first occurrence.
        let needle = "expect:\n";
        if let Some(pos) = out.find(needle) {
            let inject = format!("  frame_hash: \"{new_hash}\"\n");
            out.insert_str(pos + needle.len(), &inject);
        }
    }
    std::fs::write(path, out)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(not(feature = "gpu_tests"))]
fn check_frame_hash_golden(
    _scenario: &Scenario,
    _final_state: FinalState<'_>,
    _yaml_path: Option<&Path>,
) -> Result<()> {
    // No GPU available — silently skip. The YAML field still
    // round-trips through serde so authors can record locally.
    Ok(())
}

/// What we hand to the L3 golden check. Direct-runner gives us
/// an owned Terminal; PTY runner gives us a shared Arc<Mutex<...>>
/// already wrapped by Session.
enum FinalState<'a> {
    Owned(crate::terminal::Terminal),
    #[allow(dead_code)]
    Shared(std::sync::Arc<parking_lot::RwLock<crate::terminal::Terminal>>),
    #[allow(dead_code)]
    _Phantom(std::marker::PhantomData<&'a ()>),
}

async fn run_pty(scenario: &Scenario, yaml_path: Option<&Path>) -> Result<()> {
    let registry = Arc::new(SessionRegistry::default());
    let spec = TermSpec {
        shell: scenario.shell.clone(),
        cwd: scenario.cwd.clone(),
        env: scenario.env.clone(),
        cols: scenario.cols,
        rows: scenario.rows,
        ..TermSpec::default()
    };
    let id = registry.spawn(&spec).await
        .with_context(|| format!("spawn scenario {:?}", scenario.name))?;
    let session = registry.get(&id).ok_or_else(|| anyhow!("session vanished after spawn"))?;

    // Give the shell ~100ms to print its initial prompt — most
    // scenarios reference the prompt in `text_contains`. Scenarios
    // that need a shell-clean start can add `- reset` as the first
    // step.
    tokio::time::sleep(Duration::from_millis(100)).await;

    for (idx, step) in scenario.steps.iter().enumerate() {
        run_step(&session, step, idx).await
            .with_context(|| format!("scenario {:?} step #{idx}", scenario.name))?;
    }

    // Final settle — give the reader pump a moment to drain the
    // PTY for whatever the last step produced.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let snap = session.snapshot_grid();
    check_expectations(&scenario.expect, &snap)
        .with_context(|| format!("scenario {:?} assertions", scenario.name))?;
    // L3 visual-golden check (no-op without --features gpu_tests).
    check_frame_hash_golden(scenario, FinalState::Shared(session.terminal_arc()), yaml_path)
        .with_context(|| format!("scenario {:?} L3 golden", scenario.name))?;
    Ok(())
}

/// Direct-feed runner — no PTY, no shell. Each `send` step's bytes
/// land in the vte parser immediately via `Terminal::feed`. This is
/// the right path for scenarios captured via `mado record` where the
/// captured bytes are already the exact stream the shell wrote and
/// the goal is to replay them into the terminal core verbatim
/// (cat-via-PTY adds a line-discipline echo layer that pollutes the
/// grid).
///
/// `wait_for_text` and `wait_ms` are honoured but the wait is moot —
/// every `send` step feeds synchronously. `resize` calls
/// `Terminal::resize`. `reset` feeds `\x1bc`.
fn run_direct(scenario: &Scenario, yaml_path: Option<&Path>) -> Result<()> {
    use crate::terminal::Terminal;
    let mut term = Terminal::new(scenario.cols as usize, scenario.rows as usize);
    for (idx, step) in scenario.steps.iter().enumerate() {
        match step {
            Step::Send { text } => term.feed(text.as_bytes()),
            Step::WaitForText { text, .. } => {
                // Synchronous — either the previous feed already
                // produced the text or it never will.
                let snap = snapshot_from_terminal(&term);
                if !snap.to_text().contains(text.as_str()) {
                    bail!(
                        "scenario {:?} step #{idx} wait_for_text {text:?} not present\n\
                         after-step grid:\n{}",
                        scenario.name,
                        snap.to_pretty()
                    );
                }
            }
            Step::WaitMs { .. } => {} // no-op in direct mode
            Step::Resize { cols, rows } => {
                term.resize(*cols as usize, *rows as usize);
            }
            Step::Reset => term.feed(b"\x1bc"),
        }
    }
    let snap = snapshot_from_terminal(&term);
    check_expectations(&scenario.expect, &snap)
        .with_context(|| format!("scenario {:?} assertions", scenario.name))?;
    // L3 visual-golden check (no-op without --features gpu_tests).
    // Done AFTER the text/cell assertions so an operator sees
    // semantic failures first, then visual mismatches.
    check_frame_hash_golden(scenario, FinalState::Owned(term), yaml_path)
        .with_context(|| format!("scenario {:?} L3 golden", scenario.name))?;
    Ok(())
}

/// Build a [`GridSnapshot`] from an owned [`Terminal`] without going
/// through `Session` (which expects an `Arc<Mutex<Terminal>>`).
fn snapshot_from_terminal(term: &crate::terminal::Terminal) -> GridSnapshot {
    use crate::session::CellSnapshot;
    let cols = term.cols();
    let rows = term.rows();
    let cursor = *term.cursor();
    let styles = term.styles();
    let cells: Vec<Vec<CellSnapshot>> = term
        .visible_rows()
        .map(|row| {
            row.iter()
                .take(cols)
                .map(|c| {
                    let style = c.style(styles);
                    let attrs = style.attrs;
                    CellSnapshot {
                        ch: c.ch,
                        width: c.width,
                        fg: [style.fg.r, style.fg.g, style.fg.b],
                        bg: [style.bg.r, style.bg.g, style.bg.b],
                        attrs: attrs.to_legacy_bits(),
                        underline: attrs.underline.to_string(),
                        underline_color: match attrs.underline_color {
                            crate::terminal::UnderlineColor::Default => None,
                            other => Some(other.to_string()),
                        },
                    }
                })
                .collect()
        })
        .collect();
    GridSnapshot {
        cols,
        rows,
        cursor_row: cursor.row,
        cursor_col: cursor.col,
        cursor_visible: cursor.visible,
        cells,
    }
}

async fn run_step(
    session: &Arc<crate::session::Session>,
    step: &Step,
    _idx: usize,
) -> Result<()> {
    match step {
        Step::Send { text } => {
            // YAML's native double-quoted string escapes already
            // decode `\n / \r / \t / \xNN / \uNNNN` into the right
            // bytes — we just forward the parsed string as UTF-8.
            //
            // Authors who need to send a literal "\033" string to
            // the shell (so shell + printf interprets it) write
            // YAML single-quoted strings or escape the backslash
            // (`"\\033"`). This is symmetric with how every other
            // YAML consumer in pleme-io treats escape strings; we
            // intentionally don't add a second layer of decoding.
            session.send_input(text.as_bytes()).await?;
        }
        Step::WaitForText { text, timeout_ms } => {
            let deadline = Instant::now() + Duration::from_millis(*timeout_ms);
            loop {
                let snap = session.snapshot_grid();
                if snap.to_text().contains(text.as_str()) {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    bail!(
                        "wait_for_text {text:?} timed out after {timeout_ms}ms\n\
                         last-observed grid:\n{}",
                        snap.to_pretty()
                    );
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
        Step::WaitMs { ms } => {
            tokio::time::sleep(Duration::from_millis(*ms)).await;
        }
        Step::Resize { cols, rows } => {
            session.resize(*cols, *rows).await?;
        }
        Step::Reset => {
            session.send_input(b"\x1bc").await?;
            // Reset takes a moment to flush.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(())
}

fn check_expectations(expect: &Expect, snap: &GridSnapshot) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();
    let text = snap.to_text();

    for needle in &expect.text_contains {
        if !text.contains(needle.as_str()) {
            failures.push(format!("text_contains: missing {needle:?}"));
        }
    }
    for forbidden in &expect.text_not_contains {
        if text.contains(forbidden.as_str()) {
            failures.push(format!("text_not_contains: found {forbidden:?}"));
        }
    }
    if let Some(want) = expect.cursor.row
        && snap.cursor_row != want
    {
        failures.push(format!("cursor.row: want {want}, got {}", snap.cursor_row));
    }
    if let Some(want) = expect.cursor.col
        && snap.cursor_col != want
    {
        failures.push(format!("cursor.col: want {want}, got {}", snap.cursor_col));
    }
    if let Some(want) = expect.cursor.visible
        && snap.cursor_visible != want
    {
        failures.push(format!(
            "cursor.visible: want {want}, got {}",
            snap.cursor_visible
        ));
    }
    if let Some(want) = expect.cols
        && snap.cols != want
    {
        failures.push(format!("cols: want {want}, got {}", snap.cols));
    }
    if let Some(want) = expect.rows
        && snap.rows != want
    {
        failures.push(format!("rows: want {want}, got {}", snap.rows));
    }
    for assert in &expect.cells {
        check_cell(assert, snap, &mut failures);
    }

    if !failures.is_empty() {
        bail!(
            "{} assertion(s) failed:\n{}\n\nfinal grid:\n{}",
            failures.len(),
            failures
                .iter()
                .map(|f| format!("  ✗ {f}"))
                .collect::<Vec<_>>()
                .join("\n"),
            snap.to_pretty()
        );
    }
    Ok(())
}

fn check_cell(assert: &CellAssert, snap: &GridSnapshot, failures: &mut Vec<String>) {
    let Some(row) = snap.cells.get(assert.row) else {
        failures.push(format!(
            "cell[{},{}]: row out of bounds (grid has {} rows)",
            assert.row, assert.col, snap.cells.len()
        ));
        return;
    };
    let Some(cell) = row.get(assert.col) else {
        failures.push(format!(
            "cell[{},{}]: col out of bounds (row has {} cols)",
            assert.row, assert.col, row.len()
        ));
        return;
    };
    if let Some(want_ch) = &assert.ch {
        let want_char = want_ch.chars().next().unwrap_or('\0');
        if cell.ch != want_char {
            failures.push(format!(
                "cell[{},{}].ch: want {:?}, got {:?}",
                assert.row, assert.col, want_char, cell.ch
            ));
        }
    }
    if let Some(want_fg) = assert.fg
        && cell.fg != want_fg
    {
        failures.push(format!(
            "cell[{},{}].fg: want {:?}, got {:?}",
            assert.row, assert.col, want_fg, cell.fg
        ));
    }
    if let Some(want_bg) = assert.bg
        && cell.bg != want_bg
    {
        failures.push(format!(
            "cell[{},{}].bg: want {:?}, got {:?}",
            assert.row, assert.col, want_bg, cell.bg
        ));
    }
    if let Some(want_attrs) = assert.attrs
        && cell.attrs != want_attrs
    {
        failures.push(format!(
            "cell[{},{}].attrs: want 0b{:08b}, got 0b{:08b}",
            assert.row, assert.col, want_attrs, cell.attrs
        ));
    }
    if let Some(want_width) = assert.width
        && cell.width != want_width
    {
        failures.push(format!(
            "cell[{},{}].width: want {want_width}, got {}",
            assert.row, assert.col, cell.width
        ));
    }
}

// ── Recording ─────────────────────────────────────────────────────

/// Options for the `mado record` subcommand.
#[derive(Debug, Clone)]
pub struct RecordOpts {
    pub output: std::path::PathBuf,
    pub name: String,
    pub description: String,
    pub cols: u16,
    pub rows: u16,
    pub cmd: Vec<String>,
}

/// Capture every byte the child writes to its PTY into a scenario
/// YAML that replays the same byte stream against a `cat` passthrough.
///
/// The recorded scenario uses `shell: cat` so the captured bytes —
/// which already contain the escape sequences atuin / fzf / vim
/// emitted — replay verbatim through the terminal core. `expect: {}`
/// is the default; operators add their assertions before committing.
///
/// Non-interactive: stdin is `/dev/null`. The recorder is for
/// capturing one-shot reproducers, not live sessions — interactive
/// recording is a future iteration once `garasu-introspect` lifts.
pub async fn record_session(opts: RecordOpts) -> Result<()> {
    use tokio::io::AsyncReadExt;
    let RecordOpts {
        output,
        name,
        description,
        cols,
        rows,
        cmd,
    } = opts;

    if cmd.is_empty() {
        bail!("record: empty command — pass `-- cmd [args...]` after the mado flags");
    }

    let program = &cmd[0];
    // Glue the command + args into a single shell invocation so we
    // can route through the existing `Pty::spawn` path (which takes
    // a shell + an optional command). Quoting is deliberately naive
    // here — escape any embedded single-quote.
    let joined: String = cmd
        .iter()
        .map(|s| {
            let escaped = s.replace('\'', "'\\''");
            format!("'{escaped}'")
        })
        .collect::<Vec<_>>()
        .join(" ");

    let pty = crate::pty::Pty::spawn(
        "/bin/sh",
        cols,
        rows,
        &std::collections::HashMap::new(),
        None,
        Some(&format!("exec {joined}")),
    )
    .await
    .context("record: spawn PTY")?;

    let mut reader = pty.reader().context("record: reader")?;
    let mut captured: Vec<u8> = Vec::new();
    let mut buf = vec![0u8; 4096];
    // Cap total capture at 1 MiB — guards against runaway processes.
    const MAX_BYTES: usize = 1_048_576;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break, // child exited / PTY closed
            Ok(n) => {
                let take = (MAX_BYTES.saturating_sub(captured.len())).min(n);
                captured.extend_from_slice(&buf[..take]);
                if captured.len() >= MAX_BYTES {
                    tracing::warn!(
                        "record: captured byte cap ({MAX_BYTES}) reached — truncating"
                    );
                    break;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "record: reader error — stopping");
                break;
            }
        }
    }

    drop(pty); // ensure child is reaped before writing the file

    let scenario_text = render_scenario_yaml(
        &name,
        &description,
        program,
        &joined,
        cols,
        rows,
        &captured,
    );
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {parent:?}"))?;
    }
    std::fs::write(&output, scenario_text)
        .with_context(|| format!("write {output:?}"))?;
    eprintln!(
        "mado record: wrote {} bytes of capture to {} ({} bytes captured from {})",
        std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0),
        output.display(),
        captured.len(),
        program,
    );
    Ok(())
}

/// Render a captured byte stream into a scenario YAML using
/// `shell: cat` for replay. The captured bytes are emitted as a
/// double-quoted string with `\xNN` for every non-printable byte so
/// the YAML parser reconstructs them losslessly.
fn render_scenario_yaml(
    name: &str,
    description: &str,
    program: &str,
    cmd_quoted: &str,
    cols: u16,
    rows: u16,
    captured: &[u8],
) -> String {
    let mut out = String::new();
    out.push_str(&format!("name: {name}\n"));
    out.push_str("description: |\n");
    for line in description.lines() {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("  # Captured via `mado record` from: {program}\n"));
    out.push_str(&format!("  # Command: {cmd_quoted}\n"));
    out.push_str(&format!("  # Bytes captured: {}\n", captured.len()));
    out.push_str(&format!("cols: {cols}\n"));
    out.push_str(&format!("rows: {rows}\n"));
    out.push_str("# `direct` runner: the captured byte stream lands in\n");
    out.push_str("# `Terminal::feed` without a PTY, so the vte parser sees\n");
    out.push_str("# exactly what the source process wrote — no\n");
    out.push_str("# line-discipline echo, no shell re-interpretation.\n");
    out.push_str("runner: direct\n");
    out.push_str("steps:\n");
    out.push_str("  - kind: send\n");
    out.push_str("    text: \"");
    for &b in captured {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push_str("\"\n");
    out.push_str("expect:\n");
    out.push_str("  # Operator: fill in assertions for the cells / text\n");
    out.push_str("  # that should hold after the captured stream replays.\n");
    out.push_str("  text_contains: []\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_yaml_round_trips() {
        let raw = r#"
name: example
description: a tiny test
cols: 40
rows: 6
shell: /bin/sh
steps:
  - kind: send
    text: "echo hi\n"
  - kind: wait_for_text
    text: "hi"
    timeout_ms: 300
expect:
  text_contains:
    - "hi"
  cursor:
    visible: true
  cells:
    - row: 1
      col: 0
      ch: "h"
"#;
        let s: Scenario = serde_yaml_ng::from_str(raw).unwrap();
        assert_eq!(s.name, "example");
        assert_eq!(s.cols, 40);
        assert_eq!(s.steps.len(), 2);
        assert!(matches!(&s.steps[0], Step::Send { text } if text.contains("echo")));
        assert!(matches!(&s.steps[1], Step::WaitForText { text, timeout_ms: 300 } if text == "hi"));
        assert_eq!(s.expect.text_contains[0], "hi");
        assert_eq!(s.expect.cells[0].row, 1);
        assert_eq!(s.expect.cells[0].ch.as_deref(), Some("h"));
    }

    #[test]
    fn unknown_yaml_field_is_rejected() {
        // serde(deny_unknown_fields) keeps the format honest.
        let raw = r#"
name: bogus
typo_field: 42
"#;
        let err = serde_yaml_ng::from_str::<Scenario>(raw).unwrap_err();
        assert!(err.to_string().contains("typo_field"), "{err}");
    }

    #[test]
    fn empty_expectations_pass() {
        // No assertions = trivially passes — a "didn't panic" test.
        let snap = GridSnapshot {
            cols: 4, rows: 2, cursor_row: 0, cursor_col: 0, cursor_visible: true,
            cells: vec![vec![]; 2],
        };
        let res = check_expectations(&Expect::default(), &snap);
        assert!(res.is_ok());
    }

    #[test]
    fn missing_text_contains_fails_with_diff() {
        let snap = GridSnapshot {
            cols: 4, rows: 1, cursor_row: 0, cursor_col: 0, cursor_visible: true,
            cells: vec![vec![
                crate::session::CellSnapshot::legacy('a', 1, [255;3], [0;3], 0),
                crate::session::CellSnapshot::legacy('b', 1, [255;3], [0;3], 0),
                crate::session::CellSnapshot::legacy('c', 1, [255;3], [0;3], 0),
                crate::session::CellSnapshot::legacy('d', 1, [255;3], [0;3], 0),
            ]],
        };
        let exp = Expect {
            text_contains: vec!["xyz".into()],
            ..Expect::default()
        };
        let err = check_expectations(&exp, &snap).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("missing"), "diff text missing: {s}");
        assert!(s.contains("xyz"), "diff text missing: {s}");
    }

    /// M2 — the snapshot surface resolves the shrunk Cell through the
    /// StyleTable and carries BOTH the legacy attrs u8 (back-compat,
    /// underline style collapses to the single legacy bit) AND the
    /// typed underline style/colour fields.
    #[test]
    fn snapshot_carries_typed_underline_and_legacy_bits() {
        let mut term = crate::terminal::Terminal::new(20, 2);
        term.feed(b"\x1b[1;4:3;58:5:9;38;2;10;20;30mU\x1b[0mV");
        let snap = snapshot_from_terminal(&term);

        let u = &snap.cells[0][0];
        assert_eq!(u.ch, 'U');
        assert_eq!(u.fg, [10, 20, 30]);
        assert_eq!(u.underline, "curly");
        assert_eq!(u.underline_color.as_deref(), Some("indexed(9)"));
        // Legacy bits: bold (1<<0) + underline (1<<2).
        assert_eq!(u.attrs, 0b0000_0101);

        let v = &snap.cells[0][1];
        assert_eq!(v.ch, 'V');
        assert_eq!(v.underline, "none");
        assert_eq!(v.underline_color, None);
        assert_eq!(v.attrs, 0);
    }
}
