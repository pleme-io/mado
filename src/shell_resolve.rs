//! The ONE place mado decides which shell to spawn.
//!
//! ── ★ WHY THIS MODULE EXISTS ────────────────────────────────────────────
//! There were FOUR independent ladders answering one question, and they did
//! not agree. Measured 2026-08-28:
//!
//!   main.rs           $SHELL -> /bin/zsh -> /bin/sh, behind a PATH guard
//!   kanshou_state.rs  params -> config -> $SHELL -> /bin/sh, NO PATH guard
//!   session.rs        $SHELL -> /bin/sh, and it never consults the config at
//!                     all, so a headless pane ignored the operator's choice
//!   mcp.rs            $SHELL -> /bin/sh, re-derived for the REPORT rather than
//!                     read back from what was actually spawned, so the two
//!                     could disagree and the report would be the wrong one
//!
//! Four floors (`/bin/sh` twice, `/bin/zsh` once, `$SHELL` once), two of them
//! unguarded. A pane opened from the GUI, from an agent, and from MCP could
//! each land on a different shell on the same machine.
//!
//! ── ★ THE PRESCRIBED SHELL IS NOW frostmourne, BY OPERATOR DECISION ─────
//! This REVERSES a documented position. `ShellConfig`'s docstring argued that
//! "a config-less mado must feel like the system terminal: it does NOT bind
//! frostmourne", with the fleet opting in through blackmatter-mado's module.
//! That reasoning was sound for a general-purpose terminal. The operator's
//! instruction on 2026-08-28 was explicit -- frostmourne as the default
//! everywhere, "especially in our own software" -- so the prescription moves
//! into the binary.
//!
//! What is NOT reversed is the guard that reasoning protected. frostmourne is
//! tried, never assumed: a release-download user with no frostmourne on PATH
//! still falls through to their own login shell rather than getting a dead
//! window. That was the real content of the old position, and it survives.

/// The fleet's shell. A bare name on purpose -- resolved through `PATH`, so a
/// nix profile, a home-manager profile and a release download all get whatever
/// they actually have rather than a store path baked in at compile time.
pub const PRESCRIBED: &str = ishou_tokens::fleet_shell::FleetShell::prescribed().prescribed;

/// True if `cmd` names a runnable binary: an absolute or relative path that is
/// a file, or a bare name found on `PATH`.
#[must_use]
pub fn is_executable(cmd: &str) -> bool {
    ishou_tokens::fleet_shell::FleetShell::is_executable(cmd)
}

/// Resolve the shell to spawn.
///
/// `configured` is an explicit choice from a CLI flag, a config file, or an
/// MCP/agent request -- `None` means "nobody said", which is where the
/// prescription applies. Every rung is guarded by [`is_executable`], so this
/// never returns a name that is not there.
#[must_use]
pub fn resolve(configured: Option<&str>) -> String {
    // ── ★ THE LADDER MOVED TO THE FLEET TOKEN ───────────────────────────
    // It lived here first, which made mado the second place with a copy --
    // tear needed the identical thing. The prescription and the rules for
    // applying it are one fact, so both now read
    // `ishou_tokens::fleet_shell::FleetShell`. This module stays as mado's
    // door onto it, keeping the tests that came from mado's own defects.
    ishou_tokens::fleet_shell::FleetShell::prescribed().resolve(configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_choice_that_exists_wins() {
        assert_eq!(resolve(Some("/bin/sh")), "/bin/sh");
    }

    #[test]
    fn an_explicit_choice_that_does_not_exist_falls_through() {
        // The old kanshou_state ladder had no guard here and would have
        // returned this string straight to execvp.
        let got = resolve(Some("/nonexistent/definitely-not-a-shell"));
        assert_ne!(got, "/nonexistent/definitely-not-a-shell");
        assert!(
            is_executable(&got),
            "the ladder returned {got:?}, which is not runnable"
        );
    }

    #[test]
    fn an_empty_choice_is_the_same_as_no_choice() {
        // session.rs used `spec.shell.is_empty()` and kanshou_state used
        // `.filter(|s| !s.is_empty())` -- two spellings of one rule, now one.
        assert_eq!(resolve(Some("")), resolve(None));
        assert_eq!(resolve(Some("   ")), resolve(None));
    }

    #[test]
    fn the_result_is_always_runnable() {
        for input in [None, Some(""), Some("/nope"), Some("frostmourne")] {
            let got = resolve(input);
            assert!(
                is_executable(&got) || got == "/bin/sh",
                "resolve({input:?}) returned {got:?}, which is not runnable"
            );
        }
    }
}

/// A shell the operator NAMED that is not runnable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedShellMissing {
    /// Exactly what was asked for, unmodified.
    pub named: String,
}

