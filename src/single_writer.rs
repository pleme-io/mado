//! Single-writer election for mado's shared on-disk state (the suggestion
//! snapshot + the praça snapshot). Every mado process (GUI windows, `mado
//! mcp`) READS the state files at boot, but only the election winner ever
//! WRITES them — two processes otherwise race the same atomic-rename target
//! and the last writer silently clobbers the other's rows.
//!
//! The election is an advisory `flock(LOCK_EX | LOCK_NB)` on a lock file in
//! the state dir, held for the process lifetime (the OS releases it on any
//! exit, including a crash — no stale-pid protocol needed). Losers keep
//! full in-memory behavior; they just skip persistence.

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

/// Whether THIS process won the state-writer election (memoized on first
/// call; the lock is held until exit). Both the suggestion persist task and
/// the praça persist gate on this.
#[must_use]
pub fn is_writer() -> bool {
    static ELECTION: OnceLock<Option<WriterLock>> = OnceLock::new();
    ELECTION
        .get_or_init(|| {
            let dir = crate::suggest::state_path()
                .parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(std::env::temp_dir);
            let won = try_acquire(&dir);
            if won.is_none() {
                tracing::info!(
                    "another mado holds the state-writer lock — this process reads state but never persists it"
                );
            }
            won
        })
        .is_some()
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
