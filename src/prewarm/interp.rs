//! The prewarm interpreter — walks a [`PrewarmSpec`](super::PrewarmSpec)'s
//! ordered steps and drives each side effect through the injectable
//! [`PrewarmEnv`] trait.
//!
//! The trait IS the testability contract (TYPED-SPEC + INTERPRETER triplet):
//! every side effect (send keystrokes, split a pane, open a URL) is behind
//! `PrewarmEnv`, so tests drive `apply` against a mock and no test needs a real
//! PTY / kube / browser. The real `PrewarmEnv` impl (the session executor) is
//! the only place the shipped session verbs are called.
//!
//! `SetEnv` steps are **not** executed here — they are pre-spawn (folded into
//! the spawn env before the session exists); `apply` runs against an
//! already-created session, so it skips them (the caller drains
//! [`PrewarmSpec::env_steps`](super::PrewarmSpec::env_steps) before spawn).

use super::{PrewarmSpec, PrewarmStep};

/// A prewarm side-effect sink. The real impl calls the session's `send_keys` /
/// split-pane / open-url; tests record the calls. Kept deliberately small — one
/// method per post-spawn effect.
pub trait PrewarmEnv {
    /// Deliver a shell command to the session as keystrokes + Enter (the
    /// executor reuses `kickoff_keystrokes`). `cmd` is already injection-safe.
    fn run_command(&mut self, cmd: &str);

    /// Open a URL (a runbook / dashboard deep-link) in the operator's browser.
    fn open_url(&mut self, url: &url::Url);
}

/// What went wrong applying a prewarm step. A typed error, never a silent
/// wrong answer (per the TYPED-SPEC triplet rule — no stub `Ok`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrewarmError {
    /// A step the interpreter does not know how to run at this phase. Today
    /// unreachable (every post-spawn variant is handled), but keeps the walk
    /// total + honest if a future variant lands without an arm.
    UnhandledStep(&'static str),
}

/// Walk a prewarm spec's steps in order against an executor, running the
/// post-spawn effects. Returns the count of shell commands + URLs actuated, or
/// the first typed error. `SetEnv` steps are skipped here (pre-spawn — see the
/// module docs).
///
/// # Errors
/// Returns [`PrewarmError`] if a step cannot be run at the post-spawn phase.
pub fn apply(spec: &PrewarmSpec, env: &mut impl PrewarmEnv) -> Result<usize, PrewarmError> {
    let mut actuated = 0usize;
    for step in spec.steps() {
        match step {
            PrewarmStep::RunCommand(_) | PrewarmStep::KubeContext(_) => {
                // Both lower to a shell command (KubeContext -> kubectl …).
                let cmd = step
                    .shell_command()
                    .ok_or(PrewarmError::UnhandledStep("expected a shell command"))?;
                env.run_command(&cmd);
                actuated += 1;
            }
            PrewarmStep::OpenUrl(url) => {
                env.open_url(url);
                actuated += 1;
            }
            // Pre-spawn: folded into the spawn env before the session exists.
            PrewarmStep::SetEnv { .. } => {}
        }
    }
    Ok(actuated)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording mock — the TYPED-SPEC testability seam. No real PTY/kube/net.
    #[derive(Default)]
    struct MockEnv {
        commands: Vec<String>,
        urls: Vec<String>,
    }

    impl PrewarmEnv for MockEnv {
        fn run_command(&mut self, cmd: &str) {
            self.commands.push(cmd.to_string());
        }
        fn open_url(&mut self, url: &url::Url) {
            self.urls.push(url.to_string());
        }
    }

    #[test]
    fn steps_execute_in_authored_order() {
        // The canonical on-call strategy: set context, describe the pod, open
        // the dashboard — must fire in exactly this order.
        let spec = PrewarmSpec::new(vec![
            PrewarmStep::kube_context("rio").unwrap(),
            PrewarmStep::run("kubectl describe pod api-0").unwrap(),
            PrewarmStep::OpenUrl(url::Url::parse("https://grafana.example/d/abc").unwrap()),
        ]);
        let mut env = MockEnv::default();
        let n = apply(&spec, &mut env).unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            env.commands,
            vec![
                "kubectl config use-context rio",
                "kubectl describe pod api-0"
            ]
        );
        assert_eq!(env.urls, vec!["https://grafana.example/d/abc"]);
    }

    #[test]
    fn set_env_steps_are_skipped_post_spawn() {
        let spec = PrewarmSpec::new(vec![
            PrewarmStep::set_env("KUBECONFIG", "/tmp/kc").unwrap(),
            PrewarmStep::run("kubectl get pods").unwrap(),
        ]);
        let mut env = MockEnv::default();
        let n = apply(&spec, &mut env).unwrap();
        // Only the RunCommand actuates here; SetEnv was drained pre-spawn.
        assert_eq!(n, 1);
        assert_eq!(env.commands, vec!["kubectl get pods"]);
    }

    #[test]
    fn empty_spec_is_a_bare_session() {
        let mut env = MockEnv::default();
        assert_eq!(apply(&PrewarmSpec::default(), &mut env).unwrap(), 0);
        assert!(env.commands.is_empty() && env.urls.is_empty());
    }
}