impl std::fmt::Display for NamedShellMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the shell {:?} is not runnable, and it was named explicitly rather \
             than prescribed — refusing to substitute a different one",
            self.named
        )
    }
}

impl std::error::Error for NamedShellMissing {}

/// Resolve a shell the operator NAMED — with no substitution.
///
/// ── ★ WHY THIS IS A SECOND FUNCTION AND NOT A FLAG ───────────────────────
/// [`resolve`]'s ladder is right for a PRESCRIPTION: a config default, or
/// nothing at all. A release-download user with no `frostmourne` on PATH must
/// get their own login shell rather than a dead window, and that is the whole
/// point of the fallback.
///
/// It is wrong for a shell the operator TYPED. Measured 2026-09-03:
/// `mado e2e --shell /nonexistent/e2e-bogus-shell` reported
/// **`pass: true`** on four of five rows, with `prompt_visible` detailing a
/// real prompt — the operator's own. The fallback had substituted a working
/// shell, and the e2e then certified *that one*, under the name of the shell
/// it never ran. A green verdict about an artifact that was never tested is
/// worse than a red one.
///
/// So authority decides the contract: a prescription may be substituted, a
/// name may not. Same rule as "an explicit `mode=` is never overridden" —
/// when the caller has decided, silently deciding differently is not a
/// convenience, it is a wrong answer wearing one.
///
/// # Errors
/// [`NamedShellMissing`] when `named` is not runnable. The name is returned
/// verbatim in the error so the message says what was asked for.
pub fn resolve_named(named: &str) -> Result<String, NamedShellMissing> {
    if is_executable(named) {
        Ok(named.to_string())
    } else {
        Err(NamedShellMissing {
            named: named.to_string(),
        })
    }
}

#[cfg(test)]
mod named_tests {
    use super::{PRESCRIBED, resolve_named};

    /// ★ THE MEASURED DEFECT: a named shell that does not exist must be an
    /// error, never a substitution.
    #[test]
    fn a_named_shell_that_is_missing_is_refused_not_substituted() {
        let bogus = "/nonexistent/e2e-bogus-shell";
        let err = resolve_named(bogus).expect_err("a missing named shell must refuse");
        assert_eq!(err.named, bogus, "the error must name what was asked for");
        // And the message must not read as a generic failure — it has to say
        // WHY no substitute was chosen, or the next reader "fixes" it by
        // adding one back.
        assert!(
            err.to_string().contains("named explicitly"),
            "the refusal must explain that substitution was declined on \
             purpose: {err}"
        );
    }

    /// ★ ANTI-VACUITY: it must still resolve a shell that IS there, or the
    /// test above passes for a function that refuses everything.
    #[test]
    fn a_named_shell_that_exists_is_returned_verbatim() {
        // `/bin/sh` is the one path POSIX guarantees.
        assert_eq!(resolve_named("/bin/sh").as_deref(), Ok("/bin/sh"));
    }

    /// ★ AND THE PRESCRIPTION KEEPS ITS FALLBACK — the two contracts are
    /// different on purpose, so a change collapsing them fails here.
    #[test]
    fn the_prescription_is_still_allowed_to_fall_back() {
        // `resolve` never returns a name that is not runnable, even when the
        // prescribed shell is absent. That is the download-and-use case and it
        // must survive this split.
        let resolved = super::resolve(Some("definitely-not-a-real-shell-xyzzy"));
        assert!(super::is_executable(&resolved));
        assert!(
            !PRESCRIBED.is_empty(),
            "the prescription must still exist for resolve() to reach for"
        );
    }
}
