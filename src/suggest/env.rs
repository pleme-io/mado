//! The side-effect boundary every suggestion source reads through — the
//! TYPED-SPEC+INTERPRETER triplet's `Environment` trait. A source's `poll`
//! does ALL its I/O through a `&dyn SuggestionEnvironment`, so:
//!
//! * the real impl ([`RealEnvironment`]) shells typed [`Cmd`]s (NO shell —
//!   `std::process::Command` argv), does HTTP via a typed `curl` argv, reads
//!   files, and resolves sops-rendered secrets; and
//! * tests drive a [`MockEnvironment`] of canned fixtures, so EVERY source is
//!   unit-tested without touching the network, a subprocess, or the cluster.
//!
//! Every method returns `Option` and is best-effort: a missing binary, a
//! non-2xx response, an absent secret, or an unauthed CLI all yield `None`,
//! never a panic — an unconfigured source simply contributes nothing.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A typed external command — program + argv, never a shell string. The one
/// sanctioned subprocess shape (per the NO SHELL law: a typed wrapper that
/// constructs `Command` from typed pieces).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cmd {
    program: String,
    args: Vec<String>,
}

impl Cmd {
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    #[must_use]
    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        for a in it {
            self.args.push(a.into());
        }
        self
    }

    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }
    #[must_use]
    pub fn args_slice(&self) -> &[String] {
        &self.args
    }

    /// Stable lookup key (program + space-joined args) for `MockEnvironment`.
    #[must_use]
    pub fn key(&self) -> String {
        let mut k = String::new();
        k.push_str(&self.program);
        for a in &self.args {
            k.push(' ');
            k.push_str(a);
        }
        k
    }
}

/// A typed HTTP GET — url + headers. The real impl runs it through `curl -sf`
/// (a typed argv, not a shell line); tests mock it by url.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpReq {
    url: String,
    headers: Vec<(String, String)>,
    basic: Option<(String, String)>,
}

impl HttpReq {
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: Vec::new(),
            basic: None,
        }
    }

    /// HTTP Basic auth (e.g. Atlassian Cloud: email + API token). curl
    /// computes the header so we need no base64 dependency.
    #[must_use]
    pub fn basic_auth(mut self, user: impl Into<String>, pass: impl Into<String>) -> Self {
        self.basic = Some((user.into(), pass.into()));
        self
    }

    #[must_use]
    pub fn basic(&self) -> Option<&(String, String)> {
        self.basic.as_ref()
    }

    #[must_use]
    pub fn header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    /// Convenience: `Authorization: Bearer <token>`.
    #[must_use]
    pub fn bearer(self, token: &str) -> Self {
        let mut v = String::from("Bearer ");
        v.push_str(token);
        self.header("Authorization", v)
    }

    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
    #[must_use]
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }
}

/// The mockable I/O boundary. Implemented once for real, once for tests.
pub trait SuggestionEnvironment: Send + Sync {
    /// Run a typed command; `Some(stdout)` on exit-0, `None` otherwise.
    fn run(&self, cmd: &Cmd) -> Option<String>;
    /// HTTP GET; `Some(body)` on 2xx, `None` otherwise.
    fn http_get(&self, req: &HttpReq) -> Option<String>;
    /// Read a file to a string, `None` if absent/unreadable.
    fn read_file(&self, path: &Path) -> Option<String>;
    /// Whether a path exists (a source prefers a real cwd for its spawn).
    fn path_exists(&self, path: &Path) -> bool;
    /// Resolve a sops-style secret by `category/name` (e.g.
    /// `atlassian/api-token`), `None` if not materialized.
    fn secret(&self, key: &str) -> Option<String>;
    /// Current unix seconds (injected for determinism + decay).
    fn now_unix(&self) -> u64;
    /// The operator's code root (`~/code`) for cwd resolution.
    fn code_root(&self) -> PathBuf;
    /// The operator's home directory.
    fn home(&self) -> PathBuf;
}

/// The production environment: real subprocesses, `curl` HTTP, filesystem,
/// sops-rendered secrets under `~/.config/<category>/<name>`.
pub struct RealEnvironment {
    code_root: PathBuf,
    home: PathBuf,
}

impl RealEnvironment {
    /// Discover roots from the environment (`PLEME_CODE_ROOT` override, else
    /// `~/code`).
    #[must_use]
    pub fn discover() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let code_root = std::env::var_os("PLEME_CODE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("code"));
        Self { code_root, home }
    }
}

