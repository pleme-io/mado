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
use vigy::eval::{
    ExtArity, ExtEvalError, ExtInterpreter, ExtValue, ExtensionHandle, VigyHost, closure_extension,
    standard_extensions,
};
use vigy::runtime::Scope;
use vigy::types::Language;
use vigy::{RuntimeHandle, TickInterval, Vigy};

/// The name mado schedules under in the shared vigy store.
pub const VIGY_HOST: &str = "mado";
const BLUE_FILE_LABEL: &str = "blue-file";

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
    pub async fn start(config: &crate::config::MadoVigyConfig) -> anyhow::Result<Self> {
        let db = db_path()?;
        tracing::info!(?db, "starting embedded vigy runtime");

        // Open the runtime with mado-side intrinsics layered onto the
        // standard extension bundle. Programs running inside this
        // runtime can call (mado-version) / (mado-tear-attached?) /
        // (mado-pane-count) etc. as native lisp forms — no MCP / RPC
        // hop required.
        let mut extensions = standard_extensions();
        extensions.push(mado_intrinsics_extension());
        // mado is the primary vigy host: it schedules its own vigies
        // (`host=mado`, the heartbeat) and the unlabelled ones registered
        // from panes or the CLI, and leaves other hosts' (`host=arnes`) to
        // them. It still lists and inspects every vigy in the shared store.
        let rt = RuntimeHandle::open_with_scope(
            &db,
            extensions,
            Scope::HostAndUnlabelled(VIGY_HOST.into()),
        )
        .await
        .with_context(|| format!("open vigy runtime at {}", db.display()))?;

        // Register the default heartbeat (idempotent — same name+program
        // yields the same id, so this is a no-op on subsequent startups
        // unless the operator deleted it). A runtime that runs only for blue
        // vigies wakes nothing on its own.
        if config.enabled {
            let interval = TickInterval::from_millis(DEFAULT_HEARTBEAT_INTERVAL_MS)
                .expect("heartbeat interval is in range");
            let mut v = Vigy::new(DEFAULT_HEARTBEAT_NAME, DEFAULT_HEARTBEAT_PROGRAM, interval)?;
            v.labels.insert(vigy::runtime::HOST_LABEL, VIGY_HOST)?;
            v.labels.insert("kind", "heartbeat")?;
            if let Err(e) = rt.register_or_update(v).await {
                tracing::warn!(err = %e, "could not register mado heartbeat vigy");
            }
        }

        let host = Self { rt: Arc::new(rt) };
        if config.blue {
            host.sync_blue_files(config.tick_ms).await;
        }
        Ok(host)
    }

    /// Make the store hold exactly the `.b` files of the vigies directory: a
    /// new or changed file registers, a removed or off one leaves.
    pub async fn sync_blue_files(&self, tick_ms: u64) {
        let Some(dir) = crate::world::vigies_dir() else {
            return;
        };
        let overlay = crate::world::installed();
        let wanted = blue_vigies(&dir, tick_ms, &overlay.vigies, &overlay.off_vigies);
        let mut kept = Vec::new();
        for vigy in wanted {
            kept.push(vigy.id.clone());
            if let Err(e) = self.rt.register_or_update(vigy).await {
                tracing::warn!(err = %e, "could not register a blue vigy");
            }
        }
        let selector = format!("{BLUE_FILE_LABEL}=true");
        if let Ok(present) = self.rt.list(Some(&selector)).await {
            for vigy in present {
                if !kept.contains(&vigy.id) {
                    if let Err(e) = self.rt.delete(&vigy.id).await {
                        tracing::warn!(err = %e, "could not remove a stale blue vigy");
                    }
                }
            }
        }
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

/// Resolve the vigy SQLite DB path with three precedence levels:
///
///   1. `MADO_VIGY_DB` env var (explicit mado-specific override)
///   2. `VIGY_DB` env var (matches what `vigy` CLI honors)
///   3. `~/.local/share/vigy/vigy.db` (the canonical vigy default)
///
/// Defaulting to vigy's canonical path — not a mado-private path —
/// means a vigy registered via `vigy register` from any pane is
/// visible to mado's embedded runtime, and vice versa. Single
/// source of truth across the operator's whole workstation.
fn db_path() -> anyhow::Result<PathBuf> {
    if let Ok(p) = std::env::var("MADO_VIGY_DB") {
        return Ok(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("VIGY_DB") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME").context("HOME unset")?;
    let mut p = PathBuf::from(home);
    p.push(".local");
    p.push("share");
    p.push("vigy");
    p.push("vigy.db");
    Ok(p)
}

/// Lazy singleton so the host can be reached from any module after
/// startup without re-plumbing state through every constructor.
static HOST: OnceCell<MadoVigyHost> = OnceCell::const_new();

/// Initialise the global vigy host. Idempotent — subsequent calls
/// return the existing host.
pub async fn init() -> anyhow::Result<MadoVigyHost> {
    init_with(&crate::config::MadoVigyConfig {
        enabled: true,
        blue: false,
        ..crate::config::MadoVigyConfig::default()
    })
    .await
}

pub async fn init_with(config: &crate::config::MadoVigyConfig) -> anyhow::Result<MadoVigyHost> {
    let host = HOST
        .get_or_try_init(|| async { MadoVigyHost::start(config).await })
        .await
        .cloned()?;
    let _ = TICK_MS.set(config.tick_ms);
    Ok(host)
}

static TICK_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// Bring the store in line with the vigies directory after a save. A runtime
/// that never started stays stopped until the next launch.
pub fn sync_soon() {
    let (Some(host), Some(tick_ms)) = (get(), TICK_MS.get().copied()) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("mado-vigy-sync".to_owned())
        .spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(host.sync_blue_files(tick_ms));
            }
        });
}

