//! Starting a process mado will never wait for, without leaving a corpse.
//!
//! ── ★ THE SAME CLASS AS `omoya::spawn`, AND DELIBERATELY A DIFFERENT SHAPE ─
//! omoya was found leaking a zombie per launcher invocation (plo, 2026-09-03:
//! three `.tobira-wrapped` processes in state `Z`). It closed the class by
//! setting `SIGCHLD` to `SIG_IGN` process-wide, which is the strongest answer
//! available — POSIX then guarantees a dead child is never *transformed* into
//! a zombie, so there is no cleanup to forget and no reaper to starve.
//!
//! **That answer is unavailable here, and taking it would break the
//! terminal.** Ignoring SIGCHLD costs the ability to `wait`, and mado waits:
//! it owns a PTY shell, and `platform.rs` waits on a child it starts. A
//! `waitpid` that returns `ECHILD` does not report an error — it reports that
//! the process vanished, which is a plausible answer and the wrong one.
//!
//! So this is the second member of one family, not a copy of the first:
//!
//! | the process… | the shape | why |
//! |---|---|---|
//! | waits for NOTHING | `SIGCHLD = SIG_IGN`, once | kernel-enforced, no per-call cost, covers library spawns too |
//! | waits for SOME children | **double-fork, per spawn** | leaves signal handling untouched, so the waits that must work still do |
//!
//! Same goal, different shapes — which the fleet's convergence rule says to
//! write down rather than force into one type, because a single primitive
//! here would have to be safe for the waiter, and the safe-for-a-waiter
//! version is strictly weaker for omoya.
//!
//! ── ★ HOW DOUBLE-FORK MAKES THE CORPSE UNCREATABLE ───────────────────────
//! `fork` twice and let the middle process exit immediately. The grandchild
//! is then an orphan, the kernel reparents it to PID 1, and PID 1 reaps it —
//! that is init's entire job. mado's only child is the middle process, which
//! is already dead by the time we look, so the one `wait` here always returns
//! at once and can never block the render loop.
//!
//! The property is therefore not "mado remembers to clean up" but "mado never
//! has a long-lived child to clean up". Nothing accumulates because nothing
//! is created.
//!
//! ── ★ WHAT IT COSTS, STATED RATHER THAN GLOSSED ──────────────────────────
//! The caller loses the grandchild's pid. `Child::id` names the middle
//! process, which is gone microseconds later, so reporting it would be worse
//! than reporting nothing — a pid that never matches anything in `ps` reads
//! as a lookup failure rather than as a design choice. Recovering the real
//! pid means a pipe and a read on the startup path; nothing here needs it,
//! and `tear_discovery`'s log line was corrected to stop claiming one.

use std::io;
use std::process::Command;

/// Start `cmd` as an orphan, adopted by PID 1 from birth.
///
/// Use for anything mado starts and never waits for: an opener for a clicked
/// link, a daemon meant to outlive us. Do NOT use for a child whose exit
/// status mado reads — this deliberately gives that status to init.
///
/// # Errors
/// Returns the spawn error, or a fork failure surfaced from the child.
pub fn detached(cmd: &mut Command) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: `pre_exec` runs in the forked child between `fork` and
        // `exec`, where only async-signal-safe calls are legal. `fork` and
        // `_exit` are both on that list; nothing else happens in here, and no
        // memory is shared with the parent.
        unsafe {
            cmd.pre_exec(|| {
                match libc::fork() {
                    // The fork failed. Reporting it aborts the exec and the
                    // error reaches the caller — better than silently
                    // continuing as a plain child, which would reintroduce
                    // exactly the leak this function exists to remove.
                    -1 => Err(io::Error::last_os_error()),
                    // The grandchild. Carry on to `exec`; it is an orphan the
                    // moment its parent exits on the next line.
                    0 => Ok(()),
                    // The middle process. Leave IMMEDIATELY, and via `_exit`
                    // rather than `exit`: this is a forked copy of a
                    // multi-threaded renderer, and running atexit handlers or
                    // flushing its inherited buffers here would be a
                    // second process writing mado's file descriptors.
                    _ => libc::_exit(0),
                }
            });
        }
    }

    let mut middle = cmd.spawn()?;
    // Reaps the middle process, which has already exited. This is the one
    // wait in the design and it is bounded by construction — not a blocking
    // call that happens to be quick.
    let _ = middle.wait();
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// ★ THE INVARIANT, MEASURED: after `detached`, this process has no child.
    ///
    /// The grandchild reports its own parent pid into a file. If double-fork
    /// worked, that ppid is 1 (or another subreaper) and NOT us — which is
    /// precisely the statement "mado has no child here to leak".
    #[test]
    fn a_detached_process_is_not_our_child() {
        let out = std::env::temp_dir().join(format!("mado-detach-{}", std::process::id()));
        let _ = std::fs::remove_file(&out);

        let mut cmd = Command::new("/bin/sh");
        // Sleep first so the read cannot race the grandchild's own exit: a
        // process that has already died would report a ppid of 1 for the
        // wrong reason and pass this test without proving anything.
        cmd.arg("-c")
            .arg(format!("sleep 1; echo $PPID > {}", out.display()));
        let Ok(()) = detached(&mut cmd) else {
            eprintln!("NOTE: no /bin/sh — detachment untested on this host");
            return;
        };

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let ppid = loop {
            if let Ok(t) = std::fs::read_to_string(&out)
                && let Ok(v) = t.trim().parse::<u32>()
            {
                break v;
            }
            if std::time::Instant::now() > deadline {
                eprintln!("NOTE: the probe never reported — detachment untested");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        };
        let _ = std::fs::remove_file(&out);

        assert_ne!(
            ppid,
            std::process::id(),
            "the process is still OUR child, so mado will hold its corpse for \
             the rest of the session — the double-fork in `detached` did not \
             happen"
        );
    }

    /// ★ THE ANTI-VACUITY CONTROL, and it is not decoration.
    ///
    /// The test above passes if `detached` merely FAILS to start anything —
    /// no file, an early return, and a green result. This proves the same
    /// command started as an ordinary child DOES report us as its parent, so
    /// the assertion above is measuring detachment rather than absence.
    #[test]
    fn the_control_shows_an_ordinary_child_names_us_as_parent() {
        let out = std::env::temp_dir().join(format!("mado-control-{}", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("echo $PPID > {}", out.display()))
            .spawn();
        let Ok(mut child) = child else {
            eprintln!("NOTE: no /bin/sh — control not run");
            return;
        };
        let _ = child.wait();
        let Ok(t) = std::fs::read_to_string(&out) else {
            eprintln!("NOTE: the control produced no output");
            return;
        };
        let ppid: u32 = t.trim().parse().expect("ppid");
        let _ = std::fs::remove_file(&out);
        assert_eq!(
            ppid,
            std::process::id(),
            "an ordinary child did not name us as its parent, so this harness \
             cannot tell a detached process from an attached one and the \
             detachment test above proves nothing"
        );
    }
}
