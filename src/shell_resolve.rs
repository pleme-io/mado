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