impl SuggestionEnvironment for RealEnvironment {
    fn run(&self, cmd: &Cmd) -> Option<String> {
        let out = Command::new(cmd.program())
            .args(cmd.args_slice())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn http_get(&self, req: &HttpReq) -> Option<String> {
        // curl as a typed argv: `-sf` = silent + fail-on-error (non-2xx →
        // exit≠0 → None), bounded by a max-time so a wedged endpoint can't
        // stall the watcher. No shell — argv only.
        let mut c = Cmd::new("curl")
            .arg("-sf")
            .arg("--max-time")
            .arg("10");
        if let Some((user, pass)) = req.basic() {
            let mut up = String::new();
            up.push_str(user);
            up.push(':');
            up.push_str(pass);
            c = c.arg("-u").arg(up);
        }
        for (k, v) in req.headers() {
            let mut h = String::new();
            h.push_str(k);
            h.push_str(": ");
            h.push_str(v);
            c = c.arg("-H").arg(h);
        }
        c = c.arg(req.url());
        self.run(&c)
    }

    fn read_file(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn secret(&self, key: &str) -> Option<String> {
        // sops-nix renders secrets to ~/.config/<category>/<name>.
        let p = self.home.join(".config").join(key);
        std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn code_root(&self) -> PathBuf {
        self.code_root.clone()
    }
    fn home(&self) -> PathBuf {
        self.home.clone()
    }
}

/// A canned environment for unit tests — every source is tested by feeding it
/// fixture command output / HTTP bodies / files / secrets and asserting the
/// suggestions it produces.
#[derive(Default, Clone)]
pub struct MockEnvironment {
    cmds: BTreeMap<String, String>,
    https: BTreeMap<String, String>,
    files: BTreeMap<PathBuf, String>,
    secrets: BTreeMap<String, String>,
    paths: std::collections::BTreeSet<PathBuf>,
    now: u64,
    code_root: PathBuf,
    home: PathBuf,
}

impl MockEnvironment {
    #[must_use]
    pub fn new() -> Self {
        Self {
            now: 1_000_000,
            code_root: PathBuf::from("/code"),
            home: PathBuf::from("/home/op"),
            ..Self::default()
        }
    }

    /// Register stdout for a command keyed by [`Cmd::key`] (program + args).
    #[must_use]
    pub fn cmd(mut self, key: impl Into<String>, out: impl Into<String>) -> Self {
        self.cmds.insert(key.into(), out.into());
        self
    }
    #[must_use]
    pub fn http(mut self, url: impl Into<String>, body: impl Into<String>) -> Self {
        self.https.insert(url.into(), body.into());
        self
    }
    #[must_use]
    pub fn file(mut self, p: impl Into<PathBuf>, c: impl Into<String>) -> Self {
        self.files.insert(p.into(), c.into());
        self
    }
    /// Mark a path as existing (for `path_exists`).
    #[must_use]
    pub fn path(mut self, p: impl Into<PathBuf>) -> Self {
        self.paths.insert(p.into());
        self
    }
    #[must_use]
    pub fn secret_val(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.secrets.insert(k.into(), v.into());
        self
    }
    #[must_use]
    pub fn at(mut self, now: u64) -> Self {
        self.now = now;
        self
    }
    #[must_use]
    pub fn roots(mut self, code_root: impl Into<PathBuf>, home: impl Into<PathBuf>) -> Self {
        self.code_root = code_root.into();
        self.home = home.into();
        self
    }
}

impl SuggestionEnvironment for MockEnvironment {
    fn run(&self, cmd: &Cmd) -> Option<String> {
        self.cmds.get(&cmd.key()).cloned()
    }
    fn http_get(&self, req: &HttpReq) -> Option<String> {
        self.https.get(req.url()).cloned()
    }
    fn read_file(&self, path: &Path) -> Option<String> {
        self.files.get(path).cloned()
    }
    fn path_exists(&self, path: &Path) -> bool {
        self.paths.contains(path) || self.files.contains_key(path)
    }
    fn secret(&self, key: &str) -> Option<String> {
        self.secrets.get(key).cloned()
    }
    fn now_unix(&self) -> u64 {
        self.now
    }
    fn code_root(&self) -> PathBuf {
        self.code_root.clone()
    }
    fn home(&self) -> PathBuf {
        self.home.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_key_is_program_plus_args() {
        let c = Cmd::new("gh").arg("pr").arg("list").args(["--json", "title"]);
        assert_eq!(c.key(), "gh pr list --json title");
    }

    #[test]
    fn http_bearer_sets_auth_header() {
        let r = HttpReq::new("https://x").bearer("tok");
        assert_eq!(r.headers()[0].0, "Authorization");
        assert_eq!(r.headers()[0].1, "Bearer tok");
    }

    #[test]
    fn mock_env_serves_fixtures_and_none_otherwise() {
        let env = MockEnvironment::new()
            .cmd("gh pr list", "[]")
            .http("https://api/x", "{}")
            .secret_val("atlassian/api-token", "abc")
            .at(42);
        assert_eq!(env.run(&Cmd::new("gh").arg("pr").arg("list")).as_deref(), Some("[]"));
        assert_eq!(env.run(&Cmd::new("nope")), None);
        assert_eq!(env.http_get(&HttpReq::new("https://api/x")).as_deref(), Some("{}"));
        assert_eq!(env.http_get(&HttpReq::new("https://api/missing")), None);
        assert_eq!(env.secret("atlassian/api-token").as_deref(), Some("abc"));
        assert_eq!(env.secret("missing/key"), None);
        assert_eq!(env.now_unix(), 42);
    }
}
