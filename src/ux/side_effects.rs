//! [`TerminalSideEffects`] — the typed drain payload for everything
//! the VT engine accumulates as a side effect of feeding bytes —
//! plus the typed vocabulary the Terminal queues those effects in
//! ([`PendingNotification`], [`Urgency`], [`ProgressState`]) and the
//! ONE shared consumer ([`apply_side_effects`]) both render modes
//! dispatch the drained payload through.
//!
//! M4 closes the seam M1 deferred: `Terminal::drain_side_effects()`
//! is the single producer, and both event loops (`main.rs`
//! RedrawRequested + the `gui_tear_attach` owning loop) route the
//! drained value through [`apply_side_effects`] — the per-loop
//! `take_bell` / `take_clipboard` / title-diff polling copies are
//! deleted and BANNED by `tests/ux_unification.rs`'s drain markers.
//! The drain is pure state transfer: same pre-state drains the same
//! value; an immediately repeated drain yields the default payload.

/// One typed urgency vocabulary for the whole pipeline — the OSC 99
/// `u=` parse (`0`=Low, `1`=Normal, `2`=Critical), the queue entry,
/// and the tsuuchi dispatch all share `tsuuchi::Urgency` (re-export,
/// not a mirror — per the org single-definition rule). OSC 9 and
/// OSC 777 carry no urgency and default to `Normal`; BEL routes
/// `Normal`; OSC 1337 `RequestAttention` routes `Critical`.
pub use tsuuchi::Urgency;

/// One typed desktop notification queued by the terminal (OSC 9 /
/// OSC 777;notify / OSC 99). The queue carries ONLY notifications —
/// `ConEmu` progress (OSC 9;4) lives in its own typed lane
/// ([`ProgressState`]) and cannot be represented here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotification {
    /// Title line. OSC 9 carries body-only (`None`); OSC 777 carries
    /// an explicit title param; OSC 99 fills this from `p=title`
    /// payload parts.
    pub title: Option<String>,
    /// Body text. OSC 9's whole payload; OSC 777's body param;
    /// OSC 99's `p=body` payload parts.
    pub body: String,
    /// Urgency — only OSC 99 (`u=`) can express non-Normal.
    pub urgency: Urgency,
    /// Collapse/replace group. Only OSC 99's `i=<id>` populates this
    /// (re-notifying with the same id is replace semantics); OSC 9 /
    /// OSC 777 have no group vocabulary.
    pub group: Option<String>,
}

impl PendingNotification {
    /// Project the queue entry onto tsuuchi's dispatch type. A
    /// body-only OSC 9 entry titles as "mado" so the OS banner names
    /// its origin; urgency and group carry through typed.
    fn into_tsuuchi(self) -> tsuuchi::Notification {
        let title = self.title.unwrap_or_else(|| "mado".to_owned());
        let n = tsuuchi::Notification::new(title, self.body).urgency(self.urgency);
        match self.group {
            Some(g) => n.group(g),
            None => n,
        }
    }
}

/// `ConEmu` OSC 9;4 progress state (`ESC ] 9 ; 4 ; st ; pr ST`).
/// `st`: 0=remove, 1=set value, 2=error, 3=indeterminate, 4=paused.
/// `pr` is an integer percentage 0..=100 (clamped at parse).
///
/// Progress is NOT a notification: it lives in this separate typed
/// lane, so a progress update firing a desktop notification is
/// unrepresentable — there is no constructor from one to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressState {
    /// `st=0` — remove the progress indicator.
    Remove,
    /// `st=1` — determinate progress at `pct` percent.
    Set {
        /// 0..=100, clamped at the parse boundary.
        pct: u8,
    },
    /// `st=2` — error state; `pct` retains the last value when sent.
    Error {
        /// Optional 0..=100 percentage accompanying the error.
        pct: Option<u8>,
    },
    /// `st=3` — indeterminate (busy spinner, no percentage).
    Indeterminate,
    /// `st=4` — paused; `pct` retains the last value when sent.
    Paused {
        /// Optional 0..=100 percentage at which progress paused.
        pct: Option<u8>,
    },
}

