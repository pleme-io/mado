//! Praça persistence — the preset catalog (and the rest of the praça
//! snapshot) survives a mado restart.
//!
//! The praca crate ships the whole algebra (`Praca::to_snapshot` /
//! `from_snapshot`, with the definitions catalog carried since the pinned
//! rev); this module is only the mado-side wiring: WHERE the snapshot lives,
//! WHEN it is written, and WHICH half is restored.
//!
//! **Restore = definitions only (deliberate slice).** In the embedded
//! runtime every session dies with the process, so a persisted index/binding
//! is a catalog of ghosts: stale bindings would steer auto-attach at dead
//! sessions and stale records carry unreachable frecency. What has durable
//! value is the LATENT half — the saved presets the ○ rows instantiate —
//! so load restores `definitions` wholesale and leaves the boot-fresh
//! index/binding/policy/style untouched (config wins). The persisted file
//! still carries the FULL snapshot, so the tear-daemon world (which outlives
//! sessions) can reuse it unchanged later.
//!
//! Writes ride the suggestion engine's maintenance tick (one place, off the
//! GUI hot path), are BLAKE3-framed + atomic like the suggestion snapshot,
//! change-gated by a content hash, and gated on the single-writer election.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// The registered live praça (the SAME `Arc<Mutex<_>>` the picker bridge +
/// auto-attach driver hold) plus the persist dirty-check state.
struct Registered {
    praca: std::sync::Arc<Mutex<praca::Praca>>,
    /// BLAKE3 of the last persisted snapshot body — the change gate (praça
    /// has no generation counter; hashing the ~KB snapshot is cheap at the
    /// maintenance cadence).
    last_hash: Mutex<Option<blake3::Hash>>,
}

static REGISTERED: OnceLock<Registered> = OnceLock::new();

/// Resolve the praça snapshot path — same state-dir resolution as the
/// suggestion snapshot (`MADO_STATE_DIR` honored for tests).
#[must_use]
pub fn state_path() -> PathBuf {
    crate::suggest::state_path().with_file_name("praca.json")
}

/// Restore the persisted preset catalog into a freshly-constructed praça,
/// then register it for maintenance persistence. Called once at GUI startup,
/// right after the shared praça is built and BEFORE the picker bridge reads
/// it. Best-effort: no file / torn file / schema bump = nothing restored.
pub fn load_and_register(praca: &std::sync::Arc<Mutex<praca::Praca>>) {
    let path = state_path();
    if let Some(snap) = read_snapshot(&path) {
        let restored = praca::Praca::from_snapshot(snap);
        let n = restored.definitions.all().len();
        if n > 0 {
            let mut g = praca
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            g.definitions = restored.definitions;
            tracing::info!(presets = n, "praça preset catalog restored");
        }
    }
    let _ = REGISTERED.set(Registered {
        praca: std::sync::Arc::clone(praca),
        last_hash: Mutex::new(None),
    });
}

/// One maintenance tick: persist the full snapshot iff this process won the
/// writer election, a praça is registered, and the snapshot content actually
/// changed since the last write. Called from the suggestion engine's
/// maintenance loop (so persistence needs no extra thread); a bare-tier mado
/// with the stream disabled simply never persists presets — acceptable, the
/// bare tier has no picker presets surface either.
pub fn maintenance_tick() {
    let Some(reg) = REGISTERED.get() else {
        return;
    };
    if !crate::single_writer::is_writer() {
        return;
    }
    let snap = {
        let g = reg
            .praca
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.to_snapshot()
    };
    let Ok(json) = serde_json::to_vec(&snap) else {
        return;
    };
    let hash = blake3::hash(&json);
    {
        let mut last = reg
            .last_hash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *last == Some(hash) {
            return;
        }
        *last = Some(hash);
    }
    crate::suggest::store::atomic_write_framed(&state_path(), &json);
}

/// Read + verify the framed snapshot; `None` on absent/torn/bumped files.
fn read_snapshot(path: &std::path::Path) -> Option<praca::PracaSnapshot> {
    let bytes = std::fs::read(path).ok()?;
    let json = crate::suggest::store::unframe_snapshot(&bytes)?;
    serde_json::from_slice(&json).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tear_types::MultiplexerControl;

    #[test]
    fn definitions_round_trip_but_boot_state_wins_elsewhere() {
        let dir = std::env::temp_dir().join("mado-praca-store-test");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("praca.json");

        // A prior run saved one preset (captured off a real live session,
        // the same path the save-as-preset gesture takes) plus an
        // (obsolete) session record.
        let inproc = tear_core::InProcess::new();
        inproc.set_spawn_env(tear_types::SpawnEnv::none());
        let live = inproc
            .new_session_with_source_and_size(
                "x",
                "/bin/sh",
                &[],
                tear_types::SessionSource::Named("praca-store-test".into()),
                (80, 24),
            )
            .expect("spawn");
        let old = std::sync::Mutex::new(praca::Praca::new());
        old.lock()
            .unwrap()
            .index
            .upsert(praca::SessionRecord::for_project(
                live,
                std::path::PathBuf::from("/code/x"),
                ishou_tokens::SessionNameStyle::Emoji,
                100,
            ));
        assert!(
            crate::session_picker::capture_preset(&inproc, &old, live, 100),
            "preset captured from the live session"
        );
        let (snap, def_id) = {
            let g = old.lock().unwrap();
            let def_id = g.definitions.all()[0].def_id;
            (g.to_snapshot(), def_id)
        };
        let json = serde_json::to_vec(&snap).unwrap();
        crate::suggest::store::atomic_write_framed(&path, &json);

        // A fresh boot: restore against the file. Definitions come back;
        // the boot-fresh index (empty here) is NOT polluted with the dead
        // record; policy/name_style stay as constructed.
        let fresh = Arc::new(std::sync::Mutex::new(praca::Praca::new()));
        if let Some(snap) = read_snapshot(&path) {
            let restored = praca::Praca::from_snapshot(snap);
            let mut g = fresh.lock().unwrap();
            g.definitions = restored.definitions;
        }
        let g = fresh.lock().unwrap();
        assert!(
            g.definitions.get(def_id).is_some(),
            "preset survived the restart"
        );
        assert!(
            g.index.all().is_empty(),
            "dead session records are NOT restored"
        );
        drop(g);

        // A torn file restores nothing (framed-verify catches it).
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;
        std::fs::write(&path, &bytes).unwrap();
        assert!(read_snapshot(&path).is_none(), "torn snapshot rejected");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
