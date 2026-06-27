//! Provider impls — one [`SuggestionSource`](super::source::SuggestionSource)
//! per [`SourceKind`](super::core::SourceKind).
//!
//! [`registry`] returns every implemented source; the engine consults the
//! shikumi config to decide which to actually run. Each provider is a pure
//! `poll(env)` tested through a `MockEnvironment` — the live wiring (real
//! `Command`/HTTP/secret) is the only un-mocked seam.

use std::path::PathBuf;
use std::sync::Arc;

use super::env::SuggestionEnvironment;
use super::source::SuggestionSource;

pub mod git_branch_pr;
pub mod tend_repos;

/// Every implemented source, in catalog order. The engine runs the
/// config-enabled subset.
#[must_use]
pub fn registry() -> Vec<Arc<dyn SuggestionSource>> {
    vec![
        Arc::new(git_branch_pr::GitBranchPrSource),
        Arc::new(tend_repos::TendReposSource),
    ]
}

/// Resolve the local working directory for a `owner/name` repo under the
/// operator's code root, following the workspace convention
/// `~/code/${service}/${org}/${repo}` (service defaults to `github`). Falls
/// back to the code root if the conventional path does not exist, so a spawn
/// always has a real cwd.
#[must_use]
pub fn repo_cwd(env: &dyn SuggestionEnvironment, name_with_owner: &str) -> PathBuf {
    let root = env.code_root();
    let mut parts = name_with_owner.splitn(2, '/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or(owner);
    if owner.is_empty() || name.is_empty() {
        return root;
    }
    let candidate = root.join("github").join(owner).join(name);
    if env.path_exists(&candidate) {
        candidate
    } else {
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suggest::core::SourceKind;

    #[test]
    fn registry_kinds_are_unique_and_in_catalog() {
        let reg = registry();
        let mut kinds: Vec<SourceKind> = reg.iter().map(|s| s.kind()).collect();
        let n = kinds.len();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), n, "no source kind registered twice");
        for k in &kinds {
            assert!(SourceKind::ALL.contains(k));
        }
    }

    #[test]
    fn repo_cwd_follows_workspace_convention_when_present() {
        let env = super::super::env::MockEnvironment::new()
            .roots("/code", "/home/op")
            .path("/code/github/pleme-io/mado");
        assert_eq!(
            repo_cwd(&env, "pleme-io/mado"),
            PathBuf::from("/code/github/pleme-io/mado")
        );
        // Missing dir → fall back to the code root (spawn still has a cwd).
        assert_eq!(repo_cwd(&env, "pleme-io/ghost"), PathBuf::from("/code"));
    }
}
