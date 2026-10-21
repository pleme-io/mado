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
- **Away-notification** — a command that finishes while you're in another
  app raises a structured card (see "Relevant cards" below).
  `CommandCompletionConfig::rank` decides: not-a-TUI ∧ outcome-wanted ∧
  (a failure, or a success that ran ≥ `min_duration_ms` (10 s)) ∧ unfocused.
  `should_notify` already applies the focus gate, so the dispatch is
  `NotifyWhen::Always` — no double-gate drops it.

The raw fact (`CommandCompletion { exit_code, duration_ms, used_alt_screen, command }`)
is emitted by `Terminal` and *all* policy lives in `apply_side_effects` +
config — the signal stays a pure fact (TYPED-SPEC discipline). Try it inside
a Mado.app window with `mado feedback-test`.

## Pane activity alerts — shipped

A long agent session (Claude Code) in a background mado window no longer has
to be polled. While the window is unfocused, `src/pane_alert.rs` watches the
pane and raises an alert for one of four reasons:

| Reason | Fires when | Default |
|---|---|---|
| `waiting_for_input` | a prompt shape from the typed table (`PROMPT_SHAPES`) has stayed on the bottom 24 rows for `prompt_settle_ms`: Claude Code's input box (a `❯` line under a `─` rule, with no "esc to interrupt" spinner), its permission prompt ("Do you want to…" + `❯ 1.`), a `(y/n)`-style confirm, a password or passphrase prompt | notify |
| `long_command` | an OSC 133 `D` closes a command that passes `notifications.command_completion` (≥ `min_duration_ms`, not a TUI, outcome wanted) | notify |
| `idle` | output ran for `idle_sustain_secs` (gaps under 5 s) and then stopped for `idle_quiet_secs` | badge |
| `bell` | BEL | badge |

An alert prefixes the window title with `badge` (visible in the native tab
label, the Window menu and window switchers; mado's flush titlebar hides the
title text itself), adds one to the dock badge (`badge_unread`), and with
`flash` plays the visual-bell flash in the window and bounces the dock once.
A `notify` reason also sends one card through the notification center, so it
goes to shirase when its socket is live (`backend: auto`). It is published to
the kanshou leaf `pane_alert`, which `attention_get` forwards as `pane_alert`.
Focusing the window clears it.

The detector is a pure state machine (`Detector::step(now, Input)`), so every
rule is a unit test: one alert at a time, a higher reason escalates the badge,
each reason flashes at most once per alert, and nothing flashes twice inside
`cooldown_secs`, which a focus round-trip does not reset. A prompt already on
screen when focus leaves was seen, so it does not alert; the idle timer and the
prompt-settle timer are frame deadlines, so an idle window wakes only when one
is due.

When pane alerts are enabled and `long_command` is not `off`, they own the
long-command banner and the older `command_completion` banner stays quiet, so a
finished command produces one notification, not two.

```yaml
notifications:
  pane_alerts:
    enabled: true
    badge: "● "
    flash: true
    idle_sustain_secs: 10
    idle_quiet_secs: 30
    prompt_settle_ms: 750
    cooldown_secs: 10
    reasons:
      waiting_for_input: notify
      long_command: notify
      idle: badge
      bell: badge
```

Scope: mado renders one tear pane per window, so "pane not focused" is "its
window is not key". Panes that no mado window is attached to send mado no
bytes and are not watched; covering them needs tear to publish per-pane
activity.

## Relevant cards — shipped

`notifications.relevance` (prescribed on) replaces the generic banner with a
card that carries the fact. `enabled: false` restores the old generic banner.

| Field | Command finished | Waiting for input |
|---|---|---|
| title | the command line, one line, cut at `title_max_chars` (80) with `…`; "Command finished" / "Command failed" when no text was captured | "Waiting for input" |
| subtitle | pane title, else working dir (OSC 7) | same |
| body | `✅ exit 0 · 2m 14s` or `❌ exit 1 · 1m 34s` (`humanize_duration`) | `⏳ ` + the prompt shape ("Claude Code is waiting for input", "Waiting for a password", …) |
| category | `command-finished` | `waiting-for-input` |
| group | the pane id | the pane id |
| id | `pane-<pane id>` | `pane-<pane id>` |
| action | `focus_pane` ("Focus"), when `focus_action` | same |

The id is one per pane, so a newer card replaces the older one on backends that
update in place. The pane id is the tear `PaneId` in the embedded and daemon
loops and `local` in the local-PTY loop.

Rank, all of it config:

| Event | Urgency (key) | Notifies when unfocused |
|---|---|---|
| failure | `relevance.failure` = normal | always, whatever its duration (`command_completion.failure_ignores_min_duration`, on) |
| slow success | `relevance.slow_success` = low | only past `command_completion.min_duration_ms` |
| waiting for input | `relevance.waiting_for_input` = normal | when `pane_alerts.reasons.waiting_for_input` is `notify` |
| fast success | none | never |

A TUI exit (`respect_alt_screen`) never notifies. One finished command makes one
notification: pane alerts own it while `long_command` is not `off`, and the
`command_completion` banner stays quiet.

**Command text.** `Terminal` records it as a fact on `CommandCompletion.command`,
never in policy. Source order: the `cmdline_url=<percent-encoded>` parameter on
OSC 133 `C` (kitty's spelling), else the text between the `B` mark and the `C`
mark on the primary grid. A shell that emits neither `B` nor `cmdline_url` (the
bundled zsh and fish integrations emit no `B`) gets the "Command finished" /
"Command failed" title, never a guessed command. bash emits `B`.

## Focus back — shipped

`notifications.focus` (prescribed on) opens a Unix socket,
`notifications.focus.socket`, default `$XDG_RUNTIME_DIR/mado-focus.sock`, else
`$TMPDIR/mado-focus.sock`, mode 0600. One client sends one JSON line:

```
{"focus_pane":"<group>"}\n   ->   {"ok":true}\n
```

`<group>` is the card's `group`. Other lines are refused with
`{"ok":false,"error":"malformed|empty-pane|unsupported-action"}`; the wire is
`tsuuchi::ActionMessage` (`{"action":"<id>"}` is its other shape, refused here).
`ok:true` means queued. On the next event-loop tick mado raises the app and its
window, clears the pane alert, and, when the group is another live pane and
`tear.session_switching` is on, switches the window to it (the `switch_session`
channel). A group that is neither displayed nor switchable is logged and
ignored. One socket per path: a second mado finds it live and does not take it.

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
- **M1.5 (relevance and focus-back — shipped):** structured cards, rank by
  config, `focus_pane` socket. **Open:** the card reaches shirase's click only
  when the backend sends `action_socket`; that needs a tsuuchi release with
  `ShiraseBackend::with_action_socket` (in tsuuchi's tree, not in 0.1.5), then
  one call in `platform::notification_dispatcher` with
  `focus_server::default_socket_path()`.
- **M2:** Linux `libnotify` backend parity; converge with `shirase` (the
  notification *center*/observe side) on one action model.
