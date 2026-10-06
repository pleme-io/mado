use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use engate_attach::Producer;
use engate_types::AttachError;

pub struct StreamWatch<P> {
    inner: P,
    ended: Arc<AtomicBool>,
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
        let ended = Arc::new(AtomicBool::new(false));
        (
            Self {
                inner,
                ended: Arc::clone(&ended),
            },
            StreamEnded(ended),
        )
    }
}

impl<P: Producer> Producer for StreamWatch<P> {
    type Item = P::Item;
    type Snap = P::Snap;

    fn snapshot(&self) -> Result<Self::Snap, AttachError> {
        self.inner.snapshot()
    }

    fn subscribe(&self) -> Result<mpsc::Receiver<Self::Item>, AttachError> {
        let upstream = self.inner.subscribe()?;
        let (tx, rx) = mpsc::channel();
        let ended = Arc::clone(&self.ended);
        std::thread::Builder::new()
            .name("mado-stream-watch".into())
            .spawn(move || {
                while let Ok(item) = upstream.recv() {
                    if tx.send(item).is_err() {
                        return;
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
        tx: Mutex<Option<mpsc::Sender<u8>>>,
    }

    #[derive(Debug, Clone)]
    struct Snap;

    impl engate_types::Snapshot for Snap {}

    struct Up(Arc<Upstream>);

    impl Producer for Up {
        type Item = u8;
        type Snap = Snap;

        fn snapshot(&self) -> Result<Snap, AttachError> {
            Ok(Snap)
        }

        fn subscribe(&self) -> Result<mpsc::Receiver<u8>, AttachError> {
            let (tx, rx) = mpsc::channel();
            *self.0.tx.lock().unwrap() = Some(tx);
            Ok(rx)
        }
    }

    fn eventually(flag: &StreamEnded, want: bool) -> bool {
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            if flag.is_set() == want {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    #[test]
    fn items_pass_through_and_an_upstream_close_is_reported() {
        let up = Arc::new(Upstream {
            tx: Mutex::new(None),
        });
        let (watch, ended) = StreamWatch::new(Up(Arc::clone(&up)));
        let rx = watch.subscribe().unwrap();
        up.tx.lock().unwrap().as_ref().unwrap().send(7).unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), 7);
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
        up.tx.lock().unwrap().as_ref().unwrap().send(1).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !ended.is_set(),
            "a view switching away must not read as the stream ending"
        );
    }
}
