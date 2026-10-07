use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use engate_attach::Producer;
use engate_types::AttachError;

use crate::perf::{QueueTicket, StreamQueue, TearCall, TearCalls};

pub struct StreamWatch<P> {
    inner: P,
    ended: Arc<AtomicBool>,
    queue: &'static StreamQueue,
    calls: &'static TearCalls,
}

#[derive(Debug)]
pub struct Stamped<T> {
    item: T,
    received_at: u64,
    ticket: QueueTicket,
}

impl<T> Stamped<T> {
    pub fn into_parts(self) -> (T, u64) {
        let Self {
            item,
            received_at,
            ticket,
        } = self;
        drop(ticket);
        (item, received_at)
    }
}

#[derive(Clone)]
pub struct StreamEnded(Arc<AtomicBool>);

impl StreamEnded {
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl<P> StreamWatch<P> {
    pub fn new(inner: P) -> (Self, StreamEnded) {
        Self::observed(
            inner,
            &crate::perf::STREAM_QUEUE,
            &crate::perf::UI_TEAR_CALLS,
        )
    }

    pub fn observed(
        inner: P,
        queue: &'static StreamQueue,
        calls: &'static TearCalls,
    ) -> (Self, StreamEnded) {
        let ended = Arc::new(AtomicBool::new(false));
        (
            Self {
                inner,
                ended: Arc::clone(&ended),
                queue,
                calls,
            },
            StreamEnded(ended),
        )
    }
}

enum Relay {
    Open,
    UpstreamClosed,
    DownstreamClosed,
}

fn relay<T: AsRef<[u8]>>(
    upstream: &mpsc::Receiver<T>,
    tx: &mpsc::Sender<Stamped<T>>,
    queue: &'static StreamQueue,
) -> Relay {
    let Ok(first) = upstream.recv() else {
        return Relay::UpstreamClosed;
    };
    let mut relayed = 0i64;
    let mut next = Some(first);
    while let Some(item) = next {
        relayed += 1;
        let ticket = QueueTicket::enter(queue, item.as_ref().len());
        let stamped = Stamped {
            item,
            received_at: crate::perf::now_ns(),
            ticket,
        };
        if tx.send(stamped).is_err() {
            return Relay::DownstreamClosed;
        }
        next = upstream.try_recv().ok();
    }
    queue.relayed_per_wake().set(relayed);
    Relay::Open
}

impl<P> Producer for StreamWatch<P>
where
    P: Producer,
    P::Item: AsRef<[u8]>,
{
    type Item = Stamped<P::Item>;
    type Snap = P::Snap;

    fn snapshot(&self) -> Result<Self::Snap, AttachError> {
        crate::perf::count_tear_call(self.calls, TearCall::ProducerSnapshot);
        self.inner.snapshot()
    }

    fn subscribe(&self) -> Result<mpsc::Receiver<Self::Item>, AttachError> {
        crate::perf::count_tear_call(self.calls, TearCall::ProducerSubscribe);
        let upstream = self.inner.subscribe()?;
        let (tx, rx) = mpsc::channel();
        let ended = Arc::clone(&self.ended);
        let queue = self.queue;
        std::thread::Builder::new()
            .name("mado-stream-watch".into())
            .spawn(move || {
                loop {
                    match relay(&upstream, &tx, queue) {
                        Relay::Open => {}
                        Relay::DownstreamClosed => return,
                        Relay::UpstreamClosed => break,
                    }
                }
                ended.store(true, Ordering::Release);
            })
            .map_err(|e| AttachError::SubscribeFailed(e.to_string()))?;
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct Upstream {
        tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    }

    #[derive(Debug, Clone)]
    struct Snap;

    impl engate_types::Snapshot for Snap {}

    struct Up(Arc<Upstream>);

    impl Producer for Up {
        type Item = Vec<u8>;
        type Snap = Snap;

        fn snapshot(&self) -> Result<Snap, AttachError> {
            Ok(Snap)
        }

        fn subscribe(&self) -> Result<mpsc::Receiver<Vec<u8>>, AttachError> {
            let (tx, rx) = mpsc::channel();
            *self.0.tx.lock().unwrap() = Some(tx);
            Ok(rx)
        }
    }

    fn eventually(flag: &StreamEnded, want: bool) -> bool {
        until(|| flag.is_set() == want)
    }

    #[test]
    fn items_pass_through_and_an_upstream_close_is_reported() {
        let up = Arc::new(Upstream {
            tx: Mutex::new(None),
        });
        let (watch, ended) = StreamWatch::new(Up(Arc::clone(&up)));
        let rx = watch.subscribe().unwrap();
        up.tx
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(vec![7])
            .unwrap();
        let (item, _) = rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .into_parts();
        assert_eq!(item, vec![7]);
        assert!(!ended.is_set());
        up.tx.lock().unwrap().take();
        assert!(
            eventually(&ended, true),
            "an upstream close must be observable"
        );
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn dropping_our_own_receiver_is_not_an_upstream_end() {
        let up = Arc::new(Upstream {
            tx: Mutex::new(None),
        });
        let (watch, ended) = StreamWatch::new(Up(Arc::clone(&up)));
        let rx = watch.subscribe().unwrap();
        drop(rx);
        up.tx
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(vec![1])
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !ended.is_set(),
            "a view switching away must not read as the stream ending"
        );
    }

    fn fresh_queue() -> &'static StreamQueue {
        Box::leak(Box::new(StreamQueue::new()))
    }

    fn fresh_calls() -> &'static TearCalls {
        Box::leak(Box::new(kanshou::metrics::Family::new()))
    }

