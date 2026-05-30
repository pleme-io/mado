//! `MadoAppState` — the aggregator the kanshou server exposes.
//!
//! Reads the live atomics in `crate::render` and the session
//! registry the GUI mado actually populated. The MCP server in
//! `mado mcp` connects to this socket and forwards every
//! introspection query through it — closing the "MCP returns
//! process-local zeros while the GUI renders" class structurally.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use kanshou::{Introspect, Query, QueryError, QueryResult};

/// Live aggregator. Hand-implements [`Introspect`] because the
/// leaves are static atomics + a session-registry accessor — the
/// derive macro doesn't see static counters or arbitrary `&self`
/// methods.
///
/// Add new queryable surface by extending the match arms below + the
/// `schema` array. Each leaf is one `serde_json::json!` line; no
/// boilerplate.
pub struct MadoAppState {
    /// Snapshot of the config the GUI actually loaded. Cloned at
    /// startup; queries over this never read the on-disk file.
    pub config: Arc<crate::config::MadoConfig>,
    /// Session registry the MCP tools populate. Same `Arc` the GUI
    /// holds; live reads.
    pub sessions: Arc<crate::session::SessionRegistry>,
}

impl MadoAppState {
    #[must_use]
    pub fn new(
        config: Arc<crate::config::MadoConfig>,
        sessions: Arc<crate::session::SessionRegistry>,
    ) -> Self {
        Self { config, sessions }
    }
}

impl Introspect for MadoAppState {
    fn query(&self, q: &Query) -> QueryResult {
        let Some(first) = q.path.first().map(String::as_str) else {
            return Err(QueryError::unknown_field(String::new()));
        };
        match first {
            "frame_perf" => Ok(serde_json::json!({
                "last_frame_us": crate::render::LAST_FRAME_US.load(Ordering::Relaxed),
                "last_frame_rects": crate::render::LAST_FRAME_RECTS.load(Ordering::Relaxed),
                "last_frame_text": crate::render::LAST_FRAME_TEXT.load(Ordering::Relaxed),
                "last_frame_shape_cache": crate::render::LAST_FRAME_SHAPE_CACHE.load(Ordering::Relaxed),
                "total_frames": crate::render::TOTAL_FRAMES.load(Ordering::Relaxed),
                "total_frames_skipped": crate::render::TOTAL_FRAMES_SKIPPED.load(Ordering::Relaxed),
            })),
            "sessions" => {
                let summaries = self.sessions.list();
                Ok(serde_json::json!({
                    "count": summaries.len(),
                    "sessions": summaries,
                }))
            }
            "config" => serde_json::to_value(&*self.config).map_err(|e| {
                QueryError::internal(format!("serialize MadoConfig: {e}"))
            }),
            "process" => Ok(serde_json::json!({
                "pid": std::process::id(),
                "binary": std::env::current_exe()
                    .ok()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                "version": env!("CARGO_PKG_VERSION"),
            })),
            other => Err(QueryError::unknown_field(other.to_string())),
        }
    }

    fn schema(&self) -> &'static [&'static str] {
        &["frame_perf", "sessions", "config", "process"]
    }
}

/// Spawn the kanshou server in a tokio task. Returns the path the
/// server bound to so the caller can log it. The task is detached;
/// dropping it shuts the server (and removes the socket file).
///
/// `app_name` is the canonical wire identifier — operator tools use
/// it to filter discovery. Pass `"mado"` for the GUI process.
pub fn spawn_server(
    app_name: &str,
    state: Arc<MadoAppState>,
) -> std::io::Result<std::path::PathBuf> {
    let server = kanshou::Server::new(app_name, state)?;
    let socket_path = server.socket_path().to_path_buf();
    tokio::spawn(async move {
        if let Err(e) = server.serve().await {
            tracing::warn!(error = ?e, "mado kanshou server exited with error");
        }
    });
    Ok(socket_path)
}
