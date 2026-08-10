# mado notifications — the dream system

> Destination-first (Operating Principle #0). This document names the
> *pinnacle* notification system for mado, then the phased path to it.
> The primitive is `tsuuchi` (the fleet notification library); mado is
> its richest consumer.

## The problem we started from

mado's macOS notification backend (`OsaScriptBackend`) *unconditionally*
shelled out to `/usr/bin/osascript` running an AppleScript
`display notification`. Consequences:

- Banners were attributed to **Script Editor**, not mado (wrong name,
  wrong icon).
- It tripped the macOS **automation-permission popup** ("… wants to
  control Script Editor") — the reported symptom.
- `urgency` and `group` were silently dropped (osascript has no surface
  for them).
- A `fork`+`exec` of osascript per notification, plus a zombie-reaping
  thread — a NO-SHELL / TYPED-EMISSION violation.

The historical reason for osascript: an *unbundled* CLI binary cannot use
`UNUserNotificationCenter` (it aborts without a bundle identifier) and
`NSUserNotificationCenter` returns nil there. But mado ships as
**`Mado.app`** (`CFBundleIdentifier = io.pleme.mado`), which *has* a
bundle id — so the native, rich path is available whenever mado runs
bundled.

## The destination

A **native, typed, rich, focus-aware** notification system:

1. **`tsuuchi` is the rich vocabulary** (the load-bearing primitive, not
   a mado-local hack). Extended with sound, actions (buttons + reply),
   category, attachment (image), stable id (update-in-place), timeout,
   and an `Urgency → interruption-level` mapping. Backward-compatible.
   Every fleet consumer (tobira, hikyaku, ayatsuri, …) inherits the
   richness for free.
2. **A native `UNUserNotificationCenter` backend** in mado: proper
   mado-attributed banners, sound, interruption levels, threading,
   attachments — **no osascript, no Script-Editor popup**. The one
   permission prompt it shows is the normal "Mado would like to send
   notifications", under mado's own name.
3. **Focus-aware, coalesced, rate-limited orchestration**: only-when-
   unfocused delivery by default, storm coalescing, samba-style rate
   limiting, quiet-hours / mute, threading, history, action routing.
4. **Every terminal notification protocol** feeds it: OSC 9 (iTerm2),
   OSC 777 (urxvt/foot), OSC 99 (kitty rich protocol), BEL, OSC 1337
   RequestAttention.
5. **mado-native event sources**: long-command-completion notify (the
   killer feature — "✓ `cargo build` finished in 2m 14s" when you're not
   looking), background-pane activity, session exit.
6. **One typed shikumi `NotificationsConfig`** governs every axis, with
   the HM/NixOS/Darwin module surface.

The illegal state — "notification attributed to Script Editor / the
automation popup" — becomes **unrepresentable** in a bundled build: the
osascript path is demoted to explicit opt-in; the default path is native.

### The fleet plane takes precedence (2026-08-10)

`Auto` now prefers **shirase** — `tsuuchi::ShiraseBackend`, a Unix socket to
the pleme-io notification daemon — and only falls through to
`UNUserNotificationCenter` when that socket is absent. The reason is a fleet
posture, not a preference: `blackmatter.components.macos.quiet` disables
`com.apple.notificationcenterui.agent`, and the native backend is built on
`UNUserNotificationCenter`, so it goes down with the agent. A machine without
shirase behaves exactly as it did before.

Two escapes, both explicit and neither silent: `native` skips shirase and
uses Apple's agent; `shirase` demands the fleet plane and falls back to
**log, never to Apple's agent** — substituting the thing the operator was
trying to shed would hide the dependency rather than report it.

Honest scope: this routes what **mado emits**. Notifications from
*third-party* apps cannot be re-routed at all — macOS exposes no public API
for one process to observe another's user notifications, which is a fact
about the platform rather than a gap in this design.

## Layers

| Layer | What | Where |
|---|---|---|
| **L0** | `tsuuchi` vocabulary: `NotificationSound`, `NotificationAction`/`ActionKind`, `NotificationAttachment`, `category`/`id`/`timeout`/`icon` fields, `Capabilities`, `Urgency::interruption_level()` | `tsuuchi/src/notification.rs`, `backend.rs` |
| **L1** | native `UNUserNotificationCenter` backend + bundle detection + graceful fallback (dock/log); osascript opt-in | `mado/src/notify_mac.rs`, `platform.rs::notification_dispatcher()` |
| **L1′** | **shirase (fleet plane), preferred by `Auto` when its socket is live** | `tsuuchi::ShiraseBackend`, `platform.rs::notification_dispatcher()` |
| **L2** | terminal protocols (already parsed; extended for OSC 99 richness) | `mado/src/terminal.rs` OSC dispatch |
| **L3** | mado-native sources: command-completion (OSC 133), attention, session-exit | `mado/src/terminal.rs`, `ux/side_effects.rs` |
| **L4** | orchestrator: focus-gating, coalesce, rate-limit, DND/mute, history, routing | `mado/src/notify/center.rs` |
| **L5** | shikumi `NotificationsConfig` (+ nix module surface) | `mado/src/config.rs`, `blackmatter-mado` |
| **L6** | MCP `notify_send` / `notifications_list` / mute + a live `notify-test` | `mado/src/mcp.rs`, `main.rs` |

## The `Urgency → macOS interruption level` mapping (closes the dropped-urgency gap)

| tsuuchi `Urgency` | `UNNotificationInterruptionLevel` | Sound default |
|---|---|---|
| `Low` | `Passive` (no banner intrusion, lands quietly) | Silent |
| `Normal` | `Active` (standard banner + sound) | Default |
| `Critical` | `TimeSensitive` (breaks through Focus modes) | Critical |

(`Critical`/alert level proper needs an Apple entitlement; we use
`TimeSensitive`, which needs none and still pierces Focus.)

## Focus policy (`when`)

Every notification carries an effective delivery policy:

- `always` — deliver regardless of focus.
- `unfocused` (default) — deliver only when mado is **not** the key
  window. This is the standard terminal UX.
- `invisible` — deliver only when mado is not visible at all.

macOS already suppresses banners while your app is frontmost, so
`unfocused` composes naturally with the native backend; the orchestrator
enforces it explicitly for the log/dock fallbacks and for accurate
history.

## Tier honesty (UNREPRESENTABILITY discipline)

- **Truly fixed:** the Script-Editor popup is gone in the bundled build —
  the default backend is native; osascript is opt-in only.
- **Parse-time / typed:** urgency now has a real surface (interruption
  level); OSC 99 fields parse into the typed `PendingNotification`.
- **Only-mitigated (named M2):** action-button *routing* back into mado
  (the `UNUserNotificationCenterDelegate` `didReceiveResponse` path) and
  foreground-presentation delegate. v1 carries actions in the vocabulary
  and registers categories so buttons *appear*; routing their taps is
  M2. This is the existing "honest partial mapping, never silently
  dropped" discipline — unsupported axes are traced, not dropped.

## Emitting escapes — the typed `vt` surface

Outbound terminal escapes (the `notify-test` demo, and anything mado writes
back to a PTY) are built through the typed emitters in `src/vt.rs` — the
OSC peer of the existing `csi()` / `dcs()` / `apc()` builders:
`osc(code, params, terminator)` plus `osc9_notify` / `osc777_notify` /
`osc99_notify` / `osc1337_request_attention` / `osc133(Osc133Mark)` (the
shell-integration prompt marks the `feedback-test` demo emits). The call
site declares the numeric code and typed params (never the escape bytes),
and every builder is byte-pinned by tests. This is the ★★ TYPED EMISSION
rule applied to terminal syntax — no hand-spelled `\x1b]9;…` control strings.

## Watching your commands (OSC 133 completion) — shipped

mado brackets every shell command with OSC 133 marks (`C` output-start →
`D;<exit>` end) and turns the span into two peripheral cues:

- **Exit-status glow** — the cursor glow pulses **green on a clean exit,
  red on a failure** (`feedback.exit_code_glow`, prescribed-on). The colours
  are the active theme's `exit_ok` / `exit_err` (the ANSI green/red slots),
  so the pulse tracks the theme. Policy (`ux/side_effects.rs::should_exit_glow`):
  a failure *always* pulses (a fast failure is exactly when you want the
  cue); a success pulses only when it ran ≥ 2 s (an `ls` never strobes); a
  TUI (`used_alt_screen`) never pulses (you just quit an editor). This rides
  the one tintable engawa `glow_on_bell` effect — `ring_tinted(rgb)` — so the
  bell ring and the exit pulse share one GPU pass.
- **Away-notification** — a slow command finishing while you're in another
  app raises a **"✓ Command finished" / "✗ Command failed"** banner with the
  humanized runtime (`CommandCompletionConfig::should_notify`: enabled ∧
  not-a-TUI ∧ ≥ `min_duration_ms` (10 s) ∧ outcome-wanted ∧ unfocused).
  `should_notify` already applies the focus gate, so the dispatch is
  `NotifyWhen::Always` — no double-gate drops it.

The raw fact (`CommandCompletion { exit_code, duration_ms, used_alt_screen }`)
is emitted by `Terminal` and *all* policy lives in `apply_side_effects` +
config — the signal stays a pure fact (TYPED-SPEC discipline). Try it inside
a Mado.app window with `mado feedback-test`.

## Milestones

- **M0 (this arc — shipped):** L0 tsuuchi extension · L1 native backend +
  fallback · L4 orchestrator (focus/coalesce/rate-limit/DND/history) ·
  L5 config · L2 OSC 9/777/99 routed through the focus-gated center ·
  L6 `notify-test` (typed `vt` OSC emitters). **The popup dies; rich
  native banners land; the try-it-together path is ready.**
- **M1 (command-watching — shipped):** L3 command-completion — the raw
  `(exit, duration, used_alt_screen)` signal from `Terminal`, decided in
  `apply_side_effects` against the `command_completion` config, drives both
  the away-**notification** and the exit-status **glow** (see "Watching your
  commands" above). `mado feedback-test` demos it. **Remaining M1:** richer
  OSC 99
  parse (actions / sound / `o=` policy / `c=close`) · action-response
  delegate (route taps: Focus pane / Copy / Open / Reply-inject) ·
  foreground-presentation delegate · attachments delivery · MCP
  `notify_send` / `notifications_list` / mute.
- **M2:** Linux `libnotify` backend parity; converge with `shirase` (the
  notification *center*/observe side) on one action model.