/// One frame's worth of terminal side effects, drained atomically by
/// `Terminal::drain_side_effects()` and dispatched by
/// [`apply_side_effects`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TerminalSideEffects {
    /// OSC 0/2 window title, when it changed since the last drain.
    pub title: Option<String>,
    /// BEL was received (visual bell + notification urgency feed).
    pub bell: bool,
    /// OSC 52 clipboard write payload to sync to the system clipboard.
    pub clipboard: Option<String>,
    /// Pending notifications (OSC 9 / 777 / 99), typed.
    pub notifications: Vec<PendingNotification>,
    /// `ConEmu` OSC 9;4 progress — its own typed lane, never a
    /// notification (different field, not a filtered queue entry).
    pub progress: Option<ProgressState>,
    /// OSC 7 working directory, when it changed since the last drain.
    pub cwd: Option<String>,
    /// OSC 1337 `RequestAttention` rising edge since the last drain —
    /// dock bounce + critical-urgency dispatch.
    pub attention: bool,
    /// A shell command finished (OSC 133 `D`) since the last drain — the
    /// raw fact (exit + duration + whether a TUI ran). `apply_side_effects`
    /// decides the exit-status glow + the away-notification against config.
    pub command_completion: Option<CommandCompletion>,
}

/// One completed shell command, derived from the OSC 133 `C`→`D` span.
/// The *raw signal* — the policy (notify when away, skip TUIs, glow) lives
/// in `apply_side_effects` + the config, keeping this a pure fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandCompletion {
    /// The command's exit status (OSC 133 `D;<code>`).
    pub exit_code: i32,
    /// Wall-clock duration from `C` (output start) to `D` (end), ms.
    pub duration_ms: u64,
    /// Whether the alternate screen was entered during the command — i.e.
    /// it was an interactive TUI (vim/less/lazygit), so completion is not
    /// interesting (the operator just quit an editor).
    pub used_alt_screen: bool,
}