/// Whether this GUI starts the runtime: asked for by name, or because a blue
/// vigy waits on disk.
#[must_use]
pub fn should_start(config: &crate::config::MadoVigyConfig) -> bool {
    config.enabled
        || (config.blue
            && crate::world::vigies_dir().is_some_and(|dir| !blue_files(&dir).is_empty()))
}

fn blue_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "b"))
        .collect();
    files.sort();
    files
}

/// Each `.b` file as a vigy, ticked as `every` says or else every `tick_ms`.
fn blue_vigies(
    dir: &std::path::Path,
    tick_ms: u64,
    every: &std::collections::HashMap<String, u64>,
    off: &[String],
) -> Vec<Vigy> {
    let mut vigies = Vec::new();
    for file in blue_files(dir) {
        let Some(name) = file.file_stem().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        if off.contains(&name) {
            continue;
        }
        let ms = every
            .get(&name)
            .copied()
            .filter(|ms| *ms > 0)
            .unwrap_or(tick_ms);
        let built = std::fs::read_to_string(&file)
            .map_err(anyhow::Error::from)
            .and_then(|program| {
                let interval = TickInterval::from_millis(ms).map_err(anyhow::Error::from)?;
                let mut v = Vigy::in_language(name.clone(), program, Language::Blue, interval)?;
                v.labels.insert(vigy::runtime::HOST_LABEL, VIGY_HOST)?;
                v.labels.insert(BLUE_FILE_LABEL, "true")?;
                Ok(v)
            });
        match built {
            Ok(v) => vigies.push(v),
            Err(e) => tracing::warn!(file = %file.display(), err = %e, "a blue vigy was refused"),
        }
    }
    vigies
}

/// Read-only handle to the global vigy host, if it's been initialised.
pub fn get() -> Option<MadoVigyHost> {
    HOST.get().cloned()
}

// ─────────────────────────────────────────────────────────────────
// MadoIntrinsicsExtension — mado-side host intrinsics
// ─────────────────────────────────────────────────────────────────
//
// Exposes mado runtime state to vigy programs through tatara-lisp
// intrinsics. A vigy program can call these directly:
//
//   (vigy-log "info" (string-append "mado " (mado-version)))
//   (if (mado-tear-attached?)
//       (vigy-pull "tear-sessions")
//       (vigy-defer "tear not attached"))
//
// The trait surface keeps this clean: mado defines its intrinsics in
// one place (this function), passes them as a HostExtension when
// constructing the runtime, and vigy programs see them as if they
// were built into tatara-lisp.
//
// Pattern for adding more: just register more callables in the
// closure. No vigy-eval changes needed.

