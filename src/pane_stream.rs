use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::task::{Wake, Waker};
use std::time::{Duration, Instant};

use engate_attach::{Attach, Polled, Producer};
use engate_types::{AttachError, Live};
use tear_types::engate_wrap::PaneSnapshotWrap;

use crate::config::PaneFate;
use crate::engate_consumer::{ResponseWriter, TerminalSink};
use crate::perf::{Arrivals, TearCall, TearCalls};
use crate::render::SharedTerminal;

pub const REATTACH_BACKOFF: Duration = Duration::from_millis(500);
pub const DRAIN_BUDGET: u32 = 4096;

struct Stamp {
    arrivals: Arrivals,
    bell: Waker,
}

impl Wake for Stamp {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.arrivals.note(crate::perf::now_ns());
        self.bell.wake_by_ref();
    }
}

pub struct CountedProducer<P> {
    inner: P,
    calls: &'static TearCalls,
}

impl<P> CountedProducer<P> {
    pub fn new(inner: P, calls: &'static TearCalls) -> Self {
        Self { inner, calls }
    }
}

impl<P: Producer> Producer for CountedProducer<P> {
    type Item = P::Item;
    type Snap = P::Snap;

    fn snapshot(&self) -> Result<Self::Snap, AttachError> {
        crate::perf::count_tear_call(self.calls, TearCall::ProducerSnapshot);
        self.inner.snapshot()
    }