impl CommandCompletion {
    /// Whether the command exited cleanly.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

/// THE shared per-frame side-effect consumer — the single
/// implementation both event-loop adapters call with their drained
/// [`TerminalSideEffects`] (M1 ux-unification pattern; the drain
/// markers in `tests/ux_unification.rs` ban per-loop polling and
/// require this call in both adapters).
///
/// Routing:
/// - clipboard → system clipboard via the loop's `hasami` provider
/// - bell → renderer bell flash AND (when `bell.notify`) a focus-gated
///   Normal-urgency notification
/// - notifications → the notification center (focus-gated, coalesced,
///   rate-limited) with their typed urgency
/// - attention edge → Critical-urgency notification (always delivered) +
///   dock attention
/// - progress / cwd → typed trace (no renderer surface yet — the
///   progress UI lands with the renderer's M4 follow-up)
/// - title → returned to the adapter, which owns the
///   `EventResponse::set_title` translation (madori applies it)
///
/// Every OS-banner decision (focus policy, coalescing, rate limiting,
/// mute, the master switch) is owned by the [`NotificationCenter`]
/// (`crate::notify_center`); this function only names the *source* of
/// each notification.
pub fn apply_side_effects(
    effects: TerminalSideEffects,
    renderer: &mut crate::render::TerminalRenderer,
    clipboard: &dyn hasami::ClipboardProvider,
    notify: &mut crate::notify_center::NotificationCenter,
) -> Option<String> {
    let focused = renderer.focused();
    // Per-frame focus tick — clears the dock badge on the unfocused→
    // focused transition.
    notify.on_frame(focused);
    if let Some(clip) = effects.clipboard
        && let Err(e) = clipboard.copy_text(&clip)
    {
        tracing::warn!(error = %e, "OSC 52 clipboard sync failed");
    }
    if effects.bell {
        // The visual bell flash always fires; the native audible bell and
        // the desktop banner are opt-in (`notifications.bell.audible` /
        // `.notify`, default off — bells are frequent).
        renderer.trigger_bell();
        if notify.bell_audible() {
            crate::platform::ring_bell(notify.bell_sound_name());
        }
        if notify.bell_notify() {
            notify.notify(
                &tsuuchi::Notification::new("mado", "Bell")
                    .urgency(Urgency::Normal)
                    .group("bell"),
                focused,
            );
        }
    }
    for pending in effects.notifications {
        notify.notify(&pending.into_tsuuchi(), focused);
    }
    if effects.attention {
        // An explicit attention request always breaks through (it is the
        // program saying "I need the human"), so it bypasses the focus
        // gate via `NotifyWhen::Always`.
        notify.notify_with(
            &tsuuchi::Notification::new("mado", "Attention requested")
                .urgency(Urgency::Critical)
                .group("attention"),
            crate::config::NotifyWhen::Always,
            focused,
        );
        // Only bounce the dock when mado is NOT already the focused
        // window — bouncing to get attention you already have is noise.
        if !focused {
            crate::platform::request_dock_attention();
        }
    }
    if let Some(progress) = effects.progress {
        // OSC 9;4 command progress → the dock badge (via the center's
        // progress↔unread arbitration). The full in-window progress bar is
        // a follow-up; the dock surface lands now.
        use crate::notify_center::DockProgress;
        let dock = match progress {
            ProgressState::Remove => DockProgress::Clear,
            ProgressState::Set { pct } => DockProgress::Percent(pct),
            ProgressState::Indeterminate => DockProgress::Busy,
            ProgressState::Error { pct } | ProgressState::Paused { pct } => {
                pct.map_or(DockProgress::Busy, DockProgress::Percent)
            }
        };
        notify.set_progress(dock);
        tracing::debug!(?progress, "ConEmu progress → dock badge");
    }
    if let Some(cwd) = effects.cwd {
        tracing::trace!(%cwd, "OSC 7 cwd update");
    }
    if let Some(cc) = effects.command_completion {
        // Peripheral exit-status glow. A finished command pulses the cursor
        // glow green (clean) / red (failed). We skip TUIs (you just quit an
        // editor) and skip *fast successes* (an `ls` shouldn't strobe) — but
        // a failure ALWAYS pulses, however brief, because a fast failure is
        // exactly the moment the cue earns its keep. The renderer applies the
        // final `feedback.exit_code_glow` + reduce-motion gate.
        if should_exit_glow(&cc) {
            renderer.glow_on_exit_status(cc.exit_code);
        }
        // Away-notification for a long-running command. `should_notify`
        // already applied the focus gate (`only_when_unfocused`), so the
        // dispatch uses `Always` — double-gating here would silently drop it.
        if notify.command_completion().should_notify(&cc, focused) {
            notify.notify_with(&completion_notification(&cc), crate::config::NotifyWhen::Always, focused);
        }
    }
    effects.title
}

/// A clean command that ran at least this long (ms) still earns a success
/// glow; anything faster is background noise (`cd`, `ls`) and stays quiet.
/// Failures glow regardless of duration.
const EXIT_GLOW_SUCCESS_MIN_MS: u64 = 2_000;

/// Whether a finished command should pulse the exit-status glow — the pure
/// policy core (the renderer applies the config + reduce-motion gate on top).
/// Skip TUIs (you just quit an editor); a failure always pulses; a success
/// pulses only when it ran long enough to be worth noticing.
#[must_use]
fn should_exit_glow(cc: &CommandCompletion) -> bool {
    !cc.used_alt_screen && (!cc.succeeded() || cc.duration_ms >= EXIT_GLOW_SUCCESS_MIN_MS)
}

/// Build the desktop banner for a finished command — a typed
/// [`tsuuchi::Notification`], never a hand-spliced string. Title names the
/// outcome (✓/✗); body is the humanized runtime (+ exit code on failure).
/// Grouped `command` so a burst of completions coalesces to the latest.
fn completion_notification(cc: &CommandCompletion) -> tsuuchi::Notification {
    let dur = humanize_duration(cc.duration_ms);
    let (title, body) = if cc.succeeded() {
        ("✓ Command finished".to_owned(), format!("Done in {dur}"))
    } else {
        ("✗ Command failed".to_owned(), format!("Exit {} after {dur}", cc.exit_code))
    };
    tsuuchi::Notification::new(title, body).urgency(Urgency::Normal).group("command")
}

/// Humanize a millisecond duration into a compact, glanceable string:
/// `820ms`, `3.4s`, `1m 05s`, `1h 02m`. Pure + total (saturating), so it
/// is trivially testable and never panics on absurd inputs.
#[must_use]
fn humanize_duration(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let secs = ms / 1_000;
    if secs < 60 {
        // One decimal of seconds below a minute (3.4s reads better than 3s).
        let tenths = (ms % 1_000) / 100;
        return format!("{secs}.{tenths}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m {:02}s", secs % 60);
    }
    let hours = mins / 60;
    format!("{hours}h {:02}m", mins % 60)
}

#[cfg(test)]
mod completion_tests {
    use super::*;

