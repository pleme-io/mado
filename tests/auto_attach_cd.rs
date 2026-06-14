//! Proof: auto-attach-on-cd — the displayed pane's shell `cd`ing into a
//! different project drives a switch (spawn-if-new / switch-if-bound)
//! that lands the displayed terminal on that project's session, and a
//! `cd` WITHIN the same project does not thrash.
//!
//! mado is a binary-only crate, so `tests/` can't import the GUI's
//! internal `AutoAttachDriver` / `Terminal` / `run_against_pane_unified`
//! (those are unit-tested in `src/auto_attach.rs`). This test instead
//! exercises the EXACT seam the event loop wires together, against the
//! real public crates the driver composes:
//!
//!   * `praca::Praca` — the decision engine (index + binding + policy),
//!   * `tear_core::InProcess` — the live control plane (spawn + the
//!     SessionId→PaneId registry translation),
//!   * `engate_attach` — the displayed-pane attach (subscribe → replay →
//!     live), the same lifecycle the GUI's switchable attach drives.
//!
//! The flow mirrors `src/auto_attach.rs::AutoAttachDriver` exactly:
//!   1. Boot: attach the displayed model to project A's session.
//!   2. cd into project B: `praca.on_cwd_change` decides `SpawnNew`;
//!      spawn B's session in the SAME InProcess, bind+index it, and
//!      perform the switch (drop A's attach, reset the model, attach to
//!      B's pane + replay). Assert the displayed model now shows B.
//!   3. cd WITHIN B (a deeper dir): `on_cwd_change` decides `Stay` —
//!      no second switch (the no-thrash property).
//!   4. cd back into A: `on_cwd_change` decides `SwitchTo` the original
//!      (bound, still-live) session — not a duplicate spawn.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engate_attach::{Attach, Consumer};
use praca::{AttachDecision, AttachPolicy, Praca, ProjectBinding, SessionIndex, SessionRecord};
use tear_core::engate_producer::PaneProducer;
use tear_core::InProcess;
use tear_types::engate_wrap::PaneSnapshotWrap;
use tear_types::{MultiplexerControl, PaneId, SessionId, SessionSource};

/// Consumer mirroring mado's `TerminalSink` shape (the displayed
/// terminal): replay + live both append; `reset` clears (the switch's
/// `terminal.write().reset()`).
#[derive(Clone)]
struct RecordingConsumer(Arc<Mutex<Vec<u8>>>);

impl RecordingConsumer {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::new())))
    }
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    fn reset(&self) {
        self.0.lock().unwrap().clear();
    }
}

impl Consumer for RecordingConsumer {
    type Item = Vec<u8>;
    type Snap = PaneSnapshotWrap;
    fn replay(&mut self, snapshot: Self::Snap) {
        self.0.lock().unwrap().extend(snapshot.to_ansi());
    }
    fn consume(&mut self, item: Self::Item) {
        self.0.lock().unwrap().extend(item);
    }
}

/// A live attach against `pane`, drained until quiescent.
type LiveAttach =
    Attach<engate_types::Live, PaneProducer, RecordingConsumer>;

fn attach_to(inproc: &Arc<InProcess>, pane: PaneId, model: RecordingConsumer) -> LiveAttach {
    let producer = PaneProducer::new(Arc::clone(inproc), pane);
    let attach = Attach::builder()
        .producer(producer)
        .consumer(model)
        .build();
    let (attach, history) = attach.subscribe().expect("subscribe");
    let attach = attach.replay(history).expect("replay");
    let mut attach = attach.start_live();
    drain(&mut attach);
    attach
}

fn drain(attach: &mut LiveAttach) {
    for _ in 0..512 {
        if !attach.poll_one() {
            break;
        }
    }
}

/// The SessionId→PaneId translation the driver performs: an embedded
/// session is one window with one `active_pane`.
fn first_pane_of(inproc: &Arc<InProcess>, session: SessionId) -> Option<PaneId> {
    inproc.with_registry(|r| {
        r.sessions
            .get(&session)
            .and_then(|s| s.windows.values().next().map(|w| w.active_pane))
    })
}

/// Spawn a `/bin/sh` session whose shell starts in `cwd`, return its id.
/// Mirrors the driver's spawn: set the spawn-env cwd, then create.
fn spawn_session_at(inproc: &Arc<InProcess>, name: &str, cwd: &Path) -> SessionId {
    inproc.set_spawn_env(
        tear_types::SpawnEnv::none().with_cwd(Some(cwd.to_string_lossy().into_owned())),
    );
    inproc
        .new_session_with_source_and_size(
            name,
            "/bin/sh",
            SessionSource::Named(name.into()),
            (80, 24),
        )
        .expect("spawn session")
}

/// Write a marker into a pane + wait until its snapshot shows it.
fn write_marker_and_wait(inproc: &Arc<InProcess>, pane: PaneId, marker: &str) {
    let cmd = format!("printf '{marker}\\n'\n");
    inproc.send_keys(pane, cmd.as_bytes()).expect("send_keys");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let snap = inproc.pane_snapshot(pane).expect("snapshot");
        let text = String::from_utf8_lossy(&snap.to_ansi()).into_owned();
        if text.contains(marker) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("marker {marker:?} never appeared in pane {pane}");
}

