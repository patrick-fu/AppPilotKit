//! Internal single-result handoff without sending owned resources through a channel.

use crate::adapter::Cancellation;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicU8, Ordering},
};
use std::time::Instant;

const PRODUCING: u8 = 0;
const READY: u8 = 1;
const TAKEN: u8 = 2;
const ABANDONED: u8 = 3;
const REAPING: u8 = 4;
const REAPED: u8 = 5;
const CLEANUP_FAILED: u8 = 6;

type Reaper<T> = Box<dyn FnOnce(T) -> bool + Send>;

struct Shared<T> {
    state: AtomicU8,
    cell: Mutex<Option<T>>,
    reaper: Mutex<Option<Reaper<T>>>,
    cancellation: Cancellation,
    changed: Condvar,
}

/// Producer half of an internal owned result.
pub(crate) struct OwnedOpProducer<T> {
    shared: Arc<Shared<T>>,
}

/// Consumer half of an internal owned result.
pub(crate) struct OwnedOp<T> {
    shared: Arc<Shared<T>>,
}

/// Publication never carries the owned resource in its notification.
pub(crate) enum PublishResult {
    Published,
    Reaped,
    CleanupFailed,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<T> OwnedOp<T> {
    pub(crate) fn new(
        reaper: impl FnOnce(T) -> bool + Send + 'static,
    ) -> (OwnedOpProducer<T>, Self) {
        let shared = Arc::new(Shared {
            state: AtomicU8::new(PRODUCING),
            cell: Mutex::new(None),
            reaper: Mutex::new(Some(Box::new(reaper))),
            cancellation: Cancellation::new(),
            changed: Condvar::new(),
        });
        (
            OwnedOpProducer {
                shared: Arc::clone(&shared),
            },
            Self { shared },
        )
    }

    pub(crate) fn cancellation(&self) -> Cancellation {
        self.shared.cancellation.clone()
    }

    pub(crate) fn take(&self) -> Option<T> {
        if self
            .shared
            .state
            .compare_exchange(READY, TAKEN, Ordering::Acquire, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        self.shared.cell.lock().expect("owned result cell").take()
    }

    pub(crate) fn abandon(&self) -> bool {
        if self
            .shared
            .state
            .compare_exchange(PRODUCING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.shared.cancellation.cancel();
            return true;
        }
        false
    }

    /// Waits for publication without carrying the resource in the notification.
    /// At the deadline this atomically abandons a still-producing operation.
    pub(crate) fn wait_until(&self, deadline: Instant) -> Option<T> {
        let mut cell = self.shared.cell.lock().expect("owned result cell");
        loop {
            if self
                .shared
                .state
                .compare_exchange(READY, TAKEN, Ordering::Acquire, Ordering::Acquire)
                .is_ok()
            {
                return cell.take();
            }
            if Instant::now() >= deadline {
                if self
                    .shared
                    .state
                    .compare_exchange(PRODUCING, ABANDONED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    self.shared.cancellation.cancel();
                    return None;
                }
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (next, _) = self
                .shared
                .changed
                .wait_timeout(cell, remaining)
                .expect("owned result wait");
            cell = next;
        }
    }
}

impl<T> OwnedOpProducer<T> {
    /// Writes the cell before publishing `READY`; a late producer owns reaping.
    pub(crate) fn publish(self, value: T) -> PublishResult {
        {
            let mut cell = self.shared.cell.lock().expect("owned result cell");
            *cell = Some(value);
        }
        match self.shared.state.compare_exchange(
            PRODUCING,
            READY,
            Ordering::Release,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                self.shared.changed.notify_all();
                PublishResult::Published
            }
            Err(ABANDONED) => {
                self.shared.state.store(REAPING, Ordering::Release);
                let value = self
                    .shared
                    .cell
                    .lock()
                    .expect("owned result cell")
                    .take()
                    .expect("late result is present");
                let reaper = self
                    .shared
                    .reaper
                    .lock()
                    .expect("owned result reaper")
                    .take()
                    .expect("late publication has one reaper");
                if reaper(value) {
                    self.shared.state.store(REAPED, Ordering::Release);
                    self.shared.changed.notify_all();
                    PublishResult::Reaped
                } else {
                    self.shared.state.store(CLEANUP_FAILED, Ordering::Release);
                    self.shared.changed.notify_all();
                    PublishResult::CleanupFailed
                }
            }
            Err(_) => unreachable!("owned operation has one producer"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc, Barrier,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
    };

    #[test]
    fn producer_publishes_before_consumer_takes() {
        let (producer, operation) = OwnedOp::new(|_: u8| panic!("ready result is not reaped"));
        assert!(matches!(producer.publish(7_u8), PublishResult::Published));
        assert_eq!(operation.take(), Some(7));
        assert_eq!(operation.take(), None);
    }

    #[test]
    fn abandon_signals_cancellation_and_gives_late_result_to_one_reaper() {
        let reaped = Arc::new(AtomicUsize::new(0));
        let reaped_by_callback = Arc::clone(&reaped);
        let (producer, operation) = OwnedOp::new(move |value| {
            assert_eq!(value, 9);
            reaped_by_callback.fetch_add(1, Ordering::SeqCst);
            true
        });
        let start = Arc::new(Barrier::new(2));
        let late = Arc::new(Barrier::new(2));
        let observed_cancel = Arc::new(AtomicBool::new(false));
        let producer_start = Arc::clone(&start);
        let producer_late = Arc::clone(&late);
        let producer_cancel = operation.cancellation();
        let producer_observed = Arc::clone(&observed_cancel);
        let publisher = thread::spawn(move || {
            producer_start.wait();
            producer_late.wait();
            producer_observed.store(producer_cancel.is_cancelled(), Ordering::Release);
            producer.publish(9_u8)
        });
        start.wait();
        assert!(operation.abandon());
        late.wait();
        assert!(matches!(
            publisher.join().expect("publisher completes"),
            PublishResult::Reaped
        ));
        assert!(observed_cancel.load(Ordering::Acquire));
        assert_eq!(reaped.load(Ordering::SeqCst), 1);
        assert_eq!(operation.take(), None);
    }
}