    fn cc(exit_code: i32, duration_ms: u64, used_alt_screen: bool) -> CommandCompletion {
        CommandCompletion { exit_code, duration_ms, used_alt_screen }
    }

    #[test]
    fn humanize_covers_every_scale() {
        assert_eq!(humanize_duration(0), "0ms");
        assert_eq!(humanize_duration(820), "820ms");
        assert_eq!(humanize_duration(999), "999ms");
        assert_eq!(humanize_duration(1_000), "1.0s");
        assert_eq!(humanize_duration(3_400), "3.4s");
        assert_eq!(humanize_duration(59_900), "59.9s");
        assert_eq!(humanize_duration(60_000), "1m 00s");
        assert_eq!(humanize_duration(65_000), "1m 05s");
        assert_eq!(humanize_duration(3_600_000), "1h 00m");
        assert_eq!(humanize_duration(3_720_000), "1h 02m");
    }

    #[test]
    fn humanize_never_panics_on_extremes() {
        // Total + saturating — an absurd runtime must still format.
        let _ = humanize_duration(u64::MAX);
    }

    #[test]
    fn failure_always_glows_even_when_instant() {
        // A fast failure is exactly when the cue earns its keep.
        assert!(should_exit_glow(&cc(1, 5, false)));
    }

    #[test]
    fn fast_success_stays_quiet_slow_success_glows() {
        assert!(!should_exit_glow(&cc(0, 500, false)), "a quick `ls` must not strobe");
        assert!(should_exit_glow(&cc(0, 5_000, false)), "a 5s job earns a done-pulse");
    }

    #[test]
    fn a_tui_never_glows() {
        // Quitting vim/less is not a completion worth flashing — even a
        // non-zero editor exit stays quiet (the alt-screen filter wins).
        assert!(!should_exit_glow(&cc(0, 60_000, true)));
        assert!(!should_exit_glow(&cc(130, 60_000, true)));
    }

    #[test]
    fn notification_names_the_outcome() {
        let ok = completion_notification(&cc(0, 12_000, false));
        assert_eq!(ok.title, "✓ Command finished");
        assert_eq!(ok.body, "Done in 12.0s");

        let fail = completion_notification(&cc(2, 90_000, false));
        assert_eq!(fail.title, "✗ Command failed");
        assert_eq!(fail.body, "Exit 2 after 1m 30s");
        // Completions coalesce to the latest under one group.
        assert_eq!(fail.group.as_deref(), Some("command"));
    }
}