fn mado_intrinsics_extension() -> ExtensionHandle {
    closure_extension(|interp: &mut ExtInterpreter| {
        // (mado-board "key" "title") → the row on this process's Ctrl-S board;
        // the same key updates it, so a vigy shows its latest result every tick.
        both(
            interp,
            "mado-board",
            ExtArity::Exact(2),
            |args: &[ExtValue], _host: &mut VigyHost, sp| {
                let key = lisp_string(&args[0], sp)?;
                let title = lisp_string(&args[1], sp)?;
                crate::world::board_row(key, title);
                Ok(ExtValue::Nil)
            },
        );

        // (mado-version) → string
        // The mado binary's CARGO_PKG_VERSION. Useful for vigies that
        // need to gate behavior on mado version (e.g. "if mado >= 0.2
        // assume the new OSC 1338 path exists").
        both(
            interp,
            "mado-version",
            ExtArity::Exact(0),
            |_args: &[ExtValue], _host: &mut VigyHost, _sp| {
                Ok(ExtValue::Str(std::sync::Arc::from(env!(
                    "CARGO_PKG_VERSION"
                ))))
            },
        );

        // (mado-host) → string
        // The hostname mado is running on (best-effort via `hostname`
        // crate). Reconcilers running across a heterogeneous fleet
        // use this to gate host-specific behavior.
        both(
            interp,
            "mado-host",
            ExtArity::Exact(0),
            |_args: &[ExtValue], _host: &mut VigyHost, _sp| {
                let h = crate::host::short_hostname();
                Ok(ExtValue::Str(std::sync::Arc::from(h.as_str())))
            },
        );

        // (mado-process-uptime-ms) → int
        // Milliseconds since the mado process started. Lets vigies
        // implement "wait until mado has been up for at least N ms
        // before doing anything" patterns without rolling their own
        // start-time bookkeeping.
        both(
            interp,
            "mado-process-uptime-ms",
            ExtArity::Exact(0),
            |_args: &[ExtValue], _host: &mut VigyHost, _sp| {
                let ms = MADO_START
                    .get_or_init(|| std::time::Instant::now())
                    .elapsed()
                    .as_millis() as i64;
                Ok(ExtValue::Int(ms))
            },
        );

        // (mado-tear-attached?) → bool
        // Whether mado has an active tear-daemon attachment right
        // now. Vigies that sync tear state should defer when this is
        // false. Reads through tear-discovery's cached state — cheap.
        both(
            interp,
            "mado-tear-attached?",
            ExtArity::Exact(0),
            |_args: &[ExtValue], _host: &mut VigyHost, _sp| {
                Ok(ExtValue::Bool(probe_tear_attached()))
            },
        );

        // (mado-emit-event "kind" "message")
        // Sugar around tracing::info! tagged with `source = "mado-vigy"`
        // so operators tailing mado logs can filter just the events
        // that originated from vigy programs.
        both(
            interp,
            "mado-emit-event",
            ExtArity::Exact(2),
            |args: &[ExtValue], _host: &mut VigyHost, sp| {
                let kind = lisp_string(&args[0], sp)?;
                let message = lisp_string(&args[1], sp)?;
                tracing::info!(
                    source = "mado-vigy",
                    kind = %kind,
                    message = %message,
                    "mado-vigy event",
                );
                Ok(ExtValue::Nil)
            },
        );

        // ── Floating-browser control (mado-memory-privileged: needs mado's
        // live in-process float/window state; cannot be done in the shell).
        // Each mutating verb queues a BrowserVerb on the process-global
        // browser bridge; it returns `true` only when a live GUI float loop
        // is draining the sink (else `false` — the no-injection-sink honesty
        // gate, never a silent success). The bridge is installed only in the
        // GUI process, so a vigy in the `mado mcp` process honestly gets
        // `false`. ──

        // (mado-browser-open "https://…") → bool (queued?)
        both(
            interp,
            "mado-browser-open",
            ExtArity::Exact(1),
            |args: &[ExtValue], _host: &mut VigyHost, sp| {
                let url = lisp_string(&args[0], sp)?;
                let ok = crate::browser_bridge::get()
                    .map(|b| b.open(&url))
                    .unwrap_or(false);
                Ok(ExtValue::Bool(ok))
            },
        );

        // (mado-browser-navigate id "url") → bool
        both(
            interp,
            "mado-browser-navigate",
            ExtArity::Exact(2),
            |args: &[ExtValue], _host: &mut VigyHost, sp| {
                let id = lisp_int(&args[0], sp)?;
                let url = lisp_string(&args[1], sp)?;
                let ok = crate::browser_bridge::get()
                    .map(|b| b.navigate(mado::float::BrowserId(id as u32), &url))
                    .unwrap_or(false);
                Ok(ExtValue::Bool(ok))
            },
        );

        // (mado-browser-snap id :left-half) → bool
        both(
            interp,
            "mado-browser-snap",
            ExtArity::Exact(2),
            |args: &[ExtValue], _host: &mut VigyHost, sp| {
                let id = lisp_int(&args[0], sp)?;
                let zone = lisp_string(&args[1], sp)?;
                let ok = crate::browser_bridge::get()
                    .map(|b| b.snap(mado::float::BrowserId(id as u32), &zone))
                    .unwrap_or(false);
                Ok(ExtValue::Bool(ok))
            },
        );

        // (mado-browser-focus id) → bool
        both(
            interp,
            "mado-browser-focus",
            ExtArity::Exact(1),
            |args: &[ExtValue], _host: &mut VigyHost, sp| {
                let id = lisp_int(&args[0], sp)?;
                let ok = crate::browser_bridge::get()
                    .map(|b| b.focus(mado::float::BrowserId(id as u32)))
                    .unwrap_or(false);
                Ok(ExtValue::Bool(ok))
            },
        );

        // (mado-browser-close id) → bool
        both(
            interp,
            "mado-browser-close",
            ExtArity::Exact(1),
            |args: &[ExtValue], _host: &mut VigyHost, sp| {
                let id = lisp_int(&args[0], sp)?;
                let ok = crate::browser_bridge::get()
                    .map(|b| b.close(mado::float::BrowserId(id as u32)))
                    .unwrap_or(false);
                Ok(ExtValue::Bool(ok))
            },
        );

        // (mado-browser-set-dom id "sexp") → bool. The WRITE side of the DOM-Way
        // loop (pairs with mado-browser-dom-sexp's read): replace the surface's
        // DOM from a tatara-lisp S-expression — read the DOM as lisp, rewrite it,
        // push it back live. Inline `<lisp>` in the sexp is expanded. Validates
        // the sexp first; a malformed sexp returns false, never a silent bad
        // render. Needs mado's live in-process page state (mado-memory-
        // privileged), so it is a mado intrinsic, not shell.
        both(
            interp,
            "mado-browser-set-dom",
            ExtArity::Exact(2),
            |args: &[ExtValue], _host: &mut VigyHost, sp| {
                let id = lisp_int(&args[0], sp)?;
                let sexp = lisp_string(&args[1], sp)?;
                if nami_core::lisp::sexp_to_dom(&sexp).is_err() {
                    return Ok(ExtValue::Bool(false));
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let ok = crate::browser_bridge::get()
                    .map(|b| b.set_dom(mado::float::BrowserId(id as u32), &sexp))
                    .unwrap_or(false);
                Ok(ExtValue::Bool(ok))
            },
        );

        // (mado-browser-list) → JSON string of live float surfaces
        // (ExtValue has no list variant, so the read snapshot is JSON —
        // the same shape browser_list returns over MCP).
        both(
            interp,
            "mado-browser-list",
            ExtArity::Exact(0),
            |_args: &[ExtValue], _host: &mut VigyHost, _sp| {
                let surfaces = crate::browser_bridge::get()
                    .map(crate::browser_bridge::BrowserBridge::surfaces)
                    .unwrap_or_default();
                let json = serde_json::to_string(&surfaces).unwrap_or_else(|_| String::from("[]"));
                Ok(ExtValue::Str(std::sync::Arc::from(json.as_str())))
            },
        );

        // (mado-browser-dom-sexp id) → the page DOM as a homoiconic tatara-lisp
        // S-expression string (nami-core dom_to_sexp). "The DOM Way of the
        // Browser": the live page tree, queryable as lisp data — the read side
        // of the DOM-manipulation surface. Needs mado's live in-process page
        // state (mado-memory-privileged), so it is a mado intrinsic, not shell.
        both(
            interp,
            "mado-browser-dom-sexp",
            ExtArity::Exact(1),
            |args: &[ExtValue], _host: &mut VigyHost, sp| {
                let id = lisp_int(&args[0], sp)?;
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let target = id as u32;
                let sexp = crate::browser_bridge::get()
                    .and_then(|b| {
                        b.surfaces()
                            .into_iter()
                            .find(|s| s.id == target)
                            .map(|s| s.dom_sexp)
                    })
                    .unwrap_or_default();
                Ok(ExtValue::Str(std::sync::Arc::from(sexp.as_str())))
            },
        );
    })
}

/// One intrinsic under its lisp name and under the spelling blue can write:
/// blue has no `-` in a name.
fn both(
    interp: &mut ExtInterpreter,
    lisp: &'static str,
    arity: ExtArity,
    run: fn(&[ExtValue], &mut VigyHost, vigy::eval::Span) -> Result<ExtValue, ExtEvalError>,
) {
    interp.register_fn(lisp, arity, run);
    interp.register_fn(lisp.replace('-', "_"), arity, run);
}

fn probe_tear_attached() -> bool {
    // Best-effort: try a tear-discovery probe. The discover() helper
    // is sync + cheap (reads a config snapshot + checks socket path).
    let cfg = crate::config::load(&None).unwrap_or_default().tear;
    matches!(
        crate::tear_discovery::discover(&cfg),
        crate::tear_discovery::DiscoveryOutcome::Attached(_, _)
    )
}

fn lisp_string(v: &ExtValue, sp: vigy::eval::Span) -> std::result::Result<String, ExtEvalError> {
    match v {
        ExtValue::Str(s) => Ok(s.to_string()),
        ExtValue::Symbol(s) => Ok(s.to_string()),
        ExtValue::Keyword(s) => Ok(s.to_string()),
        other => Err(ExtEvalError::type_mismatch(
            "string|symbol|keyword",
            other.type_name(),
            sp,
        )),
    }
}

/// Extract an `i64` from a lisp arg (browser-surface ids arrive as ints).
fn lisp_int(v: &ExtValue, sp: vigy::eval::Span) -> std::result::Result<i64, ExtEvalError> {
    match v {
        ExtValue::Int(i) => Ok(*i),
        other => Err(ExtEvalError::type_mismatch("int", other.type_name(), sp)),
    }
}

/// Process start time — lazily initialised on first call to
/// `(mado-process-uptime-ms)`. `OnceCell` so concurrent vigy ticks
/// agree on a single anchor.
static MADO_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

#[cfg(test)]
mod tests {
    use super::*;
    use vigy::eval::{VigyHost, evaluate_in};

    fn blue_extensions() -> Vec<ExtensionHandle> {
        let mut extensions = standard_extensions();
        extensions.push(mado_intrinsics_extension());
        extensions
    }

    #[test]
    fn every_intrinsic_has_a_name_blue_can_write() {
        let program = "x = mado_version()\ny = mado_host()\nz = mado_process_uptime_ms()\nvigy_log(\"info\", \"#{x} #{y} #{z}\")\nvigy_noop()\n";
        let host = evaluate_in(
            Language::Blue,
            program,
            VigyHost::default(),
            &blue_extensions(),
        )
        .unwrap();
        assert_eq!(host.actions.len(), 1);
        assert!(host.log[0].message.contains(env!("CARGO_PKG_VERSION")));
    }

    #[tokio::test]
    async fn a_blue_file_ticks_and_its_row_reaches_the_board() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("clock.b"),
            "n = vigy_tick_count\nmado_board(\"clock\", \"blue tick #{n}\")\nvigy_noop()\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("note.txt"), "not a vigy").unwrap();
        let every = std::collections::HashMap::from([("clock".to_owned(), 250)]);
        let vigies = blue_vigies(dir.path(), 5_000, &every, &[]);
        assert_eq!(vigies.len(), 1);
        assert_eq!(vigies[0].language, Language::Blue);
        assert_eq!(vigies[0].tick_interval.as_millis(), 250);
        let db = dir.path().join("vigy.db");
        let rt = RuntimeHandle::open_with_scope(
            &db,
            blue_extensions(),
            Scope::HostAndUnlabelled(VIGY_HOST.into()),
        )
        .await
        .unwrap();
        let vigy = rt.register_or_update(vigies[0].clone()).await.unwrap();
        let first = rt.tick_now(&vigy.id).await.unwrap();
        assert!(first.error.is_none(), "{first:?}");
        let titles = || -> Vec<String> {
            crate::suggest::store()
                .ranked_stored(1_000, crate::suggest::now_unix_ms())
                .iter()
                .filter(|s| s.item.title.starts_with("blue tick"))
                .map(|s| s.item.title.clone())
                .collect()
        };
        let first_titles = titles();
        assert_eq!(first_titles.len(), 1, "{first_titles:?}");
        let second = rt.tick_now(&vigy.id).await.unwrap();
        assert!(second.error.is_none(), "{second:?}");
        let second_titles = titles();
        assert_eq!(second_titles.len(), 1, "{second_titles:?}");
        assert_ne!(
            second_titles, first_titles,
            "a tick must update the surface"
        );
    }

    #[test]
    fn with_no_file_the_runtime_stays_off_and_the_switches_are_honest() {
        let dir = tempfile::tempdir().unwrap();
        assert!(blue_files(dir.path()).is_empty());
        std::fs::write(dir.path().join("a.b"), "vigy_noop()\n").unwrap();
        assert_eq!(blue_files(dir.path()).len(), 1);
        assert!(blue_vigies(dir.path(), 1000, &Default::default(), &["a".to_owned()]).is_empty());
        let off = crate::config::MadoVigyConfig {
            blue: false,
            ..crate::config::MadoVigyConfig::default()
        };
        assert!(!should_start(&off));
        let default = crate::config::MadoVigyConfig::default();
        assert!(!default.enabled && default.blue);
    }
}
