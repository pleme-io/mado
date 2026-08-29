# Safra — the observability-curation mini-framework

> *safra* (Brazilian-Portuguese: a curated **collection / holdings / archive**).
> The typed framework inside mado that maintains **long-term, curated, in-memory
> data structures** — one per tracked data-type across every observability
> service — and feeds them, efficiently, into the Ctrl-S suggestion stream so the
> operator is continuously offered *"the errors to handle, in the environments
> they track."*

> **Destination first (Operating Principle #0).** This document is the *absolute
> destination*, consolidated from the operator's directives. The phases at the
> end are the path; they are not the destination. Safra owns **almost no new
> algebra** — it is a typed curation layer *over* the substrate that already
> ships (`SuggestionSource`/`SuggestionEnvironment`/the persisted store/the
> `vigy` runtime + the ~10 existing observability sources). It is the Viggy
> reconciliation model applied to observability data.

---

## 1. The one-sentence shape

**Every time mado turns on, `vigy` agents fan out and rebuild a typed,
long-term, in-memory `Safra` — one curated structure per tracked data-type per
tracked environment — then continuously and efficiently converge it to upstream
deltas (batch, not one-by-one), all tunable at three scopes (global / group /
specific), authored in tatara-lisp, materializing its secrets from SOPS, shipped
by *nobody by default* and layered in via a private deployment layer.**

The operator authors *declarations*; vigy does the *rebuilding*; the safra holds
the *curated truth*; Ctrl-S *renders* it. Two human verbs: declare + observe.

---

## 1.5 The operating model — the live work board you burn down

**Ctrl-S is a consistently-updating board of issues the operator works down to
zero.** Two lanes, one board:

- **Sprint lane** (planned) — Jira sprint + assigned tickets (the existing
  suggestion sources).
- **Stability / on-call lane** (reactive) — the safra-curated signals from the
  Lareira tap mesh (firing alerts, PromQL/SLO breaches, log spikes,
  OpsGenie/Datadog paging state).

Both are the same kind of thing — *a unit of work to pick up* — co-ranked in one
board.

**The loop: populate → target → open session → work → clean.**

- *populate* — vigy agents converge each lane's items into its `CuratedSet`;
  **resolved items decay off** (the board self-cleans as alerts recover /
  tickets close).
- *target* — scan Ctrl-S, pick one.
- *open session* — Enter spawns a **pre-named session scoped to that item**
  (ticket key + summary, or incident env + error) and **binds** the session to it.
- *work → clean* — troubleshoot in the session; you burn the board **down to an
  empty curated set — everything clean.**

**Bounded board, capped backlog, backfill.** Every curated item is session-able
— but the board must **stay under control**, never flood the screen:

- The **curated backlog** (the `CuratedSet`) holds *all* live errors, kept
  bounded by dedup + recurrence + decay + a per-cell `max_items` cap (the
  "keep them under control" invariant).
- The **visible board** shows only the top-`max_visible` ranked items, with a
  `per_source_cap` so one noisy source can't crowd out the rest.
- As a visible slot frees — an item resolved (decay), its session opened
  (soft-acked, in-progress), or dismissed — **the next-ranked backlog item fills
  in.** The board is always full of the most-important *open* work, and drains
  as you resolve things.

**Lifecycle — the "in progress" mark.** An item with an active bound session is
*being worked*: soft-acked so it deprioritizes (doesn't crowd new errors) but
doesn't vanish until upstream confirms resolution (decay) or you dismiss it. The
board legibly shows *waiting* vs *in progress* vs *gone*.

The engine already gives the hard part: `converge`/`decay` **is** the
consistently-updating, self-cleaning board; `ranked()` is the priority; the
suggestion stream's `max_visible`/`per_source_cap`/backfill is the windowing.
The concrete additions this model implies: enforce the per-cell `max_items` cap;
the **projection** (curated item → a board `Suggestion` carrying the
scoped-session `SpawnSpec`); and the **session-binding lifecycle** (open →
in-progress → resolved).

---

## 2. What already exists (the foundation — extend, never duplicate)

| Need | Already shipped | File |
|---|---|---|
| Mockable I/O boundary (HTTP + secrets + subprocess + clock) | `SuggestionEnvironment` trait — **already has `http_get(HttpReq)` + `secret(key)` + `run(Cmd)` + `now_unix`** | `src/suggest/env.rs:134` |
| SOPS secret hook | `RealEnvironment::secret()` reads `~/.config/<category>/<name>` (sops-nix render path) | `src/suggest/env.rs:260` |
| Typed HTTP request builder (bearer/basic/headers, curl-as-argv, no shell) | `HttpReq` | `src/suggest/env.rs` |
| Source contract | `SuggestionSource::poll(env, cfg)` trait | `src/suggest/source.rs:55` |
| ~10 observability sources | grafana_alerts/incidents/oncall, datadog_monitors, k8s_unhealthy, flux_failing, breathe_conflict, engenho_nodes, opsgenie_alerts, aws_health, cloudflare_deployments | `src/suggest/sources/` |
| Persisted store (long-term) | the suggestion store persists to `~/.local/share/mado/suggestions.json` | `src/suggest/store.rs` |
| Continuous pooling runtime | the embedded **vigy** reconciler runtime + 1 watcher/source | `src/vigy_host.rs`, `src/suggest/mod.rs` |
| Recurrence/dedup | the `anomaly-recurrence` pattern (stable signature + recurrence count) | (skill / to wire) |
| Tiered config | shikumi `TieredConfig` (bare/prescribed) + per-source overrides | `src/config.rs` |
| TYPED-SPEC triplet + mock | `MockEnvironment` with `.http()`/`.secret_val()` builders | `src/suggest/env.rs:317` |

**Safra adds exactly five things the substrate lacks:** (1) a *declared typed
schema* for tracked data + environments; (2) the *long-term curated structure*
(per-type, per-env, deduped, recurrence-counted, decaying) as a first-class
value distinct from the raw suggestion store; (3) *tracked-environment scoping*
(sources run per-environment, not globally); (4) *efficient batch/delta
convergence* (not full re-fetch); (5) the *three-scope (global/group/specific)
tunable config*, layered, authored in tatara-lisp, scoped to the private
deployment layer.

---

## 2.5 Upstream — the Lareira tap mesh (what safra absorbs)

**The concrete source is the Lareira observability program**
(doctrine home to author: `pleme-io/theory/LAREIRA.md`). Its tap layer deploys
**taps** that branch live data flows off a target fleet — a matrix of Vector
clusters across (tenant × cloud × dev/staging/prod) — into an in-boundary
**Lareira mesh** —
`vector → NATS-JetStream (respiro, scale-to-zero) → breathe →
VictoriaMetrics (victoriaops) / VictoriaLogs → Grafana(as-code) → grafana-rio
MCP`, + ntfy. The store, dashboards-as-code, breathe, and the **`grafana-rio`
MCP are LIVE on rio.**

**safra is the mado-side operator consumer of that mesh** — the in-terminal
realization of the `alert`-tap doctrine ("a personal early-warning over
my own copy, earlier than the team, never paging"). It absorbs the curated
signals the taps produce and surfaces them in Ctrl-S. The surfaces map
directly onto safra's `ServiceKind`s:

| Lareira tap surface | safra `CellSource` |
|---|---|
| **victoriaops** (VictoriaMetrics) | PromQL — firing alerts / SLO breaches, per-tenant tag |
| **VictoriaLogs** | error-log spikes |
| **grafana-rio MCP** (live) | Grafana alerts via MCP — the "grafana-mcp, structured" path |
| **`mirror` taps** (team OpsGenie / Datadog state) | read-only paging-state pull |

**The cell ↔ tap identity:** a safra `TrackedEnvironment` **is a
tap-cell** — the tap-layer cell grammar `tap-<datakind>-<tenant>-<env>-<cloud>-<region>`
(e.g. `tap-audit-<tenant>-prod-aws-use2`), its endpoints the tap access
grammar (`grafana.<tap>.<tenant>.<env>.<domain>`), auth via the cofre/SOPS
`SecretRef` safra already models. The `groups` axis carries
tenant/env/cloud/region so the three-scope config can tune e.g. all cells of one
tenant, or all `prod` cells, at once.

**First-real-cell unlock:** because `grafana-rio` MCP + victoriaops are
**already live**, safra's first real cell can absorb **now** — no dependency on
the per-tenant tap deploys (which gate on the tap layer's own M0). Scope:
**non-tenant fleet-global telemetry only**; per-tenant data is
residency/governance-gated (raw data never crosses residency; rio receives only
fleet-global / derived signal). The `borealis` codename hygiene applies —
safra's *generic* curation surface never names the consumer; the concrete
tap endpoints + SecretRefs live in the deployment config layer (§8).

---

## 3. The declared schema (TYPED-SPEC + INTERPRETER triplet)

Per the org ★★ TYPED-SPEC rule, every safra concept ships as **typed Rust
border + authored Lisp spec + working interpreter over the mockable
`SuggestionEnvironment`**. The author-facing tatara-lisp forms:

- **`(deftrackedenv …)`** — a watched environment: name, group(s), the service
  endpoints it exposes (a Grafana URL, a VictoriaMetrics/Prometheus base, a
  Datadog site, a kube-context), and a `cofre`/SOPS `SecretRef` per endpoint
  (never plaintext). The tracked-environment set is **whatever fleet the
  deployment layer declares** (e.g. `dev` / `staging-*` / `prod-*`); mado ships
  none.
- **`(deftrackeddata …)`** — a tracked data-type: its *schema* (the typed shape
  of one curated item — e.g. a firing alert, an unhealthy pod, a SLO breach), its
  *source* (which service + query), its *identity* (the stable signature for
  dedup/recurrence), its *decay* (TTL / staleness), and its *convergence* (how to
  diff observed-vs-curated and apply a batch delta).
- **`(deferrorsource …)`** — a concrete source binding a `(deftrackeddata)` to an
  endpoint on a `(deftrackedenv)` via a typed query (PromQL for VM/Prometheus, a
  Grafana datasource query, a Datadog monitor filter, a kube list+field-selector).
  Implements `SuggestionSource` against the existing trait.

Each `(def…)` is a `#[derive(TataraDomain)]` Rust struct; bad compositions are
compile errors on the Rust side (TYPED-SPEC discipline). A catalog
(`(defsafracatalog)`) reflects every tracked data-type (★★ CATALOG REFLECTION).

---

## 4. The curated structure — `Safra` (long-term, in-memory, per-type per-env)

```
Safra
 └── per TrackedEnvironment (rio, staging-use2, prod-euw1, …)
      └── per TrackedDataKind (alerts, unhealthy-pods, slo-breaches, …)
           └── CuratedSet<Item>
                ├── items keyed by Item::signature()      (stable identity)
                ├── recurrence count + first_seen/last_seen (anomaly-recurrence)
                ├── decay (drop when last_seen older than the kind's TTL)
                └── rank (severity × recurrence × recency)
```

- **Long-term:** the `Safra` persists (content-addressed snapshot on disk, the
  store's existing persistence deepened) so a restart re-surfaces the last-known
  curated truth *instantly* while vigy re-pools in the background — the operator
  never stares at an empty Ctrl-S after a restart.
- **Curated, not raw:** an `Item` enters the curated set through
  `converge(observed_batch) → Delta { added, updated, expired }` — dedup by
  signature, bump recurrence on re-observation, decay on absence. The safra is
  the *converged* state, never the raw upstream dump.
- **Feeds Ctrl-S:** a thin projection maps the top-ranked curated items per
  environment into `Suggestion`s (the existing type) — "🔥 prod-euw1: 3× OOMKilled
  api-gateway" → Enter spawns a pre-named session scoped to that error+env.

---

## 5. Vigy rebuilds it — on boot + continuously, efficiently, in batch

**The operator's load-bearing directive:** *"every time we turn mado on we send
vigy agents to rebuild the curated in-memory structure for each type of data
tracked across all these services."*

- **Boot fan-out:** on mado start, for each `(deftrackeddata × deftrackedenv)`
  cell whose config is enabled, register a `vigy` reconciler. Each reconciler's
  first tick **rebuilds** its `CuratedSet` from upstream (seeded from the
  persisted snapshot, then reconciled to live).
- **Continuous convergence (Viggy seven-beat):** each reconciler ticks on its
  configured cadence — *observe* (batch-fetch upstream), *diff* (against the
  curated set by signature), *adjust* (apply the `Delta`), *attest* (bump
  recurrence/decay), *tick*. **Efficient + batch:** one query returns the whole
  current set for that (type, env); the diff is O(set) by signature; only the
  delta mutates the structure — never a per-item round-trip, never a full rebuild
  after the first.
- **Batch across cells:** reconcilers sharing an endpoint (e.g. all rio
  k8s-derived kinds) batch their upstream calls where the API allows (one
  `kubectl get … -o json` feeds many kinds), paced by the samba `LeakyBucket`
  (the fleet rate-limit primitive) so a watched prod cluster is never hammered.

---

## 5.5 Homeostatic curation — reuse `breathe-control`'s band law

**The curation control loop and `breathe`'s resource-homeostasis loop are the
same algorithm** — both hold a measured level inside a band by a pure
observe → decide → carve loop with a shrink-safety clamp. So safra **consumes**
breathe's control core rather than re-deriving it (Prime Directive: shared
library at ≥2 consumers).

`pleme-io/breathe`'s **`breathe-control`** crate already factors the pure band
law out of the k8s executor: `decide` / `plan_tick` → a typed `Decision`
(`Hold | Grow{from,to} | Shrink{from,to} | NoSafeShrink{current}`) against a
`BandConfig` (floor / setpoint / ceiling / shrink-below / grow-above + a
warmup-hold + a metric-missing policy), with a **shrink-safety clamp** so a carve
never crosses the safe floor. Its tuned knobs ride `lapidar::TunedParam` — a
second primitive safra shares. The law is pure; the only coupling is that its
level is *named* for bytes (`floor_bytes` …).

**The mapping is exact, including the safety semantics:**

| breathe (memory) | safra (curation) |
|---|---|
| measured level = bytes used | measured level = curated item-count |
| carved limit = the memory limit | carved limit = the retention budget |
| shrink-safety: never carve below the safe floor → never OOM-kill | never evict an **unhandled high-rank** item → never drop an issue you haven't worked |
| warmup-hold: don't shrink before the peak is observed | don't shrink retention before steady-state error volume is seen (post-boot) |
| `Grow` / `Hold` / `Shrink` the limit | `Grow` / `Hold` / `Shrink` retention (supersedes the hard `max_items` cap) |

**Two safra consumers of the one band law:**

1. **Backlog homeostasis** — hold the curated item-count in a band; the hard
   `CuratedSet::cap` becomes the degenerate floor of a `decide`-driven retention
   carve whose `NoSafeShrink` analog is "an unhandled critical is present —
   refuse to shrink."
2. **Cadence breathing** — the `respiro` inhale/hold/exhale applied to the
   reconcile interval: high error pressure → *inhale* (poll faster, keep more);
   steady → *hold*; board clean → *exhale* (poll slowly, rest at near-zero cost).
   The same `decide` law over poll-interval instead of retention.

**The generalization move:** lift `breathe-control`'s band law to be
**unit-agnostic** (the `_bytes` level → a unit-neutral `u64` / newtype) so both
breathe (memory/CPU) and safra (item-count, cadence) consume one core. This is a
focused cross-repo effort (breathe + mado) — greenlight-gated, its own phase
(see §10 M4′).

---

## 6. Three-scope tunability (global / group / specific) — layered config

Per the directive *"all configurable and tunable both in the global, group, and
specific sense."* The safra config resolves each knob (cadence, enabled,
max-items, decay-TTL, rank-weights, rate-budget) through **three scopes**, most
specific wins:

```
global   →  safra defaults (all kinds, all envs)        e.g. cadence = 120s
   ⊕
group    →  per environment-group / per data-kind-group  e.g. group "prod" cadence = 60s
   ⊕
specific →  per (kind × env) cell                         e.g. (oom-kills × prod-euw1) cadence = 30s
```

This composes with shikumi's existing tier model: mado's **bare** tier ships the
safra **off entirely** (no endpoints, no secrets, no pooling); the **private
deployment layer** supplies the global+group+specific config on top (the tracked
envs, the SOPS `SecretRef`s, the per-cell tuning). mado-the-binary never ships a
single endpoint or secret for any consumer.

---

## 7. SOPS secrets → memory (reuse the existing hook)

Each `(deftrackedenv)` endpoint names a `SecretRef` (`category/name`). The
existing `SuggestionEnvironment::secret(key)` already resolves the sops-nix
render path (`~/.config/<category>/<name>`). Safra materializes the needed
secrets **into memory** on boot (read once, hold for the source's bearer/basic
auth), never logging them, never writing them anywhere. The Nix side
(the deployment layer / blackmatter-secrets) declares the `sops.secrets` so the
render path exists; safra only *reads* it through the typed hook. No new secret
vocabulary — `SecretRef` stays the only consumer surface (composes with cofre).

---

## 8. Nix-native + mado-config-native + tatara-lisp, scoped to the deployment layer

- **Authoring** is tatara-lisp (`(deftrackedenv)`/`(deftrackeddata)`/`(deferrorsource)`).
- **Config** is mado-config-native (a new `safra:` section in the mado
  shikumi schema, **bare = off**), resolved through the three scopes.
- **Delivery** is Nix-native + **layered**: a private, out-of-tree HM module
  contributes the safra config layer (envs + SOPS refs + tuning) *on top of*
  mado's base config — declared once, deployed via home-manager. mado ships none
  of it; turning it on is adopting that layer.

---

## 9. Composition with fleet rules

- **TYPED-SPEC + INTERPRETER triplet** — each safra domain = Rust border + Lisp
  spec + interpreter over the mockable `SuggestionEnvironment` (tests need no real
  Grafana/cluster).
- **Viggy / CONTINUOUS CONVERGENCE** — each (kind × env) reconciler is a
  desired-vs-observed loop; the safra is the converged state.
- **anomaly-recurrence** — the curated identity + recurrence-count surface.
- **TYPED EMISSION / NO SHELL** — queries are typed `HttpReq`/`Cmd` (curl/kubectl
  as argv, already the pattern); no `format!()` of query strings beyond typed
  builders.
- **samba rate-limiting** — every upstream call paced by `LeakyBucket`.
- **CATALOG REFLECTION** — `(defsafracatalog)` self-describes the tracked kinds.
- **cofre / SOPS** — `SecretRef` is the only secret vocabulary.
- **shikumi tiers + 3-scope layering** — bare-off, deployment layer on.

---

## 10. The route (phases — each shippable)

- **M0 — typed core + one live cell.** ✅ **shipped (2026-07-01).** The `safra`
  module: the declared schema (`TrackedEnvironment`, `TrackedDataKind`,
  `CuratedSet<Item>` + `converge → Delta`), the three-scope config resolver,
  and the live wiring: a `safra:` section in the mado config (environments +
  kinds + gha filter + tuning scopes, **off by default**), the
  `SafraSuggestionSource` adapter (`SourceKind::Safra`) that reconciles every
  configured cell on its OWN resolved cadence inside the suggestion engine's
  fan-out poll and projects the top curated signals onto Ctrl-S, and TWO
  concrete `CellSource`s — `VmAlertsSource` (VictoriaMetrics/Prometheus
  `/api/v1/alerts` firing alerts, the M0 gate cell) + `GhaDeploymentSource`
  (the deploy-flow lane). **Runtime note (supersedes §5's "vigy agents"
  phrasing):** M0 rides the existing `SuggestionEngine` watcher plane — zero
  new runtime; the vigy-hosted per-cell reconcilers are the M-later
  destination, and §5's boot-rebuild + batch-convergence semantics hold
  unchanged (the adapter's first poll IS the boot rebuild).
- **M1 — the tatara-lisp domains** (`deftrackedenv`/`deftrackeddata`/`deferrorsource`)
  + the catalog + the persisted long-term snapshot (instant re-surface on restart).
- **M2 — the source matrix** — VictoriaMetrics/Prometheus (PromQL), Grafana
  (datasource + grafana-mcp-shaped), Datadog, k8s-API, deepening/reusing the
  existing sources into per-env curated kinds; batch-by-endpoint.
- **M3 — the deployment layer** — the private HM module supplying the tracked
  fleet envs + SOPS `SecretRef`s + global/group/specific tuning, layered
  onto mado; the SOPS render wiring.
- **M4 — convergence hardening** — samba pacing per endpoint, decay/rank tuning,
  the seven-beat attestation, drift/“upstream changed” efficiency (etag/since
  cursors where the API supports incremental).
- **M4′ — homeostatic curation (shared band law)** — generalize
  `breathe-control`'s band law unit-agnostic; safra consumes it for backlog
  homeostasis (shrink-safe retention) + cadence breathing (respiro
  inhale/hold/exhale over poll interval). Cross-repo (breathe + mado); shares
  `lapidar` tuned-params. See §5.5.

Tier-honest ledger: every PR advances a phase or leaves a `pending-safra: <Mn>`
note. **Per-repo waiver:** safra is **off by default** — no waiver needed to
*not* ship it; adopting it is opting into the private deployment layer.

**Canonical spec:** this doc. **Built on:** `src/suggest/` + `src/vigy_host.rs`.
**Operator face:** the private safra config layer (M3).
