//! Launch-perf timeline helpers, and the render, input and queue
//! instrumentation the `frame_perf` leaf exports (on `kanshou::metrics`).
//!
//! `set_launch_start` records the process's effective start
//! timestamp once (in `main`); `log_phase` then emits an
//! INFO-level tracing line stamping milliseconds-since-start
//! at every named milestone. The operator reads these out of
//! mado's stderr to see where time goes from binary-exec to
//! first frame.
//!
//! Cost: one `Instant::elapsed()` + one tracing call per
//! milestone. Tracing skips the formatting work entirely when
//! the level is filtered out, so this is effectively zero
//! when `RUST_LOG=warn` is set.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use kanshou::metrics::{Counter, Family, Gauge, Label, LogHistogram};

use crate::config::HistogramMode;
use crate::render::TerminalRenderer;

mod tear;

pub use tear::Counted;

static LAUNCH_START: OnceLock<Instant> = OnceLock::new();

/// Stamp the launch start instant. Call once in `main`
/// immediately after tracing is initialized.
pub fn set_launch_start(instant: Instant) {
    let _ = LAUNCH_START.set(instant);
}

/// Log a named launch-perf phase with elapsed-ms since start.
/// No-op (returns immediately) if `set_launch_start` was never
/// called.
pub fn log_phase(name: &'static str) {
    if let Some(start) = LAUNCH_START.get() {
        let ms = start.elapsed().as_millis();
        tracing::info!(target: "mado::perf", phase = name, ms, "launch phase");
    }
}

kanshou::metric_labels! {
    pub enum PaintReason {
        Content = "content",
        ForcedScrub = "forced_scrub",
        EpochScrub = "epoch_scrub",
        Overlay = "overlay",
        Animation = "animation",
        LateIdle = "late_idle",
    }
}

kanshou::metric_labels! {
    pub enum TearCall {
        Capabilities = "capabilities",
        ListSessions = "list_sessions",
        GetSession = "get_session",
        GetWindow = "get_window",
        GetPane = "get_pane",
        NewSession = "new_session",
        NewSessionWithSource = "new_session_with_source",
        NewSessionWithSourceAndSize = "new_session_with_source_and_size",
        NewSessionIn = "new_session_in",
        RenameSession = "rename_session",
        KillSession = "kill_session",
        NewWindow = "new_window",
        KillWindow = "kill_window",
        SelectWindow = "select_window",
        SplitPane = "split_pane",
        KillPane = "kill_pane",
        SelectPane = "select_pane",
        ResizePane = "resize_pane",
        ApplyLayout = "apply_layout",
        PaneResizeAbsolute = "pane_resize_absolute",
        SendKeys = "send_keys",
        PaneSubscriberCount = "pane_subscriber_count",
        SetInputPolicy = "set_input_policy",
        PaneSnapshot = "pane_snapshot",
        PaneCursorKeysMode = "pane_cursor_keys_mode",
        SetSpawnEnv = "set_spawn_env",
        WithRegistry = "with_registry",
        GetConfig = "get_config",
        SetConfig = "set_config",
        ProducerSnapshot = "producer_snapshot",
        ProducerSubscribe = "producer_subscribe",
    }
}

pub type TearCalls = Family<TearCall, { TearCall::COUNT }>;

kanshou::metric_labels! {
    pub enum TearWrite {
        Keys = "send_keys",
        Resize = "pane_resize_absolute",
        QueryAnswer = "query_answer",
        Prewarm = "prewarm_keys",
    }
}

pub type TearWrites = Family<TearWrite, { TearWrite::COUNT }>;

impl TearWrite {
    #[must_use]
    pub const fn consequence(self) -> &'static str {
        match self {
            TearWrite::Keys => "these keystrokes never reached the pane",
            TearWrite::Resize => "the pane keeps its old size",
            TearWrite::QueryAnswer => "the shell may stall on an unanswered DSR/DA/OSC query",
            TearWrite::Prewarm => "the prewarm command never reached the shell",
        }
    }
}

type TearWriteLogSlots = [AtomicU64; TearWrite::COUNT];

