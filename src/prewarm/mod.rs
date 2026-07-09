//! Session pre-warming — the on-call cockpit.
//!
//! A `PrewarmSpec` is an ordered list of typed [`PrewarmStep`]s that run
//! **eagerly, once, at session birth** so choosing an alert's session lands the
//! operator already-in-the-issue (kube-context set, pod described, logs
//! streaming, runbook open). It is the ordered generalization of
//! `izumi::SpawnSpec`'s single `initial_command` into the multi-step setup that
//! on-call actually needs.
//!
//! # Scope boundary (mado/CLAUDE.md "mado-memory-privileged only")
//!
//! The **strategy** (which alert class prewarms what, gated on mado live-state)
//! is mado-state-privileged and belongs on the vigy side. The **actions** (the
//! `kubectl`/`open` commands the *new session's shell* runs) are shell-doable
//! and are carried here as **validated data**, run by the shell/executor —
//! never reimplemented as privileged mado write-intrinsics. This module is the
//! typed data + interpreter; the executor ([`PrewarmEnv`] impls) actuates via
//! the shipped session verbs.
//!
//! # Injection defense (load-bearing)
//!
//! A prewarm command is delivered as PTY keystrokes + Enter, so an embedded
//! control byte (`\n`, ESC, …) in upstream alert data would EXECUTE a second
//! command. [`reject_injection`] rejects that at the typed border — the SAME
//! guard `izumi::SpawnSpec::with_command` applies — so a [`PrewarmStep`]
//! carrying an un-runnable / injection-bearing command is **unrepresentable**
//! (its constructor returns `None`).
//!
//! This is the mado-side realization (the tier-honest interim per
//! `docs/PREWARM.md`); the destination lifts `PrewarmSpec`/`PrewarmStep`
//! upstream into `izumi` beside `SpawnSpec`. The typed shape here is
//! deliberately the one that lifts unchanged.

pub mod interp;

// The prewarm core (this module + `interp`) is the M0 foundation: the typed
// border + interpreter, fully tested against a mock. Its consumers — the
// `PrewarmExecutor` at the session-create seam (M1) and the safra strategy
// builder (first slice) — land next, so the public surface is unused until
// then. Named `allow(dead_code)` documents that intent (mado is a bin crate).
// See docs/PREWARM.md.
#[allow(unused_imports)]
pub use interp::{PrewarmEnv, PrewarmError, apply};

/// Reject a command string that is blank or carries a control byte — the shared
/// PTY-newline-injection guard (the exact predicate `izumi::SpawnSpec::
/// with_command` applies). Returns the trimmed-safe command, or `None` if it
/// must not reach a shell. Every `PrewarmStep` that lowers to keystrokes flows
/// its command through this at construction, so an injection-bearing step
/// cannot be built.
#[must_use]
pub fn reject_injection(cmd: &str) -> Option<String> {
    let c = cmd.trim();
    if c.is_empty() || c.chars().any(char::is_control) {
        None
    } else {
        Some(c.to_string())
    }
}

/// One step of a prewarm strategy. Each variant that reaches a shell is
/// **valid-by-construction** — the smart constructors reject injection, so an
/// un-runnable step has no code path (parse-time-rejected tier).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrewarmStep {
    /// Run a command in the session's shell (delivered as keystrokes + Enter).
    /// Built only via [`PrewarmStep::run`] — the command is injection-checked.
    RunCommand(String),
    /// Set an environment variable. Applied **pre-spawn** (folded into the
    /// spawn env before the session is created — env can't be cleanly injected
    /// into an already-live shell), so a `Vec<PrewarmStep>` is NOT a uniform
    /// post-spawn sequence; the executor partitions these out first.
    SetEnv { key: String, value: String },
    /// Set the kube-context. Typed sugar that lowers to `kubectl config
    /// use-context <ctx>`; the ctx flows through the same injection guard.
    KubeContext(String),
    /// Open a URL (a runbook or dashboard deep-link) in the operator's browser.
    /// `url::Url`-typed, so it is parse-time-rejected before it lands here.
    OpenUrl(url::Url),
}

impl PrewarmStep {
    /// Build a [`PrewarmStep::RunCommand`], rejecting blank / injection-bearing
    /// commands (returns `None`). The only way to construct one.
    #[must_use]
    pub fn run(cmd: &str) -> Option<Self> {
        reject_injection(cmd).map(PrewarmStep::RunCommand)
    }

    /// Build a [`PrewarmStep::KubeContext`], rejecting an unusable context name.
    #[must_use]
    pub fn kube_context(ctx: &str) -> Option<Self> {
        reject_injection(ctx).map(PrewarmStep::KubeContext)
    }

    /// Build a [`PrewarmStep::SetEnv`]. The key must be a non-empty, non-control
    /// identifier; the value is rejected only for control bytes (an env value
    /// may legitimately be empty).
    #[must_use]
    pub fn set_env(key: &str, value: &str) -> Option<Self> {
        let k = key.trim();
        if k.is_empty() || k.chars().any(char::is_control) || value.chars().any(char::is_control) {
            return None;
        }
        Some(PrewarmStep::SetEnv { key: k.to_string(), value: value.to_string() })
    }

