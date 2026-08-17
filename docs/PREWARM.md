# Session pre-warming — the on-call cockpit (vigy-configurable)

> **Direction (operator, 2026-07-09, from an on-call rotation):** a Datadog/OpsGenie/
> Grafana/VM alert on the Ctrl-S board shouldn't just Enter into a *bare* shell
> — choosing to create its session should land you **already in the issue**:
> kube-context set, pod described, logs streaming, runbook + dashboard open. The
> **prewarm strategy** (which alert class prewarms what) is **fully
> vigy-configurable**. This doc is the destination-first plan.

## Destination (Op-Principle #0)

mado is an **on-call cockpit**: `alert Signal → PrewarmStrategy (selected by
ServiceKind × severity × identity) → PrewarmSpec (ordered Vec<PrewarmStep>) →
compiled into the spawn payload → executed EAGERLY, once, in the freshly-spawned
session`. A typed `PrewarmStep` sum type makes an **un-runnable /
injection-bearing prewarm step unrepresentable** — the ordered generalization of
`izumi::SpawnSpec`'s single `initial_command` into the multi-step setup on-call
actually needs.

## The scope-boundary discipline (load-bearing)

Per mado's "mado-memory-privileged only" boundary: the **STRATEGY** (which alert
class prewarms what, gated on mado live-state — env, tear-attached, session
graph, current context) is mado-state-privileged → **vigy decides**. The
**ACTIONS** (the `kubectl`/`curl`/`open` commands the *new session's shell*
runs) are shell-doable → carried as **validated data** and **run by the shell**,
**never** reimplemented as privileged `(mado-send-keys)`/`(mado-spawn-term)`
write-intrinsics (which would blur the boundary). **vigy stays decide-only;** the
mado *executor* (not lisp) actuates via the shipped MCP-lineage verbs
(`spawn_term`/`send_keys`/`switch_session`). This is the same SpawnSpec (noun) /
session_picker (verb) split that exists today.

## The vocabulary — TYPED-SPEC triplet (`mado/src/prewarm/`)

1. **Rust border** — `PrewarmSpec { steps: Vec<PrewarmStep> }` + the sum type
   `PrewarmStep` (`RunCommand` · `SetEnv` · `KubeContext` · `OpenUrl(url::Url)`),
   **each valid-by-construction** (parse-time-rejected — matching SpawnSpec's
   grade). `RunCommand` reuses `SpawnSpec::with_command`'s control-byte/blank
   rejection (the PTY-newline-injection defense; alert labels/detail can contain
   `\n`) — extracted to a shared `reject_injection(&str) -> Option<String>`.
2. **Authored Lisp spec** — a hand-crafted `(defprewarm …)` reader (net-new,
   mado-side; vigy has zero TataraDomain and it stays telos-free). Per the
   macro-vocabulary core learning ([`MACRO-VOCABULARY.md`](./MACRO-VOCABULARY.md)):
   **generate** the per-`(ServiceKind × severity)` strategy *table*; keep the
   ergonomic step-list *reader* hand-crafted. Do **not** route `(defprewarm)`
   through `#[derive(TataraDomain)] register::<T>()` — over-abstraction for
   vigy's raw-lisp runtime. Shape:
   `(defprewarm :on vm/oom-killed :steps ((kube-context $env) (run "kubectl describe pod $identity") (open-url $link)))`
   — `$env`/`$identity`/`$link` are the **only** interpolated values, filled from
   Signal *fields* (never raw-label concatenation), each re-flowed through the
   control-byte border.
3. **Interpreter** — `apply(spec, &mut impl PrewarmEnv) -> Result<_, PrewarmError>`
   behind an `Environment` trait (`send_keys`/`split_pane`/`set_env`/`open_url`)
   so tests mock the side effects — no test needs a real PTY/kube/network.

## SpawnSpec ownership — upstream to izumi (decisive); mado-side interim allowed