pub static UI_TEAR_CALLS: TearCalls = Family::new();
pub static TEAR_WRITE_FAILURES: TearWrites = Family::new();
static TEAR_WRITE_LOGGED_AT: TearWriteLogSlots = [const { AtomicU64::new(0) }; TearWrite::COUNT];
const TEAR_WRITE_LOG_EVERY_NS: u64 = 1_000_000_000;
pub static PARSED_BYTES: Counter = Counter::new();
pub static DRAINED_PER_WAKE: Gauge = Gauge::new();

static CLOCK: OnceLock<Instant> = OnceLock::new();
static RENDER: RenderMetrics = RenderMetrics::new();

thread_local! {
    static ON_UI_THREAD: Cell<bool> = const { Cell::new(false) };
    static DISPATCH_AT: Cell<u64> = const { Cell::new(0) };
    static UI_PARSED: Cell<u64> = const { Cell::new(0) };
}

#[must_use]
pub fn now_ns() -> u64 {
    let base = *CLOCK.get_or_init(Instant::now);
    u64::try_from(base.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
}

fn micros(ns: u64) -> u64 {
    ns / 1_000
}

pub struct UiThread(PhantomData<*const ()>);

impl UiThread {
    #[must_use]
    pub fn mark() -> Self {
        ON_UI_THREAD.with(|c| c.set(true));
        Self(PhantomData)
    }
}

#[must_use]
pub fn on_ui_thread() -> bool {
    ON_UI_THREAD.with(Cell::get)
}

pub(crate) fn count_tear_call(calls: &TearCalls, call: TearCall) {
    if on_ui_thread() {
        calls.inc(call);
    }
}

pub fn tear_write_failed(
    write: TearWrite,
    pane: tear_types::PaneId,
    len: usize,
    error: &tear_types::ControlError,
) {
    TEAR_WRITE_FAILURES.inc(write);
    if tear_write_log_due(&TEAR_WRITE_LOGGED_AT, write, now_ns()) {
        tracing::warn!(
            write = write.name(),
            pane = ?pane,
            len,
            error = %error,
            failures = TEAR_WRITE_FAILURES.get(write),
            "a write to tear failed: {}",
            write.consequence()
        );
    }
}

fn tear_write_log_due(slots: &TearWriteLogSlots, write: TearWrite, now: u64) -> bool {
    log_due(&slots[write.index()], now, TEAR_WRITE_LOG_EVERY_NS)
}

fn log_due(last: &AtomicU64, now: u64, every: u64) -> bool {
    let prev = last.load(Ordering::Relaxed);
    (prev == 0 || now.saturating_sub(prev) >= every)
        && last
            .compare_exchange(prev, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

pub fn note_parsed(bytes: usize) {
    let n = u64::try_from(bytes).unwrap_or(u64::MAX);
    PARSED_BYTES.add(n);
    if on_ui_thread() {
        UI_PARSED.with(|c| c.set(c.get().saturating_add(n)));
    }
}

#[must_use]
pub struct UiDispatch {
    parsed_at_begin: u64,
    thread: PhantomData<*const ()>,
}

impl UiDispatch {
    pub fn begin() -> Self {
        let _ui = UiThread::mark();
        DISPATCH_AT.with(|c| c.set(now_ns()));
        Self {
            parsed_at_begin: UI_PARSED.with(Cell::get),
            thread: PhantomData,
        }
    }

    pub fn end(self, metrics: &RenderMetrics) {
        metrics.parse_tick(
            UI_PARSED
                .with(Cell::get)
                .saturating_sub(self.parsed_at_begin),
        );
    }
}

impl Drop for UiDispatch {
    fn drop(&mut self) {
        DISPATCH_AT.with(|c| c.set(0));
    }
}

pub fn ui_dispatch<F>(
    mut handler: F,
) -> impl FnMut(&madori::AppEvent, &mut TerminalRenderer) -> madori::EventResponse
where
    F: FnMut(&madori::AppEvent, &mut TerminalRenderer) -> madori::EventResponse,
{
    move |event: &madori::AppEvent, renderer: &mut TerminalRenderer| {
        let dispatch = UiDispatch::begin();
        let response = handler(event, renderer);
        dispatch.end(renderer.render_metrics());
        response
    }
}

fn dispatch_or_now() -> u64 {
    match DISPATCH_AT.with(Cell::get) {
        0 => now_ns(),
        at => at,
    }
}

#[derive(Debug, Default)]
pub struct Arrivals {
    oldest: AtomicU64,
    newest: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameArrivals {
    pub oldest: Option<u64>,
    pub newest: Option<u64>,
}

impl Arrivals {
    pub fn note(&self, at: u64) {
        let _ = self
            .oldest
            .compare_exchange(0, at, Ordering::Relaxed, Ordering::Relaxed);
        self.newest.fetch_max(at, Ordering::Relaxed);
    }

    #[must_use]
    pub fn take(&self) -> FrameArrivals {
        let some = |v: u64| (v != 0).then_some(v);
        FrameArrivals {
            oldest: some(self.oldest.swap(0, Ordering::Relaxed)),
            newest: some(self.newest.load(Ordering::Relaxed)),
        }
    }
}

#[derive(Debug)]
pub struct LatencyProbe {
    pending_input: AtomicU64,
    input_to_present_us: LogHistogram,
    byte_to_present_us: LogHistogram,
}

impl LatencyProbe {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pending_input: AtomicU64::new(0),
            input_to_present_us: LogHistogram::new(),
            byte_to_present_us: LogHistogram::new(),
        }
    }

    fn arm(&self) {
        let _ = self.pending_input.compare_exchange(
            0,
            dispatch_or_now(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    fn disarm(&self) {
        self.pending_input.store(0, Ordering::Relaxed);
    }

    fn close(&self, arrivals: FrameArrivals, now: u64) {
        if let Some(oldest) = arrivals.oldest {
            self.byte_to_present_us
                .record(micros(now.saturating_sub(oldest)));
        }
        let pending = self.pending_input.load(Ordering::Relaxed);
        if pending != 0
            && arrivals.newest.is_some_and(|newest| newest > pending)
            && self
                .pending_input
                .compare_exchange(pending, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.input_to_present_us
                .record(micros(now.saturating_sub(pending)));
        }
    }

    #[must_use]
    pub fn input_to_present(&self) -> &LogHistogram {
        &self.input_to_present_us
    }

    #[must_use]
    pub fn byte_to_present(&self) -> &LogHistogram {
        &self.byte_to_present_us
    }
}

impl Default for LatencyProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct RenderMetrics {
    paints: Family<PaintReason, { PaintReason::COUNT }>,
    declined_after_acquire: Counter,
    latency: LatencyProbe,
    parse_bytes_per_tick: LogHistogram,
    histograms: AtomicBool,
}

impl RenderMetrics {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            paints: Family::new(),
            declined_after_acquire: Counter::new(),
            latency: LatencyProbe::new(),
            parse_bytes_per_tick: LogHistogram::new(),
            histograms: AtomicBool::new(true),
        }
    }

    #[must_use]
    pub fn paints(&self) -> &Family<PaintReason, { PaintReason::COUNT }> {
        &self.paints
    }

    #[must_use]
    pub fn declined_after_acquire(&self) -> &Counter {
        &self.declined_after_acquire
    }

    #[must_use]
    pub fn latency(&self) -> &LatencyProbe {
        &self.latency
    }

    #[must_use]
    pub fn parse_bytes_per_tick(&self) -> &LogHistogram {
        &self.parse_bytes_per_tick
    }

    #[must_use]
    pub fn histograms(&self) -> HistogramMode {
        if self.recording() {
            HistogramMode::On
        } else {
            HistogramMode::Off
        }
    }

    pub fn set_histograms(&self, mode: HistogramMode) {
        self.histograms.store(mode.records(), Ordering::Relaxed);
        if !mode.records() {
            self.latency.disarm();
        }
    }

    fn recording(&self) -> bool {
        self.histograms.load(Ordering::Relaxed)
    }

    pub fn input_written(&self) {
        if self.recording() {
            self.latency.arm();
        }
    }

    pub fn presented(&self, arrivals: FrameArrivals, now: u64) {
        if self.recording() {
            self.latency.close(arrivals, now);
        }
    }

    pub fn parse_tick(&self, bytes: u64) {
        if bytes > 0 && self.recording() {
            self.parse_bytes_per_tick.record(bytes);
        }
    }
}

impl Default for RenderMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn render_metrics() -> &'static RenderMetrics {
    &RENDER
}

#[must_use]
pub fn window_bell(window: std::task::Waker) -> std::task::Waker {
    #[cfg(feature = "bench-probes")]
    return faults::bell_for(window, faults::armed().contains(&faults::Fault::WakeOff));
    #[cfg(not(feature = "bench-probes"))]
    window
}

#[cfg(feature = "bench-probes")]
pub mod faults {
    use std::sync::OnceLock;

    pub use tear_types::probes::{FAULTS_ENV, Fault, FaultList};

    #[must_use]
    pub fn from_list(list: &str) -> Vec<Fault> {
        let list = FaultList::parse(list);
        for entry in &list.refused {
            tracing::warn!(entry = %entry, "{FAULTS_ENV}: names no fault and was ignored");
        }
        list.faults
    }

    #[must_use]
    pub fn armed() -> &'static [Fault] {
        static ARMED: OnceLock<Vec<Fault>> = OnceLock::new();
        ARMED.get_or_init(|| {
            std::env::var(FAULTS_ENV)
                .map(|v| from_list(&v))
                .unwrap_or_default()
        })
    }

    #[must_use]
    pub fn bell_for(window: std::task::Waker, wake_off: bool) -> std::task::Waker {
        if wake_off {
            std::task::Waker::noop().clone()
        } else {
            window
        }
    }

    #[must_use]
    pub fn report() -> serde_json::Value {
        serde_json::Value::from(armed().iter().map(|f| f.name()).collect::<Vec<_>>())
    }
}

#[must_use]
pub fn frame_perf() -> serde_json::Value {
    use crate::render::{
        LAST_FRAME_RECTS, LAST_FRAME_SHAPE_CACHE, LAST_FRAME_TEXT, LAST_FRAME_US, TOTAL_FRAMES,
        TOTAL_FRAMES_SKIPPED, TOTAL_LATE_IDLE_PAINTS,
    };
    let perf = serde_json::json!({
        "last_frame_us": LAST_FRAME_US.load(Ordering::Relaxed),
        "last_frame_rects": LAST_FRAME_RECTS.load(Ordering::Relaxed),
        "last_frame_text": LAST_FRAME_TEXT.load(Ordering::Relaxed),
        "last_frame_shape_cache": LAST_FRAME_SHAPE_CACHE.load(Ordering::Relaxed),
        "total_frames": TOTAL_FRAMES.load(Ordering::Relaxed),
        "total_frames_skipped": TOTAL_FRAMES_SKIPPED.load(Ordering::Relaxed),
        "total_late_idle_paints": TOTAL_LATE_IDLE_PAINTS.load(Ordering::Relaxed),
        "paints": RENDER.paints(),
        "declined_after_acquire": RENDER.declined_after_acquire(),
        "ui_thread_tear_calls": &UI_TEAR_CALLS,
        "ui_thread_tear_calls_total": UI_TEAR_CALLS.total(),
        "tear_write_failures": &TEAR_WRITE_FAILURES,
        "tear_write_failures_total": TEAR_WRITE_FAILURES.total(),
        "queues": {
            "subscribe": {
                "drained_per_wake": &DRAINED_PER_WAKE,
            },
        },
        "parse": {
            "bytes_total": &PARSED_BYTES,
            "ui_bytes_per_tick": RENDER.parse_bytes_per_tick(),
        },
        "latency_us": {
            "input_to_present": RENDER.latency().input_to_present(),
            "byte_to_present": RENDER.latency().byte_to_present(),
        },
        "histograms": RENDER.histograms(),
    });
    #[cfg(feature = "bench-probes")]
    let perf = {
        let mut perf = perf;
        perf["bench_faults"] = faults::report();
        perf
    };
    perf
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanshou::metrics::Label;

    fn frame(oldest: Option<u64>, newest: Option<u64>) -> FrameArrivals {
        FrameArrivals { oldest, newest }
    }

    fn fresh() -> RenderMetrics {
        RenderMetrics::new()
    }

    #[test]
    fn a_tear_call_counts_only_on_the_marked_thread() {
        let calls: &'static TearCalls = Box::leak(Box::new(Family::new()));
        std::thread::spawn(move || count_tear_call(calls, TearCall::SelectWindow))
            .join()
            .unwrap();
        assert_eq!(calls.total(), 0, "an unmarked thread counts nothing");
        std::thread::spawn(move || {
            let _ui = UiThread::mark();
            for _ in 0..3 {
                count_tear_call(calls, TearCall::ApplyLayout);
            }
            count_tear_call(calls, TearCall::KillWindow);
        })
        .join()
        .unwrap();
        assert_eq!(calls.get(TearCall::ApplyLayout), 3);
        assert_eq!(calls.get(TearCall::KillWindow), 1);
        assert_eq!(calls.total(), 4);
    }

    #[test]
    fn a_dispatch_marks_the_ui_thread_stamps_its_keys_and_ends_with_them() {
        std::thread::spawn(|| {
            assert!(!on_ui_thread());
            let m = fresh();
            let dispatched_at = {
                let dispatch = UiDispatch::begin();
                assert!(on_ui_thread(), "the event loop's thread is the UI thread");
                let at = DISPATCH_AT.with(Cell::get);
                m.input_written();
                m.input_written();
                dispatch.end(&m);
                at
            };
            assert_ne!(dispatched_at, 0);
            assert_eq!(
                m.latency.pending_input.load(Ordering::Relaxed),
                dispatched_at
            );
            assert_eq!(
                DISPATCH_AT.with(Cell::get),
                0,
                "a dispatch stamp ends with its dispatch"
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_dispatch_records_the_bytes_parsed_on_the_ui_thread_inside_it() {
        std::thread::spawn(|| {
            let m = fresh();
            note_parsed(7);
            let dispatch = UiDispatch::begin();
            note_parsed(100);
            note_parsed(28);
            std::thread::spawn(|| note_parsed(5_000)).join().unwrap();
            dispatch.end(&m);
            UiDispatch::begin().end(&m);
            let s = m.parse_bytes_per_tick().snapshot();
            assert_eq!(s.count, 1, "a dispatch that parsed nothing is no sample");
            assert_eq!(
                s.sum, 128,
                "only this thread's bytes inside the dispatch count"
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn one_key_and_its_echo_make_exactly_one_input_sample() {
        let m = fresh();
        m.latency.pending_input.store(1_000_000, Ordering::Relaxed);
        m.presented(frame(None, None), 2_000_000);
        m.presented(frame(Some(900_000), Some(900_000)), 3_000_000);
        assert_eq!(
            m.latency().input_to_present().count(),
            0,
            "bytes older than the key cannot answer it"
        );
        assert_eq!(m.latency().byte_to_present().count(), 1);
        m.presented(frame(Some(1_500_000), Some(1_500_000)), 6_000_000);
        for _ in 0..3 {
            m.presented(frame(None, Some(1_500_000)), 7_000_000);
        }
        let s = m.latency().input_to_present().snapshot();
        assert_eq!(s.count, 1);
        assert_eq!(s.max, Some(5_000));
        assert_eq!(m.latency().byte_to_present().snapshot().count, 2);
    }

    #[test]
    fn histograms_off_records_nothing_and_disarms_the_waiting_key() {
        let m = fresh();
        assert_eq!(m.histograms(), HistogramMode::On);
        m.input_written();
        assert_ne!(m.latency.pending_input.load(Ordering::Relaxed), 0);
        m.set_histograms(HistogramMode::Off);
        assert_eq!(m.histograms(), HistogramMode::Off);
        assert_eq!(
            m.latency.pending_input.load(Ordering::Relaxed),
            0,
            "a key armed before off cannot close after on"
        );
        m.input_written();
        assert_eq!(m.latency.pending_input.load(Ordering::Relaxed), 0);
        m.presented(frame(Some(20), Some(u64::MAX)), 1_000_000);
        m.parse_tick(64);
        assert_eq!(m.latency().input_to_present().count(), 0);
        assert_eq!(m.latency().byte_to_present().count(), 0);
        assert_eq!(m.parse_bytes_per_tick().count(), 0);
        m.set_histograms(HistogramMode::On);
        m.parse_tick(64);
        assert_eq!(m.parse_bytes_per_tick().count(), 1);
    }

    #[test]
    fn arrivals_keep_the_oldest_until_a_frame_takes_them() {
        let a = Arrivals::default();
        assert_eq!(a.take(), frame(None, None));
        a.note(50);
        a.note(70);
        a.note(60);
        assert_eq!(a.take(), frame(Some(50), Some(70)));
        assert_eq!(a.take(), frame(None, Some(70)));
    }

    #[test]
    fn a_failed_tear_write_is_counted_every_time_and_logged_at_most_once_a_second() {
        let before = TEAR_WRITE_FAILURES.get(TearWrite::Resize);
        let err = tear_types::ControlError::Transport("lost".into());
        tear_write_failed(TearWrite::Resize, tear_types::PaneId(7), 0, &err);
        tear_write_failed(TearWrite::Resize, tear_types::PaneId(7), 0, &err);
        assert!(TEAR_WRITE_FAILURES.get(TearWrite::Resize) >= before + 2);
        let last = AtomicU64::new(0);
        assert!(log_due(&last, 5, 1_000));
        assert!(!log_due(&last, 900, 1_000));
        assert!(log_due(&last, 1_005, 1_000));
        assert!(!log_due(&last, 1_006, 1_000));
    }

    #[test]
    fn each_tear_write_label_has_its_own_log_limiter_and_says_what_its_failure_costs() {
        let slots: TearWriteLogSlots = [const { AtomicU64::new(0) }; TearWrite::COUNT];
        assert!(tear_write_log_due(&slots, TearWrite::Keys, 5));
        assert!(tear_write_log_due(&slots, TearWrite::QueryAnswer, 6));
        assert!(!tear_write_log_due(&slots, TearWrite::Keys, 7));
        assert!(!tear_write_log_due(&slots, TearWrite::QueryAnswer, 8));
        assert!(TearWrite::QueryAnswer.consequence().contains("DSR/DA/OSC"));
        let mut said: Vec<&str> = TearWrite::ALL.iter().map(|w| w.consequence()).collect();
        said.sort_unstable();
        said.dedup();
        assert_eq!(said.len(), TearWrite::COUNT);
    }

    #[test]
    fn frame_perf_names_every_reason_and_every_tear_method() {
        let v = frame_perf();
        for key in [
            "last_frame_us",
            "total_frames",
            "total_frames_skipped",
            "total_late_idle_paints",
            "declined_after_acquire",
            "ui_thread_tear_calls_total",
            "tear_write_failures_total",
        ] {
            assert!(v.get(key).is_some(), "frame_perf lacks {key}");
        }
        for w in TearWrite::ALL {
            assert!(v["tear_write_failures"][w.name()].is_u64());
        }
        assert!(["on", "off"].contains(&v["histograms"].as_str().unwrap()));
        for r in PaintReason::ALL {
            assert!(v["paints"][r.name()].is_u64(), "paints lacks {}", r.name());
        }
        for c in TearCall::ALL {
            assert!(v["ui_thread_tear_calls"][c.name()].is_u64());
        }
        for h in ["input_to_present", "byte_to_present"] {
            assert!(v["latency_us"][h]["count"].is_u64());
        }
        assert!(v["parse"]["ui_bytes_per_tick"]["count"].is_u64());
        assert!(v["parse"]["bytes_total"].is_u64());
        assert!(v["queues"]["subscribe"]["drained_per_wake"]["peak"].is_i64());
    }

    #[cfg(feature = "bench-probes")]
    #[test]
    fn the_wake_off_fault_hands_out_a_bell_that_rings_nothing_and_refuses_only_unknown_names() {
        use super::faults::{Fault, bell_for, from_list};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        assert_eq!(from_list("bogus, wake-off,,"), vec![Fault::WakeOff]);
        let rung = Arc::new(Count(AtomicUsize::new(0)));
        let window = std::task::Waker::from(Arc::clone(&rung));
        bell_for(window.clone(), true).wake();
        assert_eq!(rung.0.load(Ordering::SeqCst), 0, "wake-off rings nothing");
        bell_for(window, false).wake();
        assert_eq!(
            rung.0.load(Ordering::SeqCst),
            1,
            "the clean bell is the window's"
        );
        assert!(
            frame_perf()["bench_faults"].is_array(),
            "a bench build reports what it armed"
        );
    }

    const UI_SOURCES: &[&str] = &[
        "src/gui_tear_attach.rs",
        "src/session_picker.rs",
        "src/auto_attach.rs",
        "src/pane_stream.rs",
        "src/praca_store.rs",
    ];
    const UI_DIRS: &[&str] = &["src/ux", "src/picker"];
    const EVENT_LOOPS: &[&str] = &["src/main.rs", "src/gui_tear_attach.rs"];
    const NOT_A_TEAR_CALL: &[&str] = &["clone", "as_ref"];

    fn is_ident(c: Option<&char>) -> bool {
        c.is_some_and(|c| c.is_alphanumeric() || *c == '_')
    }

    fn code_only(src: &str) -> String {
        let b: Vec<char> = src.chars().collect();
        let mut out = String::with_capacity(src.len());
        let mut i = 0;
        let skip_to = |out: &mut String, from: usize, to: usize| {
            for c in &b[from..to.min(b.len())] {
                if *c == '\n' {
                    out.push('\n');
                }
            }
        };
        while i < b.len() {
            let c = b[i];
            let next = b.get(i + 1).copied();
            if c == '/' && next == Some('/') {
                while i < b.len() && b[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            if c == '/' && next == Some('*') {
                let mut j = i + 2;
                while j < b.len() && !(b[j] == '*' && b.get(j + 1) == Some(&'/')) {
                    j += 1;
                }
                skip_to(&mut out, i, j);
                i = j + 2;
                continue;
            }
            let raw_prefix = c == 'r'
                && (!is_ident(i.checked_sub(1).and_then(|p| b.get(p)))
                    || (b.get(i.wrapping_sub(1)) == Some(&'b')
                        && !is_ident(i.checked_sub(2).and_then(|p| b.get(p)))));
            if raw_prefix && (next == Some('"') || next == Some('#')) {
                let mut j = i + 1;
                let mut hashes = 0;
                while b.get(j) == Some(&'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&'"') {
                    j += 1;
                    while j < b.len()
                        && !(b[j] == '"' && (0..hashes).all(|k| b.get(j + 1 + k) == Some(&'#')))
                    {
                        j += 1;
                    }
                    skip_to(&mut out, i, j);
                    out.push_str("r\"\"");
                    i = j + 1 + hashes;
                    continue;
                }
            }
            if c == '"' {
                let mut j = i + 1;
                while j < b.len() && b[j] != '"' {
                    j += if b[j] == '\\' { 2 } else { 1 };
                }
                skip_to(&mut out, i, j);
                out.push_str("\"\"");
                i = j + 1;
                continue;
            }
            if c == '\'' && next == Some('\\') {
                let mut j = i + 3;
                while j < b.len() && b[j] != '\'' {
                    j += 1;
                }
                out.push_str("' '");
                i = j + 1;
                continue;
            }
            if c == '\'' && b.get(i + 2) == Some(&'\'') {
                out.push_str("' '");
                i += 3;
                continue;
            }
            out.push(c);
            i += 1;
        }
        out
    }

    fn production_part(src: &str) -> String {
        let end = src.find("\n#[cfg(test)]\nmod ").unwrap_or(src.len());
        code_only(&src[..end])
    }

    fn line_of(src: &str, at: usize) -> usize {
        src[..at].matches('\n').count() + 1
    }

    fn raw_backend_calls(src: &str) -> Vec<String> {
        let src = production_part(src);
        let call = regex::Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\.\s*([a-z_][a-z0-9_]*)\s*\(")
            .unwrap();
        call.captures_iter(&src)
            .filter(|c| {
                let receiver = &c[1];
                (receiver.contains("client") || receiver.contains("inproc"))
                    && !NOT_A_TEAR_CALL.contains(&&c[2])
            })
            .map(|c| {
                let at = c.get(0).unwrap().start();
                format!(
                    "line {}: `{}` calls a raw tear backend",
                    line_of(&src, at),
                    &c[0]
                )
            })
            .collect()
    }

    fn unwrapped_event_loops(src: &str) -> (usize, Vec<String>) {
        let src = code_only(src);
        let on_event = regex::Regex::new(r"\.\s*on_event\s*\(\s*").unwrap();
        let mut seen = 0;
        let mut misses = Vec::new();
        for m in on_event.find_iter(&src) {
            seen += 1;
            if !src[m.end()..].starts_with("crate::perf::ui_dispatch(") {
                misses.push(format!(
                    "line {}: an event loop's handler is not wrapped in perf::ui_dispatch",
                    line_of(&src, m.start())
                ));
            }
        }
        (seen, misses)
    }

    fn ui_source_files() -> Vec<std::path::PathBuf> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files: Vec<std::path::PathBuf> = UI_SOURCES.iter().map(|f| root.join(f)).collect();
        for dir in UI_DIRS {
            for entry in std::fs::read_dir(root.join(dir)).unwrap() {
                let path = entry.unwrap().path();
                if path.extension().is_some_and(|e| e == "rs") {
                    files.push(path);
                }
            }
        }
        files
    }

    #[test]
    fn no_ui_module_calls_a_raw_tear_backend() {
        let mut misses = Vec::new();
        for f in ui_source_files() {
            let src = std::fs::read_to_string(&f).unwrap();
            for m in raw_backend_calls(&src) {
                misses.push(format!("{}: {m}", f.display()));
            }
        }
        assert!(
            misses.is_empty(),
            "tear calls that bypass perf::Counted:\n{}",
            misses.join("\n")
        );
    }

    #[test]
    fn every_event_loop_dispatches_through_ui_dispatch() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for f in EVENT_LOOPS {
            let src = std::fs::read_to_string(root.join(f)).unwrap();
            let (seen, misses) = unwrapped_event_loops(&src);
            assert!(seen > 0, "{f} has no event loop to check");
            assert!(misses.is_empty(), "{f}:\n{}", misses.join("\n"));
        }
    }

    #[test]
    fn the_scans_see_what_they_must_and_nothing_else() {
        assert_eq!(raw_backend_calls("fn f() { client.get_pane(p); }").len(), 1);
        assert_eq!(
            raw_backend_calls("fn f() { self.inproc\n    .set_spawn_env(e); }").len(),
            1
        );
        assert_eq!(
            raw_backend_calls("fn f() { reap_client.kill_session(s); }").len(),
            1
        );
        assert!(
            raw_backend_calls("fn f() { tear.get_pane(p); control.send_keys(p, b); }").is_empty()
        );
        assert!(
            raw_backend_calls("fn f() { let c = client.clone(); inproc.as_ref(); }").is_empty()
        );
        assert!(
            raw_backend_calls("fn f() {}\n// client.send_keys(p, b)\n/* inproc.get_pane(p) */")
                .is_empty()
        );
        let quoted = r#"fn f() { log("inproc.pane_snapshot(x)"); let q = ('"', '\''); client.get_pane(p); }"#;
        assert_eq!(
            raw_backend_calls(quoted).len(),
            1,
            "a call named in a string is not a call, the real one after it is"
        );
        assert!(
            raw_backend_calls(
                "fn f() {}\n#[cfg(test)]\nmod tests { fn g() { client.get_pane(p); } }"
            )
            .is_empty()
        );

        let wrapped = "App::builder()\n    .on_event(crate::perf::ui_dispatch(move |e, r| x))";
        assert_eq!(unwrapped_event_loops(wrapped), (1, Vec::new()));
        let bare = "App::builder()\n    .on_event(move |e, r| -> EventResponse { x })";
        let (seen, misses) = unwrapped_event_loops(bare);
        assert_eq!((seen, misses.len()), (1, 1));
        assert_eq!(unwrapped_event_loops("// .on_event(move |e, r| x)").0, 0);
    }
}