    /// The shell command this step delivers as keystrokes, if any. `SetEnv`
    /// (pre-spawn) and `OpenUrl` (browser) return `None` — they are not
    /// shell-keystroke steps.
    #[must_use]
    pub fn shell_command(&self) -> Option<String> {
        match self {
            PrewarmStep::RunCommand(c) => Some(c.clone()),
            PrewarmStep::KubeContext(ctx) => Some(format_kube_context(ctx)),
            PrewarmStep::SetEnv { .. } | PrewarmStep::OpenUrl(_) => None,
        }
    }
}

/// Lower a kube-context name to its shell command. One place, so the `kubectl`
/// spelling is not scattered.
#[must_use]
fn format_kube_context(ctx: &str) -> String {
    // ctx is already injection-checked at construction; this is a fixed-shape
    // command with one validated interpolation (not free-form label text).
    let mut s = String::with_capacity(28 + ctx.len());
    s.push_str("kubectl config use-context ");
    s.push_str(ctx);
    s
}

/// An ordered prewarm strategy — the multi-step generalization of a single
/// `initial_command`. Runs once, eagerly, at session birth.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrewarmSpec {
    steps: Vec<PrewarmStep>,
}

impl PrewarmSpec {
    /// A spec from an already-validated step list (every step was built through
    /// a smart constructor, so it is valid-by-construction).
    #[must_use]
    pub fn new(steps: Vec<PrewarmStep>) -> Self {
        Self { steps }
    }

    /// Back-compat: a single `initial_command` lowers to a one-step spec — the
    /// bridge that keeps `izumi::SpawnSpec`'s existing field working when the
    /// spec is empty. Returns an empty spec if the command is unusable.
    #[must_use]
    pub fn from_initial_command(cmd: &str) -> Self {
        Self { steps: PrewarmStep::run(cmd).into_iter().collect() }
    }

    /// The steps, in order.
    #[must_use]
    pub fn steps(&self) -> &[PrewarmStep] {
        &self.steps
    }

    /// True if there is nothing to prewarm (a bare session).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// The `SetEnv` steps — applied **pre-spawn**. The executor drains these
    /// into the spawn env before session creation.
    pub fn env_steps(&self) -> impl Iterator<Item = (&str, &str)> {
        self.steps.iter().filter_map(|s| match s {
            PrewarmStep::SetEnv { key, value } => Some((key.as_str(), value.as_str())),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_injection_matches_the_spawnspec_guard() {
        // The exact predicate izumi::SpawnSpec::with_command applies.
        assert_eq!(reject_injection("gh pr checkout 1234").as_deref(), Some("gh pr checkout 1234"));
        assert_eq!(reject_injection("  trim me  ").as_deref(), Some("trim me"));
        assert_eq!(reject_injection(""), None);
        assert_eq!(reject_injection("   "), None);
        // A newline (the second-command injection) is rejected.
        assert_eq!(reject_injection("safe\nrm -rf /"), None);
        assert_eq!(reject_injection("esc\x1b[2J"), None);
    }

    #[test]
    fn run_step_is_injection_safe_by_construction() {
        assert!(PrewarmStep::run("kubectl describe pod api-0").is_some());
        // An alert label with an embedded newline cannot build a RunCommand.
        assert!(PrewarmStep::run("describe pod x\nrm -rf /").is_none());
        assert!(PrewarmStep::run("  ").is_none());
    }

    #[test]
    fn kube_context_lowers_to_the_kubectl_command() {
        let step = PrewarmStep::kube_context("rio").unwrap();
        assert_eq!(step.shell_command().as_deref(), Some("kubectl config use-context rio"));
        // A context name with a control byte is rejected.
        assert!(PrewarmStep::kube_context("bad\nctx").is_none());
    }

    #[test]
    fn set_env_allows_empty_value_but_not_control_bytes() {
        assert!(PrewarmStep::set_env("KUBECONFIG", "/tmp/kc").is_some());
        assert!(PrewarmStep::set_env("EMPTY_OK", "").is_some());
        assert!(PrewarmStep::set_env("", "x").is_none());
        assert!(PrewarmStep::set_env("BAD", "line1\nline2").is_none());
        // SetEnv is not a shell-keystroke step.
        assert_eq!(PrewarmStep::set_env("A", "b").unwrap().shell_command(), None);
    }

    #[test]
    fn from_initial_command_is_the_back_compat_bridge() {
        let spec = PrewarmSpec::from_initial_command("gh pr checkout 1234");
        assert_eq!(spec.steps().len(), 1);
        assert_eq!(spec.steps()[0], PrewarmStep::RunCommand("gh pr checkout 1234".into()));
        // An unusable command yields an empty (bare-session) spec, not a panic.
        assert!(PrewarmSpec::from_initial_command("bad\ncmd").is_empty());
    }

    #[test]
    fn env_steps_partition_out_the_set_env_steps() {
        let spec = PrewarmSpec::new(vec![
            PrewarmStep::set_env("KUBECONFIG", "/tmp/kc").unwrap(),
            PrewarmStep::run("kubectl get pods").unwrap(),
            PrewarmStep::set_env("NS", "prod").unwrap(),
        ]);
        let envs: Vec<_> = spec.env_steps().collect();
        assert_eq!(envs, vec![("KUBECONFIG", "/tmp/kc"), ("NS", "prod")]);
    }
}