    #[test]
    fn a_wake_relays_every_chunk_waiting_and_gauges_how_many() {
        let queue = fresh_queue();
        let (up_tx, up_rx) = mpsc::channel::<Vec<u8>>();
        let (tx, rx) = mpsc::channel();
        for chunk in [vec![1], vec![2, 3], vec![4, 5, 6]] {
            up_tx.send(chunk).unwrap();
        }
        assert!(matches!(relay(&up_rx, &tx, queue), Relay::Open));
        assert_eq!(
            rx.try_iter().count(),
            3,
            "one wake drains the three waiting"
        );
        assert_eq!(
            (
                queue.relayed_per_wake().get(),
                queue.relayed_per_wake().peak()
            ),
            (3, 3)
        );
        up_tx.send(vec![7]).unwrap();
        assert!(matches!(relay(&up_rx, &tx, queue), Relay::Open));
        assert_eq!(
            (
                queue.relayed_per_wake().get(),
                queue.relayed_per_wake().peak()
            ),
            (1, 3),
            "the gauge reads the last wake and keeps the largest"
        );
        drop(up_tx);
        assert!(matches!(relay(&up_rx, &tx, queue), Relay::UpstreamClosed));
        drop(rx);
        let (up_tx, up_rx) = mpsc::channel::<Vec<u8>>();
        up_tx.send(vec![8]).unwrap();
        assert!(matches!(relay(&up_rx, &tx, queue), Relay::DownstreamClosed));
    }

    #[test]
    fn the_producer_calls_a_ui_thread_makes_are_counted_and_no_others() {
        let calls = fresh_calls();
        let up = Arc::new(Upstream {
            tx: Mutex::new(None),
        });
        let (watch, _ended) = StreamWatch::observed(Up(Arc::clone(&up)), fresh_queue(), calls);
        let watch = Arc::new(watch);
        {
            let watch = Arc::clone(&watch);
            std::thread::spawn(move || {
                let _ = watch.snapshot();
                drop(watch.subscribe());
            })
            .join()
            .unwrap();
        }
        assert_eq!(calls.total(), 0, "the engate pump's thread is not the UI");
        let _ui = crate::perf::UiThread::mark();
        let _ = watch.snapshot();
        drop(watch.subscribe());
        assert_eq!(calls.get(TearCall::ProducerSnapshot), 1);
        assert_eq!(calls.get(TearCall::ProducerSubscribe), 1);
        assert_eq!(calls.total(), 2);
    }

    fn until(what: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if what() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn a_relayed_item_is_stamped_on_receipt_and_queued_until_taken() {
        let queue: &'static StreamQueue = Box::leak(Box::new(StreamQueue::new()));
        let up = Arc::new(Upstream {
            tx: Mutex::new(None),
        });
        let (watch, _ended) = StreamWatch::observed(Up(Arc::clone(&up)), queue, fresh_calls());
        let rx = watch.subscribe().unwrap();
        let before = crate::perf::now_ns();
        for chunk in [vec![1, 2], vec![3, 4, 5], vec![6, 7, 8, 9]] {
            up.tx.lock().unwrap().as_ref().unwrap().send(chunk).unwrap();
        }
        assert!(
            until(|| queue.items().get() == 3),
            "three chunks wait in the queue"
        );
        assert_eq!(queue.bytes().get(), 9);
        let (bytes, received_at) = rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .into_parts();
        assert!(received_at >= before);
        assert_eq!(bytes, vec![1, 2]);
        assert_eq!(queue.items().get(), 2);
        assert_eq!(queue.bytes().get(), 7);
        assert_eq!(queue.items().peak(), 3);
        drop(rx);
        up.tx
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(vec![0])
            .unwrap();
        assert!(
            until(|| queue.items().get() == 0 && queue.bytes().get() == 0),
            "chunks dropped with a torn-down attach leave the queue, they are not stranded in its depth"
        );
    }
}
