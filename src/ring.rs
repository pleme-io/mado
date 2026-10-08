use std::sync::OnceLock;
use std::task::Waker;

pub struct Ring {
    waker: OnceLock<Waker>,
}

impl Ring {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            waker: OnceLock::new(),
        }
    }

    pub fn connect(&self, waker: Waker) -> bool {
        self.waker.set(waker).is_ok()
    }

    pub fn ring(&self) {
        if let Some(waker) = self.waker.get() {
            waker.wake_by_ref();
        }
    }
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

pub static WINDOW: Ring = Ring::new();

pub fn on_change(mut changes: tokio::sync::watch::Receiver<u64>) {
    let _ = std::thread::Builder::new()
        .name("mado-change-bell".to_owned())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread().build() else {
                return;
            };
            runtime.block_on(async move {
                while changes.changed().await.is_ok() {
                    WINDOW.ring();
                }
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    struct Count(AtomicUsize);

    impl Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_ring_before_the_window_exists_is_a_no_op_and_after_it_reaches_the_window() {
        let ring = Ring::new();
        ring.ring();
        let count = Arc::new(Count(AtomicUsize::new(0)));
        assert!(ring.connect(Waker::from(Arc::clone(&count))));
        ring.ring();
        ring.ring();
        assert_eq!(count.0.load(Ordering::SeqCst), 2);
        assert!(
            !ring.connect(Waker::noop().clone()),
            "the first window keeps the ring"
        );
        ring.ring();
        assert_eq!(count.0.load(Ordering::SeqCst), 3);
    }
}