`PrewarmSpec`/`PrewarmStep` belong **upstream in izumi** (`izumi/src/prewarm.rs`,
beside SpawnSpec): a prewarm sequence is the exact ordered generalization of
`initial_command`, and the izumi-sources providers (github/jira/grafana/k8s/flux)
would all emit it — a mado-only shape would fork the board substrate. Wire-compat
is **additive**: `#[serde(default)] prewarm: Vec<PrewarmStep>` on `SpawnSpecWire`;
`initial_command` stays a back-compat convenience that lowers to one
`RunCommand` (golden fixtures unchanged). **Interim** (legitimate, named, not
enshrined): a mado-side-only payload (`Item<K, A>` is generic → zero izumi core
change) if the izumi cross-repo release must be deferred.

## Execution model — eager on create

At the accept seam `spawn_suggestion_inner` (`session_picker.rs:513-571`),
generalize the single `initial_command` kickoff (`:562`) into an ordered
`PrewarmExecutor` loop: `RunCommand`/`KubeContext` → `send_keys(kickoff_keystrokes)`;
`OpenUrl` → `open`; `SetEnv` → folded into `spawn_env_base` **pre-spawn** (the
named pre/post-spawn ordering split — env can't be cleanly injected into a live
shell); split-pane step (extend single-pane) for "logs streaming in a split."

## Phased path

| M | What | Status |
|---|---|---|
| **M0** | Typed `PrewarmStep` border + interpreter (mado-side core, mockable `PrewarmEnv`, injection-safe by construction) | **✓ shipped** |
| **M1** | `SessionPrewarmEnv` executor: generalize the single kickoff into the ordered eager-on-create walk (reuses `send_keys`/`url::open_link`) | **✓ shipped** |
| **first slice** | `safra::prewarm::prewarm_for_signal` — the on-call strategy from a real VM Signal (kube-context + best-effort pod-describe + open link) | **✓ shipped** |
| **M2** | Carry the `PrewarmSpec` from `project_signal` to `spawn_suggestion_inner` — **the one decision**: izumi-upstream `prewarm: Vec<PrewarmStep>` on `SpawnSpec` (destination) vs a mado-side `Item` payload wrapper (interim, must not be enshrined). Then the Datadog CellSource + `Endpoint.base_url` kube-context resolver | queued |
| **M3** | `(defprewarm-catalog)` generated strategy table + the vigy authoring surface + read-intrinsics for live-state gating | queued |
| **M4** | Session-birth event trigger (OSC-1338 ingress) + fleet strategy library (OpsGenie/Grafana/JIT-creds) | queued |

### Ground-truth correction (first slice)

The recon's idealized `kubectl describe pod $identity` does **not** hold: a real VM
firing-alert `identity` is `alertname{k=v,…}` (e.g.
`OOMKilled{pod=api-1,severity=critical}`), not a bare pod name. So
`prewarm_for_signal` extracts the pod from the label set **best-effort**
(`pod_from_identity`) and the always-correct steps (kube-context from `env`, open
the deep-link) never depend on that parse — a signal with no extractable pod
degrades to context + link, never a nonsense `describe pod OOMKilled{…}`. Every
command re-flows through the injection guard (a `pod=api\n…` label can't build a
step).

## First slice (the smallest end-to-end)

**VictoriaMetrics OOMKilled** — the only *shipped* CellSource, so no new source
(sidesteps gap-A). One `(defprewarm)` form → `PrewarmSpec [KubeContext($env),
RunCommand("kubectl describe pod $identity"), OpenUrl($link)]` built in
`safra/project.rs` for a VM `oom-kills`/Critical Signal, executed via the
`PrewarmExecutor`. Tests: proptest control-byte rejection per-step (≥100 cases);
a mocked-`PrewarmEnv` order test; a golden-fixture wire-compat test.

## Guardrails

- **PTY injection** — prewarm commands are typed templates with `$field`
  placeholders filled from Signal *fields*, every step re-flowed through the
  control-byte border; **never** `format!()` of raw label text (also a TYPED
  EMISSION violation).
- **No write-intrinsics** — vigy decides + emits typed data; the executor
  actuates. `(mado-send-keys)` is the anti-pattern.
- **Env ordering** — `SetEnv` is pre-spawn; the `Vec` is not a flat post-spawn
  sequence (documented in the type).
- **Over-abstraction** — generate the strategy *table*, keep the reader
  hand-crafted; no `#[derive(TataraDomain)]` ceremony for a small ergonomic form.
