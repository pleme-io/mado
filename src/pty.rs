//! PTY management — spawn shell processes and manage I/O.
//!
//! Uses libc directly for Unix PTY allocation (openpty).
//! Provides async reader/writer interfaces via tokio.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::Child;

/// Errors from PTY operations.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("failed to allocate PTY pair: {0}")]
    Openpty(std::io::Error),

    #[allow(dead_code)]
    #[error("failed to set terminal size: {0}")]
    Resize(std::io::Error),

    #[error("failed to spawn shell process: {0}")]
    Spawn(#[source] std::io::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Result alias for PTY operations.
pub type Result<T> = std::result::Result<T, PtyError>;

// ---------------------------------------------------------------------------
// Newtype wrapper so we can impl AsRawFd for AsyncFd<T>
// ---------------------------------------------------------------------------

struct RawOwnedFd(OwnedFd);

impl AsRawFd for RawOwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

// ---------------------------------------------------------------------------
// Pty
// ---------------------------------------------------------------------------

/// A pseudo-terminal with an associated child shell process.
pub struct Pty {
    master_fd: OwnedFd,
    _child: Child,
}

impl Pty {
    /// Allocate a new PTY and spawn the given shell command inside it.
    pub async fn spawn(shell: &str, cols: u16, rows: u16) -> Result<Self> {
        let mut master_raw: RawFd = 0;
        let mut slave_raw: RawFd = 0;

        let mut ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: openpty is a well-defined POSIX function.
        let ret = unsafe {
            libc::openpty(
                &mut master_raw,
                &mut slave_raw,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut ws,
            )
        };
        if ret < 0 {
            return Err(PtyError::Openpty(std::io::Error::last_os_error()));
        }

        // SAFETY: openpty returned valid fds.
        let master_fd = unsafe { OwnedFd::from_raw_fd(master_raw) };
        let slave_fd = unsafe { OwnedFd::from_raw_fd(slave_raw) };

        // Set the master fd to non-blocking for async I/O.
        set_nonblocking(&master_fd)?;

        // Spawn the child shell with the slave PTY as its controlling terminal.
        let child = spawn_child(shell, &slave_fd, cols, rows)?;

        // The slave fd is now duplicated into the child process; drop our copy.
        drop(slave_fd);

        tracing::info!(shell, cols, rows, "PTY spawned");

        Ok(Self {
            master_fd,
            _child: child,
        })
    }

    /// Create an async reader for the master side of the PTY.
    pub fn reader(&self) -> Result<PtyReader> {
        let fd = dup_fd(&self.master_fd)?;
        let async_fd = AsyncFd::new(RawOwnedFd(fd))?;
        Ok(PtyReader { inner: async_fd })
    }

    /// Create an async writer for the master side of the PTY.
    pub fn writer(&self) -> Result<PtyWriter> {
        let fd = dup_fd(&self.master_fd)?;
        let async_fd = AsyncFd::new(RawOwnedFd(fd))?;
        Ok(PtyWriter { inner: async_fd })
    }

    /// Return the raw file descriptor of the master PTY.
    /// Used for sending TIOCSWINSZ from the resize handler.
    pub fn master_raw_fd(&self) -> RawFd {
        self.master_fd.as_raw_fd()
    }

    /// Resize the PTY to the given dimensions.
    #[allow(dead_code)]
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        // SAFETY: TIOCSWINSZ is the standard ioctl for setting terminal size.
        let ret = unsafe {
            libc::ioctl(
                self.master_fd.as_raw_fd(),
                libc::TIOCSWINSZ,
                &ws as *const libc::winsize,
            )
        };
        if ret < 0 {
            return Err(PtyError::Resize(std::io::Error::last_os_error()));
        }

        tracing::debug!(cols, rows, "PTY resized");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Async reader
// ---------------------------------------------------------------------------

pub struct PtyReader {
    inner: AsyncFd<RawOwnedFd>,
}

impl AsyncRead for PtyReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            let mut guard = match self.inner.poll_read_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            let fd = self.inner.as_raw_fd();
            let unfilled = buf.initialize_unfilled();

            // SAFETY: valid fd and buffer.
            let n =
                unsafe { libc::read(fd, unfilled.as_mut_ptr().cast(), unfilled.len()) };

            if n >= 0 {
                buf.advance(n as usize);
                guard.clear_ready();
                return Poll::Ready(Ok(()));
            }

            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Poll::Ready(Err(err));
        }
    }
}

