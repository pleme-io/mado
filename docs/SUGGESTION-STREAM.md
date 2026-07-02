# The Suggestion Stream — continuous task-flow dominance in the Ctrl-S picker

> **Destination first** (Operating Principle #0). Open Ctrl-S any time and the
> lower band of the session picker is a **living, gently self-refreshing stack
> of "what you could start working on right now"** — a PR awaiting your review,
> a sprint ticket, a failing Flux/k8s resource, a dirty repo, a Cursor agent
> needing follow-up, an incident firing. Each row is ranked by urgency + score,
> each **Enter-spawns a pre-named session aimed at that task**. Sources are
> shikumi config; adding one is a single `SuggestionSource` impl. Nothing dead
> or duplicate is ever offered. The terminal proposes your next task before you
> go looking — *flow dominance*.

This is **not** a new daemon or a new picker. mado already embeds **vigy** (a
tatara-lisp reconciler runtime with parallel watchers + in-memory events +
lazy SQLite persistence) and the Ctrl-S picker already renders latent **○**
rows. The suggestion stream is a typed plane that parallel watchers feed into
rows the picker already knows how to draw.

## Architecture (reuse-first, no-overlap layering)

```
                      ┌─────────────────────────────────────────────┐
   external state ──▶ │ SuggestionSource::poll(&dyn Environment,cfg) │  one impl per SourceKind
 (gh / git / kubectl  └───────────────┬─────────────────────────────┘
  / flux / HTTP / fs)                 │ Vec<Suggestion>   (pure; tested via MockEnvironment)
                                      ▼
   SuggestionEngine (one tokio watcher task per enabled source, own runtime thread like vigy)
                                      │ store.ingest(source, items, now)
                                      ▼
   SuggestionStore  (ephemeral, ranked, decaying; SEPARATE from persisted presets;
                     per-source ingest preserves first_seen → shade-in continuity)
                                      │ ranked_stored(max)
                                      ▼
   PracaPickerBridge.list()  →  ○ RowKind::Suggestion rows BELOW live ● + preset ○ rows
                                      │ Enter
                                      ▼
   bridge.spawn_suggestion(id)  →  spawn a session at the suggestion's cwd + name → switch
```

| Layer | Type / file | Notes |
|---|---|---|
| Data model | `suggest::core` — `Suggestion`, `SourceKind` (27-variant catalog), `SpawnSpec`, `Urgency`, `SuggestionId` | `SpawnSpec::new` rejects empty cwd/name → an un-actionable suggestion is **unrepresentable**. The 27-source catalog is CATALOG REFLECTION (exhaustive `match` per method). |
| I/O boundary | `suggest::env` — `SuggestionEnvironment` (`RealEnvironment` / `MockEnvironment`) | The TYPED-SPEC+INTERPRETER triplet's `Environment`. Real = typed `Command` + `curl` HTTP + fs + sops secret (NO shell). Tests = canned fixtures. Every method returns `Option`/empty (graceful, never panics). |
| Store | `suggest::store` — `SuggestionStore` | Ephemeral ranked + decaying; per-source `ingest` replaces only that source's slice, preserving `first_seen_ms`. JSON snapshot for warm restart. **Never** touches the persisted preset catalog (`PracaSnapshot.definitions`). |
| Watcher plane | `suggest::source` — `SuggestionSource` trait + `SuggestionEngine` + `refresh_once` | One tokio task per enabled source, each on its own cadence, polling on the blocking pool (subprocess-safe). `refresh_once` is the single tested unit. |
| Providers | `suggest::sources::*` — one file per `SourceKind` | Each a pure `poll` + a free `parse` fn + `#[cfg(test)]` mock tests. |
| View | `session_picker::RowKind::Suggestion` + `PracaPickerBridge` | `list()` appends query-filtered, ranked ○ suggestion rows below sessions/presets (the union-navigator law). `spawn_suggestion` is the accept path. |
| Config | `config::SuggestionsConfig` (shikumi-tiered) | bare = OFF (stripped); prescribed = ON, gentle cadence, all implemented sources at defaults; per-source overrides by kebab slug. |

## Source catalog (27)

Tiered by default urgency / flow value. Each maps to one `SuggestionSource`.
`needs_auth` sources contribute nothing until their token/param is present
(graceful, never an error).

| Slug | Yields | Fetch | Auth |
|---|---|---|---|
| `git-branch-pr` | your open PRs | `gh search prs --author=@me` | gh |
| `github-review-requested` | PRs awaiting your review | `gh search prs --review-requested=@me` | gh |
| `github-assigned-issues` | issues assigned to you | `gh search issues --assignee=@me` | gh |
| `github-actions-failing` | failing CI runs (cwd repo) | `gh run list --status=failure` | gh |
| `tend-repos` | dirty / missing workspace repos | `tend status --json` | — |
| `recent-dirs` | recently-visited dirs | `~/.local/share/mado/recent_dirs` | — |
| `project-marks` | your project marks | `~/.local/share/mado/marks` | — |
| `cargo-warnings` | cargo warnings in cwd | `cargo check --message-format short` | — |
| `todo-backlog` | TODO/FIXME under code root | `rg -e TODO -e FIXME` | — |
| `jira-sprint` | sprint issues assigned to you | Jira REST `/search` (JQL) | atlassian/api-token + base_url/email |
| `jira-assigned` | all issues assigned to you | Jira REST `/search` | atlassian |
| `confluence-mentions` | pages mentioning you | Confluence REST `/search` (CQL) | atlassian |
| `flux-failing` | Flux Kustomizations/HelmReleases not Ready | `kubectl get kustomizations,helmreleases -A -o json` | kubeconfig |
| `k8s-unhealthy` | Pending / CrashLoop pods | `kubectl get pods -A -o json` | kubeconfig |
| `breathe-conflict` | breathe bands in Conflict | `kubectl get breathebands -A -o json` | kubeconfig |
| `engenho-nodes` | nodes not Ready | `kubectl get nodes -o json` | kubeconfig |
| `grafana-alerts` | firing alerts | Grafana `/api/prometheus/.../alerts` | grafana/api-token + base_url |
| `grafana-incidents` | open incidents | Grafana annotations | grafana |
| `grafana-oncall` | your on-call shifts | Grafana OnCall `/shifts` | grafana/oncall-token |
| `datadog-monitors` | alerting monitors | Datadog `/api/v1/monitor` | datadog/api-key+app-key |
| `opsgenie-alerts` | open alerts | Opsgenie `/v2/alerts` | opsgenie/api-key |
| `kurage-agents` | Cursor agents needing follow-up | `kurage list-agents --json` | kurage |
| `aws-health` | open AWS health events | `aws health describe-events` | aws cli |
| `cloudflare-deployments` | failed Pages deployments | Cloudflare API | cloudflare/api-token |
| `google-tasks` | tasks due | Google Tasks API | google/tasks-token |
| `google-calendar` | imminent events | Google Calendar API | google/calendar-token |
| `secret-age` | stale secrets to rotate | `find ~/.config -mtime +90` | — |

## Config (shikumi-tiered)

```yaml
suggestions:
  enabled: true            # master switch (bare tier = false)
  default_enabled: true    # run a source with no explicit override
  max_visible: 6           # rows shown in the picker
  per_source_cap: 3        # max rows one source may contribute (band diversity; 0 = no cap)
  shade_in_ms: 600         # per-frame fade-in duration
  ttl_secs: 900            # decay an unseen suggestion after this
  sources:
    - kind: jira-sprint
      enabled: true
      interval_secs: 300
      max_items: 5
      params: { base_url: "https://acme.atlassian.net", email: "me@acme.com" }
    - kind: aws-health
      enabled: false       # opt a heavy source out
```

`MADO_TIER=bare` strips the whole stream. Per-source secrets come from the
sops-rendered `~/.config/<category>/<name>` path (e.g. `atlassian/api-token`).

## Phased plan + tier ledger

| Phase | Scope | State |
|---|---|---|
| **M0** | substrate (core/env/store/source) + 27-source catalog + 2 providers + `RowKind::Suggestion` + bridge list/spawn + `SuggestionsConfig` + engine bootstrap | ✅ shipped |
| **M-bulk** | all 27 providers, each parser-tested via `MockEnvironment` | ✅ shipped |
| **M1a** | refresh-while-open — event-driven `watch`-broadcast re-list while resting (livestream `Reactive`), so the band updates the moment a source writes | ✅ shipped |
| **M1b** | per-frame shade-in (alpha ramp) + urgency tint (`Urgency::tint`) | ✅ shipped |
| **Kickoff** | Enter runs the task (`send_keys`) — land in the repo AND run the command | ✅ shipped |
| **Dedup** | idempotent accept + live-band suppression (`live_session_for`) — nothing duplicate offered | ✅ shipped |
| **Diversity** | `balance_per_source` + `per_source_cap` — one noisy source can't drown the band | ✅ shipped |
| **Freshness** | `util::relative_age` nudge on tasks idle ≥ 5m | ✅ shipped |
| **Testing** | property invariants (pct round-trip, rank-order, balance bounds, urgency dominance, spawnspec) | ✅ shipped |
| **Persistence** | warm-restart load + crash-safe atomic write (mkdir-p + pid-temp + `sync_all` + rename) + BLAKE3-framed/versioned snapshot (torn/schema-bump → start-empty) + a single debounced maintenance task (one disk write per `persist_debounce_secs`, gated on a change-`generation`, off the watcher hot path) + decay moved to that task | ✅ shipped |
| **M2** | ✅ typed nix surface for `suggestions` (2026-07-02): `extraHmOptions` submodule in THIS repo's flake.nix (`blackmatter.components.mado.suggestions` — nullable scalars render-if-set, `sources` listOf-submodule, whole key renders only when non-empty; the fortinet rich-schema precedent — shikumiTypedGroups' flat fields can't express it). The darwin-developer profile migrated off raw extraSettings with byte-parity PROVEN by identical store path. NOTE the original row's pointer was stale: blackmatter-mado/module is dead legacy code — the module spec lives here since the substrate.rust.tool migration. `safra` stays raw-extraSettings until a fleet consumer arms it. | ✅ shipped |
| **M3** | lift the data type into praça (`SessionOrigin::Suggested`) + a `(defsuggestionsource)` tlisp authoring surface via the vigy host | ⏭ |
| **Store hardening** | per-source-interval TTL (`decay_per_source`, 3× interval floored by global — slow sources don't flicker) + hard `max_entries` GC (rank-ordered eviction) + picker ranked-read memoization (generation-keyed cache; clone+sort skipped while idle) | ✅ shipped |
| **Living board** (2026-07-01) | the honesty + lifecycle + liveness round: (1) **merge-don't-replace config** — a yaml `sources` entry overrides ONE kind over the prescribed arm-list (`effective_sources`; `sources_replace` escape hatch), so a params-only override never disarms the surface; (2) **typed `PollOutcome` border** — `Fetched` vs `Unavailable{unconfigured/auth-missing/error}` across all providers: a fetch blip KEEPS last-known rows (no flicker, no false "resolved") and feeds a per-source **health** surface (`record_poll`/`health`); (3) **lifecycle** `Offered → Accepted{session} (◐ soft-ack, demoted) / Snoozed / Dismissed` (sticky across re-ingest + tombstones); (4) **recurrence** — tombstone window restores birth + `times_seen` (`×N` stamp, rank nudge); (5) **aging** — bounded within-tier escalation at ranked-read time; (6) **whole-board liveness** — coarse ~3s open-tick reconciles the session registry + advances ages alongside the event-driven path; positional-stability grace on Enter; accept-failure keeps the board open with a typed notice; (7) **freshness nudge** — Ctrl-S open early-ticks stale watchers (paced per-watcher); (8) **ambient attention** — a new Critical bounces the dock with the board closed (once-latch; `attention_on_critical`); (9) **snapshot age-rebasing** (`saved_at_ms`) so warm restarts don't decay fresh-at-save rows; (10) **spawn-target kickoffs** (Jira browse URL, `tend sync`, GH_TOKEN fallback, kubectl `context` param, k8s/CI severity mapping) + `SpawnSpec::with_command` rejects control bytes (PTY-injection unconstructible); (11) **agent seam** — MCP `suggest_list` / `suggest_inject` (🤝 agent lane, additive `upsert`) / `suggest_dismiss`; (12) **safra live** — `SourceKind::Safra` adapter reconciles configured cells (VM alerts + GHA deploy flows) on per-cell cadences and projects curated signals onto the board | ✅ shipped |
| **M4 (rest)** | ✅ **samba pacing** (2026-07-02: `suggest/pace.rs` `HostPacer` — one light-core `LeakyBucket` per upstream host, 1 rps / burst 3 / ±10% jitter; the `gh` CLI billed against a synthetic `api.github.com` host; `curl -s -w %{http_code}` status border; 429 any-host / 403 GitHub → 60s cooldown → `Unavailable(Error)`, last-known rows stay) + ✅ **cross-source correlation dedup** (`CorrKey` + `collapse_correlated`) + ✅ **engine hot-reload** (2026-07-02: `EngineControl` / `EngineCommand::Swap` select-loop; the enabled-gate lives inside the thread so a boot-disabled engine hot-enables; `poll_config_reload` is the ONE reactor — and MCP `config_set` is now a typed RMW of mado.yaml through the same file ingress, so agents arm/tune sources at runtime). Remaining from this row: dedup-vs-live *type-level* hardening (the view-level collapse ships; a live-duplicated row being unrepresentable is unclaimed) | ◐ |
| **Session-world union** (2026-07-02, first slice) | (1) **praca presets persist** — `praca_store` restores the definitions catalog at boot (deliberately ONLY definitions: dead-session index/bindings must not steer auto-attach) and persists the full `PracaSnapshot` from the maintenance tick (BLAKE3-framed, change-hash-gated); (2) **single-writer election** — `single_writer` flock: only one mado process (GUI windows, `mado mcp`) persists the suggestion + praça snapshots, losers load-only (no more last-writer-wins clobber); (3) **agent tools reach the LIVE board** — `suggest_list`/`suggest_inject`/`suggest_dismiss` now kanshou-forward to the GUI process (the `list_sessions` idiom) with a process-local fallback, backed by shared `board_json`/`inject`/`dismiss` fns + three new kanshou leaves. REMAINING (gated on the tear-daemon M5 single-session-world): MCP `spawn_term` sessions as ● rows / live-dedup targets, cross-instance engine sharing (dedup of duplicate API polling). **Scoped 2026-07-02 — see "Session-world union — scope" below.** | ◐ |

### Session-world union — scope (2026-07-02, read-only survey; no code yet)

**The fact that defines the arc: there are THREE disjoint session worlds with three id spaces and no shared enumeration.**

| World | Owner | Id type | Enumerated by | Reaches Ctrl-S? |
|---|---|---|---|---|
| A — headless PTY registry | `mado mcp` process | `String` `"mado-session-N"` | `SessionRegistry::list` (`session.rs:321`) | **no** |
| B — embedded tear (`InProcess`) | GUI process | `tear_types::SessionId` | `inproc.with_registry` | **yes** (the only one) |
| C — external tear daemon | `tear daemon` process | `tear_types::SessionId` (its own space) | `Client::list_sessions` RPC (`tear-client:511`) | **no** |

**Load-bearing seam:** the picker's live-session truth is `first_pane_of` → `tear_core::InProcess` (`session_picker.rs:273`), reconciled into `praca.index` by `InProcessSessionReconciler` (`picker/reconcile.rs:45`). All three dedup layers (live-vs-preset, suggestion-vs-live via `live_corrs`, `collapse_correlated`) hang off that one source. To make worlds A and C visible as ● rows AND act as live-dedup anchors, both must feed the same `praca.index`/`first_pane_of` path.

**Key constraints surfaced:**
- `reconcile.rs`'s own docstring names `spawn_term` as the gap it closes, but it reads only `InProcess` while `spawn_term` writes world A — the reconciler can't see it by construction.
- praca bindings (`ProjectBinding: BTreeMap<PathBuf, SessionId>`) + `SessionRecord` are keyed by `tear_types::SessionId` — world A's `String` ids are **unrepresentable in praca today** (a real typed border, not an oversight to paper over).
- The daemon keeps its OWN praca store at `<state>/tear/praca.json` (different dir, no shared election with mado's `writer.lock`). tear's `SESSION-TYPESCAPE.md` M2 ("route mado through the one PracaStore") is the upstream unification step; its M5 is the mado/tear no-overlap endpoint. This arc's naming should follow tear's typescape tiers, not invent a parallel "M5".
- `TearSession` carries no cwd (per-pane); the daemon derives project roots from `spawn_cwd` at session birth — cross-world project-root identity is derivable but not free.
- Cross-instance TODAY: the writer election already arbitrates the two snapshot FILES; what it does NOT arbitrate is duplicate suggestion ENGINES (two GUIs poll every upstream twice — pacing softens, doesn't dedup) and per-GUI in-memory praca divergence.

**Destination (named first):** ONE session world — every live session (embedded, daemon, headless-MCP) enumerable through one typed source keyed by `tear_types::SessionId`, praca as the single index, Ctrl-S rendering ● rows for all of them and live-dedup anchored on all of them; one engine per state-dir (the writer-election winner shares its board over kanshou; losers subscribe instead of polling).

**Phase-down (each shippable):**
1. ✅ **World A retirement** (SHIPPED 2026-07-02, 17b2aed): `spawn_term` forwards to a new kanshou `spawn_term` leaf spawning into the GUI's embedded `InProcess` (tagged `SessionSource::Agent`); the reconciler absorbs it into praca — ● row + live-dedup anchor with zero picker changes. `TermSpec.world` selector (auto/embedded/headless; tests pin headless so a suite run can never spawn into the operator's GUI). Follow-ups ledgered: agent I/O into embedded panes (needs a kanshou send-keys leaf — embedded tear hex ids are NOT addressable by headless send_keys/get_output), and cwd-at-spawn (needs an atomic spawn-with-env tear-core API; the leaf refuses to race the picker's set_spawn_env→spawn two-step).
2. **World C rows** (medium): a daemon-session source feeding `praca.index` — either a picker-side `DaemonSessionReconciler` (a second reconciler against `Client::list_sessions` when `tear.runtime = daemon` or a daemon socket is discoverable) or upstream tear M2 (one shared PracaStore). Prefer riding tear's M2 per SESSION-TYPESCAPE — coordinate there before building mado-side.
3. **Engine sharing** (largest): loser instances subscribe to the winner's board over kanshou instead of running their own 28 watchers (the `suggest_list` forward generalized into a watch stream); the election that already picks the disk writer picks the poller too.

**Non-goals:** merging the daemon's praca file into mado's (that's tear M2's call); multi-pane rendering (tear M5 proper); any change to the suggestion algebra.

### Caching + local-optimization (lessons applied from guardrail/kanshou/tend/shikumi/CAS)

A fleet caching study (the guardrail/kanshou/PracaStore/shikumi/CAS patterns) shaped the persistence:
- **Crash-safe atomic write** (the PracaStore temp→rename pattern, hardened): `create_dir_all` + a pid-tagged temp + `sync_all` before `rename` — first-run-safe + durable. Snapshot clones under the lock then drops it, so the disk I/O is lock-free.
- **Versioned + content-verified snapshot** (the CAS/BLAKE3 lesson, inlined — `blake3` is already a dep, no new closure): a `mado-suggest v1` magic + an embedded BLAKE3 of the body. A schema bump (wrong magic) or a torn file (hash mismatch) starts empty rather than feeding garbage rows.
- **Change-`generation: AtomicU64`** (the shikumi swap-then-observe contract): bumped only on a *meaningful* change (id added/removed or a displayed/ranked field changed — never a `last_seen` heartbeat). It is the persist task's dirty signal (a startup burst of 27 first-ticks coalesces to ONE write) and the future picker read-memoization key.
- **Single maintenance task owns decay + debounced persist** (the tend "save once per cycle" lesson): the 27 watchers only ever touch RAM; one task on the `persist_debounce_secs` cadence decays + writes-if-dirty. No per-watcher disk thrash, no per-tick full-map scans.
- **Deliberately NOT done** (tier-honest): kept `Mutex` (one hot reader; ArcSwap is the named-but-deferred destination if profiling ever shows picker stalls); FNV-1a `SuggestionId` kept as the RAM dedup key (not re-keyed to BLAKE3); the picker ranked-read memoization is deferred (list() is already ~2s-throttled, so its value is low until proven by profiling).

### Tier-honest notes (a `Result::Err` is mitigation, an absent path is unrepresentability — never round up)

- **Unrepresentable today:** a `Suggestion` with no spawn target (`SpawnSpec::new` is the only ingress and rejects empty cwd/name).
- **Shipped since M-bulk:** `initial_command` now **runs** — Enter on a `🔍 pr#1234` suggestion spawns the session in the repo and types `gh pr checkout 1234` (+ Enter) through the typed `MultiplexerControl::send_keys` seam (PTY input-buffering carries it until the shell is ready). The shade-in is a real per-frame alpha ramp (M1b); urgent rows tint hot (`Urgency::tint`).
- **Only-mitigated today:** a suggestion whose target session is already live is *not yet* deduped to a Switch row (M4); the kickoff relies on PTY type-ahead buffering (robust, but not a shell-ready handshake).
- **Live-shape assumptions:** the HTTP providers (jira/grafana/datadog/opsgenie/cloudflare/google) parse documented-but-unverified response shapes; each is fully mock-tested and returns empty when unauthed, so a wrong live shape degrades to "no rows," never a crash. Verify against the live API when wiring each token.
- **Core lives in mado first**, not praça — deliberate, to avoid the cross-repo git-rev-bump loop during build-out. M3 lifts it.

## Directive compliance

- **TYPED-SPEC + INTERPRETER TRIPLET** — each source = a pure interpreter over the `SuggestionEnvironment` trait, mocked in tests. (Lisp authoring leg is M3.)
- **CATALOG REFLECTION** — `SourceKind::ALL` + exhaustive per-method matches; a new variant is a compile error until labelled/catalogued.
- **shikumi** — `SuggestionsConfig` is tiered (bare/prescribed) with `deny_unknown_fields`.
- **UNREPRESENTABILITY** — no suggestion without a spawn target.
- **TYPED EMISSION** — no `format!()`; labels via `push_str`/`Display`.
- **NO SHELL** — every subprocess is a typed `Cmd` (program + argv), HTTP via a typed `curl` argv.
- **shigoto / vigy** — the recurring multi-source refresh is the watcher plane; vigy is the in-process daemon the M3 tlisp-authored sources run on.
- **no-overlap layering** — tear = model, mado = view, praça = orchestration; the store + bridge respect it.
