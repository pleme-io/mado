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
| **M2** | nix HM/NixOS/Darwin module trio for `suggestions` (blackmatter-mado + terminal.nix) | ⏭ |
| **M3** | lift the data type into praça (`SessionOrigin::Suggested`) + a `(defsuggestionsource)` tlisp authoring surface via the vigy host | ⏭ |
| **Store hardening** | per-source-interval TTL (`decay_per_source`, 3× interval floored by global — slow sources don't flicker) + hard `max_entries` GC (rank-ordered eviction) + picker ranked-read memoization (generation-keyed cache; clone+sort skipped while idle) | ✅ shipped |
| **Living board** (2026-07-01) | the honesty + lifecycle + liveness round: (1) **merge-don't-replace config** — a yaml `sources` entry overrides ONE kind over the prescribed arm-list (`effective_sources`; `sources_replace` escape hatch), so a params-only override never disarms the surface; (2) **typed `PollOutcome` border** — `Fetched` vs `Unavailable{unconfigured/auth-missing/error}` across all providers: a fetch blip KEEPS last-known rows (no flicker, no false "resolved") and feeds a per-source **health** surface (`record_poll`/`health`); (3) **lifecycle** `Offered → Accepted{session} (◐ soft-ack, demoted) / Snoozed / Dismissed` (sticky across re-ingest + tombstones); (4) **recurrence** — tombstone window restores birth + `times_seen` (`×N` stamp, rank nudge); (5) **aging** — bounded within-tier escalation at ranked-read time; (6) **whole-board liveness** — coarse ~3s open-tick reconciles the session registry + advances ages alongside the event-driven path; positional-stability grace on Enter; accept-failure keeps the board open with a typed notice; (7) **freshness nudge** — Ctrl-S open early-ticks stale watchers (paced per-watcher); (8) **ambient attention** — a new Critical bounces the dock with the board closed (once-latch; `attention_on_critical`); (9) **snapshot age-rebasing** (`saved_at_ms`) so warm restarts don't decay fresh-at-save rows; (10) **spawn-target kickoffs** (Jira browse URL, `tend sync`, GH_TOKEN fallback, kubectl `context` param, k8s/CI severity mapping) + `SpawnSpec::with_command` rejects control bytes (PTY-injection unconstructible); (11) **agent seam** — MCP `suggest_list` / `suggest_inject` (🤝 agent lane, additive `upsert`) / `suggest_dismiss`; (12) **safra live** — `SourceKind::Safra` adapter reconciles configured cells (VM alerts + GHA deploy flows) on per-cell cadences and projects curated signals onto the board | ✅ shipped |
| **M4 (rest)** | samba rate-limiting for HTTP/MCP sources + dedup-vs-live *type-level* hardening + cross-source correlation dedup + engine hot-reload on config change | ⏭ |
| **Session-world union** | the board reconciles only the GUI's embedded registry: MCP `spawn_term` sessions + external tear daemons are invisible as ● rows AND to the live-dedup; praca presets/frecency are per-process ephemera (`PracaSnapshot` unused in mado); multi-instance engines race one snapshot file. The keystone's next arc. | ⏭ |

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
