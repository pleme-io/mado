//! Typed `std::process::Command` wrappers — the SANCTIONED subprocess exception.
//!
//! The only tools invoked through this module are the genuinely-unavoidable
//! Mac-native ones (codesign, hdiutil, ditto, iconutil) plus whatever publish
//! backend a consumer drives (e.g. the `gh` CLI). Every invocation constructs
//! argv from typed pieces via `.arg()` — there is NO `sh -c "…"` anywhere, and
//! no string is ever interpolated into a shell.
//!
//! Lifted verbatim from gaveta-client's `gaveta-release::cmd` — the FIRST piece
//! of the shared extraction. Both repos now drive subprocesses through one typed
//! builder.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::error::ReleaseError;

/// A typed builder over one external program invocation.
///
/// `run()` inherits stdio and converts a non-zero exit into a typed
/// `ReleaseError::Subprocess`; `quiet()` swallows stdout/stderr for the noisy
/// informational tools (iconutil/hdiutil).
pub struct Tool {
    program: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    quiet: bool,
}

impl Tool {
    /// Start a typed invocation of `program`.
    #[must_use]
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            quiet: false,
        }
    }

    /// Append one typed argument.
    #[must_use]
    pub fn arg(mut self, a: impl AsRef<OsStr>) -> Self {
        self.args.push(a.as_ref().to_string_lossy().into_owned());
        self
    }

    /// Run with this working directory.
    #[must_use]
    pub fn cwd(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Suppress child stdout/stderr (for noisy info tools).
    #[must_use]
    pub fn quiet(mut self) -> Self {
        self.quiet = true;
        self
    }

    /// The argv this invocation would run (for assertions + dry-run plans).
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        self.args.clone()
    }

    /// The program name this invocation would run.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    fn build(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        if let Some(dir) = &self.cwd {
            c.current_dir(dir);
        }
        if self.quiet {
            c.stdout(Stdio::null()).stderr(Stdio::null());
        }
        c
    }

    /// Execute the typed invocation, mapping failure to a typed error.
    pub fn run(self) -> Result<(), ReleaseError> {
        let status = self.build().status().map_err(|source| ReleaseError::Spawn {
            program: self.program.clone(),
            source,
        })?;
        if status.success() {
            Ok(())
        } else {
            Err(ReleaseError::Subprocess {
                program: self.program,
                status,
                args: self.args,
            })
        }
    }

    /// Spawn the invocation as a long-lived child, inheriting nothing (stdio
    /// nulled) so it runs detached in the background. Used by consumers that
    /// need a background helper (e.g. a port-forward guard).
    pub fn spawn_background(self) -> Result<Child, ReleaseError> {
        self.build()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| ReleaseError::Spawn {
                program: self.program,
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_is_constructed_from_typed_pieces() {
        let t = Tool::new("codesign")
            .arg("-s")
            .arg("-")
            .arg("--force")
            .arg("--deep");
        assert_eq!(t.program(), "codesign");
        assert_eq!(t.argv(), vec!["-s", "-", "--force", "--deep"]);
    }
}