/// Two `.git`-marked project roots + a nested subdir under B, so
/// production `project_root` collapses subdirs to their root exactly as
/// at runtime.
struct Projects {
    _tmp: tempfile::TempDir,
    a: PathBuf,
    b: PathBuf,
    b_deep: PathBuf,
}

impl Projects {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let a = tmp.path().join("proj-a");
        let b = tmp.path().join("proj-b");
        let b_deep = b.join("crate").join("src");
        std::fs::create_dir_all(a.join(".git")).unwrap();
        std::fs::create_dir_all(b.join(".git")).unwrap();
        std::fs::create_dir_all(&b_deep).unwrap();
        Self {
            _tmp: tmp,
            a,
            b,
            b_deep,
        }
    }
}

#[test]
fn cd_into_a_new_project_spawns_and_switches_then_no_thrash() {
    let p = Projects::new();
    let a_root = praca::project::project_root(&p.a);
    let b_root = praca::project::project_root(&p.b);

    // ── Boot: one InProcess, the displayed model attached to A ─────
    let inproc = Arc::new(InProcess::new());
    let sess_a = spawn_session_at(&inproc, "boot-A", &a_root);
    let pane_a = first_pane_of(&inproc, sess_a).expect("A has a pane");

    let marker_a = "AUTO_ATTACH_A_4F1A";
    write_marker_and_wait(&inproc, pane_a, marker_a);

    let model = RecordingConsumer::new();
    let mut live = attach_to(&inproc, pane_a, model.clone());
    assert!(
        model.text().contains(marker_a),
        "displayed model shows A after boot attach; got {:?}",
        &model.text()[..model.text().len().min(300)]
    );

    // ── Seed praca exactly as AutoAttachDriver::new does ───────────
    let mut index = SessionIndex::new();
    index.upsert(SessionRecord::for_project(
        sess_a,
        a_root.clone(),
        ishou_tokens::SessionNameStyle::Emoji,
        1000,
    ));
    let mut binding = ProjectBinding::new();
    binding.bind(a_root.clone(), sess_a);
    let mut praca = Praca::with(
        index,
        binding,
        AttachPolicy::AutoSwitch,
        ishou_tokens::SessionNameStyle::Emoji,
    );
    let mut current_session = sess_a;

    // ── cd into project B → decide SpawnNew, perform the switch ────
    let decision = praca.on_cwd_change(Some(current_session), &p.b, 1001);
    let (b_spawn_root, b_name) = match decision {
        AttachDecision::SpawnNew { project_root, name } => (project_root, name.to_string()),
        other => panic!("cd into a brand-new project must SpawnNew; got {other:?}"),
    };
    assert_eq!(b_spawn_root, b_root, "spawn at B's resolved root");
    // Deterministic emoji name (the driver passes name.to_string()).
    let expected_name = ishou_tokens::FleetSessionNames::from_project_path(
        &b_root,
        ishou_tokens::SessionNameStyle::Emoji,
    )
    .to_string();
    assert_eq!(b_name, expected_name, "deterministic praça/ishou name");

    // Perform the spawn + bind + index (driver's perform_spawn). The
    // binding + index live on the `praca` facade's public fields, the
    // same surface the driver mutates.
    let sess_b = spawn_session_at(&inproc, &b_name, &b_root);
    let pane_b = first_pane_of(&inproc, sess_b).expect("B has a pane");
    assert_ne!(pane_a, pane_b, "B is a distinct pane");
    praca.binding.bind(b_root.clone(), sess_b);
    praca.index.upsert(SessionRecord::for_project(
        sess_b,
        b_root.clone(),
        ishou_tokens::SessionNameStyle::Emoji,
        1001,
    ));
    current_session = sess_b; // seat advance — the no-thrash guard.

    let marker_b = "AUTO_ATTACH_B_9C3E";
    write_marker_and_wait(&inproc, pane_b, marker_b);

    // The switch (driver posts pane_b → the event loop tears down +
    // rebuilds): drop A's attach, reset the model, attach to B + replay.
    drop(live);
    assert!(
        inproc.pane_snapshot(pane_a).is_ok(),
        "dropping A's attach must not kill A's PTY"
    );
    model.reset();
    let mut live = attach_to(&inproc, pane_b, model.clone());

    // ── The displayed model now reflects B, not A ──────────────────
    let after = model.text();
    assert!(
        after.contains(marker_b),
        "after the cd-switch the displayed model shows B; got {:?}",
        &after[..after.len().min(300)]
    );
    assert!(
        !after.contains(marker_a),
        "A's content is gone after the cd-switch (reset + B-only replay)"
    );

    // ── No-thrash: cd WITHIN B (a deeper dir) decides Stay ─────────
    let within = praca.on_cwd_change(Some(current_session), &p.b_deep, 1002);
    assert_eq!(
        within,
        AttachDecision::Stay,
        "a deeper cwd within the just-attached project must Stay, never re-switch"
    );

    // ── cd back into A → SwitchTo the original bound session ───────
    let back = praca.on_cwd_change(Some(current_session), &p.a, 1003);
    assert_eq!(
        back,
        AttachDecision::SwitchTo(sess_a),
        "cd back into a bound, still-live project switches (not respawns)"
    );
    // And the translation resolves to A's original live pane.
    assert_eq!(first_pane_of(&inproc, sess_a), Some(pane_a));

    // Keep the final attach alive through the assertions.
    drain(&mut live);
}
