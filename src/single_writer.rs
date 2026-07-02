//! Single-writer election for mado's shared on-disk state (the suggestion
//! snapshot + the praça snapshot). Every mado process (GUI windows, `mado
//! mcp`) READS the state files at boot, but only the election winner ever
//! WRITES them — two processes otherwise race the same atomic-rename target
//! and the last writer silently clobbers the other's rows.
//!
//! The election is an advisory `flock(LOCK_EX | LOCK_NB)` on a lock file in
//! the state dir, held for the process lifetime once won (the OS releases
//! it on any exit, including a crash — no stale-pid protocol needed).
//! Losers keep full in-memory behavior (and still LOAD the snapshots at
//! boot); they merely skip writing — and they re-contest the role on every
//! [`is_writer`] call, so the writer seat is re-filled the moment it frees.

use std::path::Path;
use std::sync::OnceLock;

/// The held election win — dropping it (never, in practice: it lives in a
/// process-wide static) releases the flock.
pub struct WriterLock {
    _file: std::fs::File,
}

/// Try to become the writer for `dir` (created if absent). `None` = another
/// live mado process already holds the lock.
#[must_use]
pub fn try_acquire(dir: &Path) -> Option<WriterLock> {
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("writer.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // SAFETY: flock on an owned, open fd; NB never blocks.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return None;
        }
    }
    // Non-unix: no flock — assume single instance (mado targets unix).
    Some(WriterLock { _file: file })
}

/// Whether THIS process currently holds the state-writer role. A win is
/// held for the process lifetime; a loss is RE-CONTESTED on every call (a
/// cheap non-blocking flock attempt), so when the winning process exits a
/// surviving instance picks the role up on its next maintenance tick —
/// a live system can never end up with zero writers. Both the suggestion
/// persist task and the praça persist gate call this per tick.
#[must_use]
pub fn is_writer() -> bool {
    static ELECTION: OnceLock<std::sync::Mutex<Option<WriterLock>>> = OnceLock::new();
    let slot = ELECTION.get_or_init(|| std::sync::Mutex::new(None));
    let mut held = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if held.is_some() {
        return true;
    }
    let dir = crate::suggest::state_path()
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    match try_acquire(&dir) {
        Some(lock) => {
            tracing::info!("state-writer role acquired — this process persists the shared snapshots");
            *held = Some(lock);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_on_the_same_dir_loses() {
        let dir = std::env::temp_dir().join("mado-writer-election-test");
        let _ = std::fs::remove_dir_all(&dir);
        // flock is per open-file-description, so two opens in ONE process
        // still contend — the test needs no second process.
        let first = try_acquire(&dir);
        assert!(first.is_some(), "first acquire wins");
        #[cfg(unix)]
        assert!(try_acquire(&dir).is_none(), "second acquire loses while held");
        drop(first);
        assert!(
            try_acquire(&dir).is_some(),
            "the lock releases on drop (and on process exit)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