    fn subscribe(&self) -> Result<mpsc::Receiver<Self::Item>, AttachError> {
        crate::perf::count_tear_call(self.calls, TearCall::ProducerSubscribe);
        self.inner.subscribe()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Drained {
    pub items: u32,
    pub ended: bool,
}

pub struct PaneStream<P: Producer<Item = Vec<u8>, Snap = PaneSnapshotWrap>> {
    live: Attach<Live, CountedProducer<P>, TerminalSink>,
    stamp: Arc<Stamp>,
    terminal: SharedTerminal,
    ended: bool,
}

impl<P> PaneStream<P>
where
    P: Producer<Item = Vec<u8>, Snap = PaneSnapshotWrap>,
{
    pub fn attach(
        producer_for: impl FnOnce(Waker) -> P,
        calls: &'static TearCalls,
        terminal: SharedTerminal,
        writer: ResponseWriter,
        bell: &Waker,
    ) -> Result<Self, AttachError> {
        let stamp = Arc::new(Stamp {
            arrivals: Arrivals::default(),
            bell: bell.clone(),
        });
        let producer = CountedProducer::new(producer_for(Waker::from(Arc::clone(&stamp))), calls);
        let consumer = TerminalSink::live(Arc::clone(&terminal), writer);
        let (subscribed, history) = Attach::builder()
            .producer(producer)
            .consumer(consumer)
            .build()
            .subscribe()?;
        let live = subscribed.replay(history)?.start_live();
        Ok(Self {
            live,
            stamp,
            terminal,
            ended: false,
        })
    }

    pub fn drain(&mut self, budget: u32) -> Drained {
        let mut items = 0;
        loop {
            if items == budget {
                self.stamp.bell.wake_by_ref();
                break;
            }
            match self.live.poll() {
                Polled::Item => items += 1,
                Polled::Empty => break,
                Polled::Closed => {
                    self.ended = true;
                    break;
                }
            }
        }
        let arrivals = self.stamp.arrivals.take();
        if items > 0 {
            let term = self.terminal.read();
            for at in [arrivals.oldest, arrivals.newest].into_iter().flatten() {
                term.note_arrival(at);
            }
            crate::perf::DRAINED_PER_WAKE.set(i64::from(items));
        }
        Drained {
            items,
            ended: self.ended,
        }
    }

    #[must_use]
    pub fn ended(&self) -> bool {
        self.ended
    }
}

pub fn spawn_feeder<P>(
    producer: P,
    calls: &'static TearCalls,
    terminal: SharedTerminal,
    writer: ResponseWriter,
    bell: Waker,
    ended: Arc<AtomicBool>,
    name: String,
) -> anyhow::Result<()>
where
    P: Producer<Item = Vec<u8>, Snap = PaneSnapshotWrap>,
{
    let consumer = TerminalSink::ringing(terminal, writer, bell.clone());
    let (subscribed, history) = Attach::builder()
        .producer(CountedProducer::new(producer, calls))
        .consumer(consumer)
        .build()
        .subscribe()?;
    let live = subscribed.replay(history)?.start_live();
    std::thread::Builder::new().name(name).spawn(move || {
        let _consumer = live.run();
        tracing::info!("engate channel closed — child PTY EOF, signalling window exit");
        ended.store(true, Ordering::Release);
        bell.wake_by_ref();
    })?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct FateWatch {
    policy: PaneFate,
    backstop: Option<Duration>,
    last_read: Instant,
    saw_end: bool,
}

impl FateWatch {
    #[must_use]
    pub fn new(policy: PaneFate, backstop_secs: u64, now: Instant) -> Self {
        Self {
            policy,
            backstop: (backstop_secs > 0).then(|| Duration::from_secs(backstop_secs)),
            last_read: now,
            saw_end: false,
        }
    }

    pub fn due(&mut self, drained: Drained, now: Instant) -> bool {
        let since = now.saturating_duration_since(self.last_read);
        let due = match self.policy {
            PaneFate::Poll => drained.items == 0,
            PaneFate::Edge => {
                (drained.ended && (!self.saw_end || since >= REATTACH_BACKOFF))
                    || self.backstop.is_some_and(|b| since >= b)
            }
        };
        self.saw_end = drained.ended;
        if due {
            self.last_read = now;
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDLE: Drained = Drained {
        items: 0,
        ended: false,
    };
    const BUSY: Drained = Drained {
        items: 3,
        ended: false,
    };
    const ENDED: Drained = Drained {
        items: 0,
        ended: true,
    };

    fn reads(watch: &mut FateWatch, start: Instant, ticks: &[(Duration, Drained)]) -> Vec<u64> {
        ticks
            .iter()
            .filter(|(at, d)| watch.due(*d, start + *at))
            .map(|(at, _)| u64::try_from(at.as_millis()).unwrap())
            .collect()
    }

    fn every_tick(secs: u64, d: Drained) -> Vec<(Duration, Drained)> {
        (0..secs * 60)
            .map(|i| (Duration::from_micros(i * 16_667), d))
            .collect()
    }

    #[test]
    fn edge_reads_nothing_at_idle_until_the_backstop() {
        let start = Instant::now();
        let mut watch = FateWatch::new(PaneFate::Edge, 30, start);
        assert!(reads(&mut watch, start, &every_tick(29, IDLE)).is_empty());
        assert!(reads(&mut watch, start, &every_tick(29, BUSY)).is_empty());
        assert!(watch.due(IDLE, start + Duration::from_secs(30)));
        assert!(!watch.due(IDLE, start + Duration::from_secs(31)));
        assert!(watch.due(IDLE, start + Duration::from_secs(60)));
    }

    #[test]
    fn edge_reads_at_once_when_the_stream_ends_then_once_per_backoff_while_it_stays_ended() {
        let start = Instant::now();
        let mut watch = FateWatch::new(PaneFate::Edge, 30, start);
        assert!(!watch.due(IDLE, start + Duration::from_millis(10)));
        assert!(watch.due(ENDED, start + Duration::from_millis(20)));
        assert!(!watch.due(ENDED, start + Duration::from_millis(30)));
        assert!(!watch.due(ENDED, start + Duration::from_millis(519)));
        assert!(watch.due(ENDED, start + Duration::from_millis(520)));
        assert!(!watch.due(IDLE, start + Duration::from_millis(600)));
        assert!(
            watch.due(ENDED, start + Duration::from_millis(610)),
            "a fresh stream that ends is a fresh edge"
        );
    }

    #[test]
    fn a_zero_backstop_turns_the_backstop_off() {
        let start = Instant::now();
        let mut watch = FateWatch::new(PaneFate::Edge, 0, start);
        assert!(!watch.due(IDLE, start + Duration::from_secs(3_600)));
        assert!(watch.due(ENDED, start + Duration::from_secs(3_601)));
    }

    #[test]
    fn poll_reads_on_every_idle_tick_as_before() {
        let start = Instant::now();
        let mut watch = FateWatch::new(PaneFate::Poll, 30, start);
        assert_eq!(reads(&mut watch, start, &every_tick(1, IDLE)).len(), 60);
        assert!(reads(&mut watch, start, &every_tick(1, BUSY)).is_empty());
        assert!(watch.due(ENDED, start));
    }
}
