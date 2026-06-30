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
by *nobody by default* and layered in via `blackmatter-akeyless`.**

The operator authors *declarations*; vigy does the *rebuilding*; the safra holds
the *curated truth*; Ctrl-S *renders* it. Two human verbs: declare + observe.

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
tunable config*, layered, authored in tatara-lisp, scoped to blackmatter-akeyless.

---

## 2.5 Upstream — the Lareira / Tendril mesh (what safra absorbs)

**The target is Akeyless, and the concrete source is the Tendril / Lareira
observability program** (Akeyless Confluence; doctrine home to author:
`pleme-io/theory/LAREIRA.md`). Tendril deploys **taps** that branch live data
flows off the Akeyless fleet (28 Vector clusters across
MTE/akeyless_global · MEU · MUS · CVS · DBK · WMT × AWS/GCP/AZR ×
dev/staging/prod) into an in-boundary **Lareira mesh** —
`vector → NATS-JetStream (respiro, scale-to-zero) → breathe →
VictoriaMetrics (victoriaops) / VictoriaLogs → Grafana(as-code) → grafana-rio
MCP`, + ntfy. The store, dashboards-as-code, breathe, and the **`grafana-rio`
MCP are LIVE on rio.**

**safra is the mado-side operator consumer of that mesh** — the in-terminal
realization of Tendril's `alert`-tap doctrine ("a personal early-warning over
my own copy, earlier than the team, never paging"). It absorbs the curated
signals the tendrils produce and surfaces them in Ctrl-S. The surfaces map
directly onto safra's `ServiceKind`s:

| Tendril / Lareira surface | safra `CellSource` |
|---|---|
| **victoriaops** (VictoriaMetrics) | PromQL — firing alerts / SLO breaches, per-tenant tag |
| **VictoriaLogs** | error-log spikes |
| **grafana-rio MCP** (live) | Grafana alerts via MCP — the "grafana-mcp, structured" path |
| **`mirror` taps** (team OpsGenie / Datadog state) | read-only paging-state pull |

**The cell ↔ tendril identity:** a safra `TrackedEnvironment` **is a tendril
tap-cell** — Tendril's cell grammar `tap-<datakind>-<tenant>-<env>-<cloud>-<region>`
(e.g. `tap-audit-mte-prod-aws-use2`), its endpoints the `tendril.*` access
grammar (`grafana.tendril.<tenant>.<env>.akeyless.dev`), auth via the cofre/SOPS
`SecretRef` safra already models. The `groups` axis carries
tenant/env/cloud/region so the three-scope config can tune e.g. all
`mte`-tenant cells or all `prod` cells at once.

**First-real-cell unlock:** because `grafana-rio` MCP + victoriaops are
**already live** (holding rio + akeyless_global telemetry), safra's first real
cell can absorb **now** — no dependency on the per-tenant tendril deploys (which
gate on Tendril's own M0). Scope: **akeyless_global (non-customer) only**;
customer-tenant data is residency/governance-gated (raw data never crosses
residency; rio receives only akeyless_global / derived signal). The `borealis`
codename hygiene applies — safra's *generic* curation surface never names the
consumer; the akeyless tendril endpoints + SecretRefs live in the
blackmatter-akeyless config layer (§8).

---

## 3. The declared schema (TYPED-SPEC + INTERPRETER triplet)

Per the org ★★ TYPED-SPEC rule, every safra concept ships as **typed Rust
border + authored Lisp spec + working interpreter over the mockable
`SuggestionEnvironment`**. The author-facing tatara-lisp forms:

- **`(deftrackedenv …)`** — a watched environment: name, group(s), the service
  endpoints it exposes (a Grafana URL, a VictoriaMetrics/Prometheus base, a
  Datadog site, a kube-context), and a `cofre`/SOPS `SecretRef` per endpoint
  (never plaintext). The tracked-environment set is the **Akeyless fleet**
  (dev/cicd/staging-*/prod-*/cs-* — the portão/shaar registry) by default.
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
 └── per TrackedEnvironment (rio, akeyless-staging-use2, prod-euw1, …)
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
safra **off entirely** (no endpoints, no secrets, no pooling); the
**blackmatter-akeyless** layer supplies the global+group+specific config on top
(the tracked envs, the SOPS `SecretRef`s, the per-cell tuning). mado-the-binary
never ships a single Akeyless endpoint or secret.

---

## 7. SOPS secrets → memory (reuse the existing hook)

Each `(deftrackedenv)` endpoint names a `SecretRef` (`category/name`). The
existing `SuggestionEnvironment::secret(key)` already resolves the sops-nix
render path (`~/.config/<category>/<name>`). Safra materializes the needed
secrets **into memory** on boot (read once, hold for the source's bearer/basic
auth), never logging them, never writing them anywhere. The Nix side
(blackmatter-akeyless / blackmatter-secrets) declares the `sops.secrets` so the
render path exists; safra only *reads* it through the typed hook. No new secret
vocabulary — `SecretRef` stays the only consumer surface (composes with cofre).

---

## 8. Nix-native + mado-config-native + tatara-lisp, scoped to blackmatter-akeyless

- **Authoring** is tatara-lisp (`(deftrackedenv)`/`(deftrackeddata)`/`(deferrorsource)`).
- **Config** is mado-config-native (a new `safra:` section in the mado
  shikumi schema, **bare = off**), resolved through the three scopes.
- **Delivery** is Nix-native + **layered**: a `blackmatter-akeyless` HM module
  contributes the safra config layer (envs + SOPS refs + tuning) *on top of*
  mado's base config — declared once, deployed via home-manager. mado ships none
  of it; turning it on is adopting the blackmatter-akeyless layer.

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
- **shikumi tiers + 3-scope layering** — bare-off, blackmatter-akeyless layer on.

---

## 10. The route (phases — each shippable)

- **M0 — typed core + one live cell.** The `safra` module: the declared schema
  (`TrackedEnvironment`, `TrackedDataKind`, `CuratedSet<Item>` + `converge → Delta`),
  the three-scope config resolver, and **one working (kind × env) cell** — rio
  VictoriaMetrics firing-alerts → curated set → vigy reconciler rebuilds on boot
  + converges in batch → projected into Ctrl-S. Fully mockable, tested, **off in
  bare**. Proves the whole spine end-to-end on one cell.
- **M1 — the tatara-lisp domains** (`deftrackedenv`/`deftrackeddata`/`deferrorsource`)
  + the catalog + the persisted long-term snapshot (instant re-surface on restart).
- **M2 — the source matrix** — VictoriaMetrics/Prometheus (PromQL), Grafana
  (datasource + grafana-mcp-shaped), Datadog, k8s-API, deepening/reusing the
  existing sources into per-env curated kinds; batch-by-endpoint.
- **M3 — the blackmatter-akeyless layer** — the HM module supplying the tracked
  Akeyless-fleet envs + SOPS `SecretRef`s + global/group/specific tuning, layered
  onto mado; the SOPS render wiring.
- **M4 — convergence hardening** — samba pacing per endpoint, decay/rank tuning,
  the seven-beat attestation, drift/“upstream changed” efficiency (etag/since
  cursors where the API supports incremental).

Tier-honest ledger: every PR advances a phase or leaves a `pending-safra: <Mn>`
note. **Per-repo waiver:** safra is **off by default** — no waiver needed to
*not* ship it; adopting it is opting into the blackmatter-akeyless layer.

**Canonical spec:** this doc. **Built on:** `src/suggest/` + `src/vigy_host.rs`.
**Operator face:** the `blackmatter-akeyless` safra config layer (M3).
