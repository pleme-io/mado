//! In-process vigy host for mado.
//!
//! Embeds the vigy reconciler runtime so apps living inside mado's
//! panes can register tatara-lisp reconcilers at runtime via MCP
//! (and, eventually, an OSC 1338 extension) without needing an
//! external daemon. The runtime ticks each registered vigy on its
//! own tokio task; mado's main loop never sees the work.
//!
//! Lifecycle:
//!   1. On mado startup we call [`MadoVigyHost::start`] from within
//!      the existing tokio runtime context.
//!   2. The host opens a SQLite DB at `~/.local/share/mado/vigy.db`
//!      (override with `MADO_VIGY_DB`).
//!   3. It registers a small "heartbeat" vigy by default so operators
//!      can confirm the runtime is alive even before they author
//!      their own reconcilers. The heartbeat is enabled-by-default
//!      but can be disabled via `mado vigy disable <id>` once
//!      operators have their own vigies in place.
//!   4. MCP tools (registered in `crate::mcp`) and any future OSC
//!      handler route through [`MadoVigyHost::dispatch`].
//!
//! Crash-resilience: if vigy startup fails (e.g. disk full), mado
//! logs a warning and continues. The terminal stays usable; the
//! reconciler runtime is purely additive.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::OnceCell;
use vigy::{RuntimeHandle, TickInterval, Vigy};

const DEFAULT_HEARTBEAT_NAME: &str = "mado-heartbeat";
const DEFAULT_HEARTBEAT_PROGRAM: &str = r#"
;; mado-heartbeat — proves the vigy runtime is alive inside mado.
;; Emits a noop reconcile action every tick + an info log line.
;; Operators can disable this with: vigy disable <id>
(vigy-log "info" "mado vigy heartbeat tick")
(vigy-noop)
"#;
const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 10_000;

/// One-process-wide vigy host. Cloneable handle; the underlying
/// runtime is reference-counted.
#[derive(Clone)]
pub struct MadoVigyHost {
    rt: Arc<RuntimeHandle>,
}

impl MadoVigyHost {
    /// Start the vigy runtime + register the default heartbeat.
    ///
    /// Must be called from within a tokio runtime. Returns
    /// `Ok(None)` if vigy initialization failed — mado keeps running
    /// without the reconciler runtime in that case (best-effort
    /// additive primitive).
    pub async fn start() -> anyhow::Result<Self> {
        let db = db_path()?;
        tracing::info!(?db, "starting embedded vigy runtime");
        let rt = RuntimeHandle::open(&db)
            .await
            .with_context(|| format!("open vigy runtime at {}", db.display()))?;

        // Register the default heartbeat (idempotent — same name+program
        // yields the same id, so this is a no-op on subsequent startups
        // unless the operator deleted it).
        let interval = TickInterval::from_millis(DEFAULT_HEARTBEAT_INTERVAL_MS)
            .expect("heartbeat interval is in range");
        let mut v = Vigy::new(
            DEFAULT_HEARTBEAT_NAME,
            DEFAULT_HEARTBEAT_PROGRAM,
            interval,
        )?;
        v.labels.insert("host", "mado")?;
        v.labels.insert("kind", "heartbeat")?;
        if let Err(e) = rt.register_or_update(v).await {
            tracing::warn!(err = %e, "could not register mado heartbeat vigy");
        }

        Ok(Self { rt: Arc::new(rt) })
    }

    /// Bare runtime handle for callers that want direct access (e.g.
    /// the OSC handler that will land in a follow-up).
    pub fn runtime(&self) -> Arc<RuntimeHandle> {
        self.rt.clone()
    }

    /// MCP dispatch shim — proxies tool calls to `vigy::mcp::dispatch`.
    /// Used by `crate::mcp` to expose vigy operations as kaname tools.
    pub async fn dispatch(
        &self,
        tool: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        vigy::mcp::dispatch(self.rt.as_ref(), tool, args).await
    }

    /// The full MCP tool catalog for vigy. Re-exported here so the
    /// kaname tool router can register it without depending on vigy
    /// directly.
    pub fn tool_catalog() -> Vec<vigy::mcp::ToolEntry> {
        vigy::mcp::tool_catalog()
    }
}

fn db_path() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("MADO_VIGY_DB") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME").context("HOME unset")?;
    let mut p = PathBuf::from(home);
    p.push(".local");
    p.push("share");
    p.push("mado");
    p.push("vigy.db");
    Ok(p)
}

/// Lazy singleton so the host can be reached from any module after
/// startup without re-plumbing state through every constructor.
static HOST: OnceCell<MadoVigyHost> = OnceCell::const_new();

/// Initialise the global vigy host. Idempotent — subsequent calls
/// return the existing host.
pub async fn init() -> anyhow::Result<MadoVigyHost> {
    HOST.get_or_try_init(|| async { MadoVigyHost::start().await })
        .await
        .cloned()
}

/// Read-only handle to the global vigy host, if it's been initialised.
pub fn get() -> Option<MadoVigyHost> {
    HOST.get().cloned()
}