// ---------------------------------------------------------------------------
// Async writer
// ---------------------------------------------------------------------------

pub struct PtyWriter {
    inner: AsyncFd<RawOwnedFd>,
}

impl AsyncWrite for PtyWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        loop {
            let mut guard = match self.inner.poll_write_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };

            let fd = self.inner.as_raw_fd();

            // SAFETY: valid fd and buffer.
            let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };

            if n >= 0 {
                guard.clear_ready();
                return Poll::Ready(Ok(n as usize));
            }

            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                guard.clear_ready();
                continue;
            }
            return Poll::Ready(Err(err));
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn dup_fd(fd: &OwnedFd) -> Result<OwnedFd> {
    let new_fd = unsafe { libc::dup(fd.as_raw_fd()) };
    if new_fd < 0 {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(new_fd) })
}

fn set_nonblocking(fd: &OwnedFd) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    let ret = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if ret < 0 {
        return Err(PtyError::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_error_display_openpty() {
        let err = PtyError::Openpty(std::io::Error::new(std::io::ErrorKind::Other, "mock"));
        let msg = format!("{err}");
        assert!(msg.contains("failed to allocate PTY pair"));
        assert!(msg.contains("mock"));
    }

    #[test]
    fn pty_error_display_spawn() {
        let err = PtyError::Spawn(std::io::Error::new(std::io::ErrorKind::NotFound, "no shell"));
        let msg = format!("{err}");
        assert!(msg.contains("failed to spawn shell process"));
        assert!(msg.contains("no shell"));
    }

    #[test]
    fn pty_error_display_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe");
        let err = PtyError::Io(io_err);
        let msg = format!("{err}");
        assert!(msg.contains("I/O error"));
    }

    #[test]
    fn pty_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::TimedOut, "timeout");
        let err: PtyError = io_err.into();
        assert!(matches!(err, PtyError::Io(_)));
    }

    #[test]
    fn pty_error_debug() {
        let err = PtyError::Openpty(std::io::Error::new(std::io::ErrorKind::Other, "test"));
        let debug = format!("{err:?}");
        assert!(debug.contains("Openpty"));
    }
}

fn spawn_child(shell: &str, slave_fd: &OwnedFd, cols: u16, rows: u16) -> Result<Child> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let slave_raw = slave_fd.as_raw_fd();

    // Duplicate the slave fd for stdin/stdout/stderr.
    let stdin_fd = dup_fd(slave_fd)?;
    let stdout_fd = dup_fd(slave_fd)?;
    let stderr_fd = dup_fd(slave_fd)?;

    // Spawn as a login shell by prepending '-' to argv[0]
    let shell_basename = std::path::Path::new(shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(shell);
    let login_arg0 = format!("-{shell_basename}");

    let mut cmd = Command::new(shell);
    cmd.arg0(&login_arg0);
    cmd.stdin(Stdio::from(stdin_fd));
    cmd.stdout(Stdio::from(stdout_fd));
    cmd.stderr(Stdio::from(stderr_fd));

    // Set terminal type so programs know our capabilities.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("TERM_PROGRAM", "mado");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    cmd.env("COLUMNS", cols.to_string());
    cmd.env("LINES", rows.to_string());

    // SAFETY: pre_exec runs in the child after fork but before exec.
    unsafe {
        cmd.pre_exec(move || {
            // Create a new session so the child becomes the session leader.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }

            // Set the slave PTY as the controlling terminal.
            if libc::ioctl(slave_raw, libc::TIOCSCTTY.into(), 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(())
        });
    }

    let child = tokio::process::Command::from(cmd)
        .spawn()
        .map_err(PtyError::Spawn)?;

    Ok(child)
}
