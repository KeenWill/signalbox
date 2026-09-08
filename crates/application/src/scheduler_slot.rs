//! Scheduler admission ownership across store I/O.

use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio::sync::watch;

#[derive(Debug)]
pub(crate) struct SchedulerSlots {
    maximum: usize,
    active: AtomicUsize,
    changed: watch::Sender<()>,
}

impl SchedulerSlots {
    pub(crate) fn new(maximum: usize) -> Arc<Self> {
        let (changed, _) = watch::channel(());
        Arc::new(Self {
            maximum,
            active: AtomicUsize::new(0),
            changed,
        })
    }

    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn changes(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    fn try_acquire(self: &Arc<Self>) -> Option<SchedulerSlot> {
        let mut active = self.active();
        while active < self.maximum {
            match self.active.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(SchedulerSlot(Arc::clone(self))),
                Err(observed) => active = observed,
            }
        }
        None
    }

    pub(crate) fn reserve(self: &Arc<Self>) -> Option<SchedulerPassSlot> {
        self.try_acquire().map(|slot| SchedulerPassSlot {
            held: Arc::new(Mutex::new(Some(slot))),
        })
    }

    async fn acquire(self: &Arc<Self>) -> SchedulerSlot {
        let mut changed = self.changes();
        loop {
            if let Some(slot) = self.try_acquire() {
                return slot;
            }
            let _ = changed.changed().await;
        }
    }

    #[cfg(test)]
    pub(crate) async fn scope<F: Future>(self: Arc<Self>, future: F) -> F::Output {
        let slot = self.acquire().await;
        CURRENT_SLOT
            .scope(Arc::new(Mutex::new(Some(slot))), future)
            .await
    }
}

#[derive(Debug)]
pub(crate) struct SchedulerSlot(Arc<SchedulerSlots>);

impl Drop for SchedulerSlot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
        self.0.changed.send_replace(());
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SchedulerPassSlot {
    held: Arc<Mutex<Option<SchedulerSlot>>>,
}

impl SchedulerPassSlot {
    pub(crate) fn is_occupied(&self) -> bool {
        self.held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
}

pub(crate) async fn scope_reserved<F: Future>(
    reservation: SchedulerPassSlot,
    future: F,
) -> F::Output {
    let result = CURRENT_SLOT
        .scope(Arc::clone(&reservation.held), future)
        .await;
    reservation
        .held
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    result
}

tokio::task_local! {
    static CURRENT_SLOT: Arc<Mutex<Option<SchedulerSlot>>>;
}

/// Releases this eligibility pass's admission slot during store I/O and
/// reacquires capacity before returning its result. Outside a scheduler pass,
/// the operation runs directly. Cancellation drops the I/O without reacquiring.
pub async fn with_scheduler_slot_released<F: Future>(io: F) -> F::Output {
    let released = CURRENT_SLOT
        .try_with(|slot| {
            slot.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        })
        .ok()
        .flatten();
    let Some(released) = released else {
        return io.await;
    };
    let slots = Arc::clone(&released.0);
    drop(released);
    let result = io.await;
    let acquired = slots.acquire().await;
    let _ = CURRENT_SLOT.try_with(|slot| {
        *slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(acquired);
    });
    slots.changed.send_replace(());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn store_io_releases_capacity_and_waits_for_reacquisition() {
        let slots = SchedulerSlots::new(1);
        let (reading, began) = tokio::sync::oneshot::channel();
        let (finish, finished) = tokio::sync::oneshot::channel();
        let task_slots = Arc::clone(&slots);
        let task = tokio::spawn(task_slots.scope(async {
            with_scheduler_slot_released(async {
                reading.send(()).expect("read started");
                finished.await.expect("read finishes");
            })
            .await;
        }));
        began.await.expect("store I/O reached");
        let text_pass = slots
            .try_acquire()
            .expect("text pass uses released capacity");
        finish.send(()).expect("finish store I/O");
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "send cannot proceed while text owns the slot"
        );
        drop(text_pass);
        task.await.expect("preparation reacquires and finishes");
        assert_eq!(slots.active(), 0);
    }

    #[tokio::test]
    async fn cancelled_store_io_does_not_leak_admission() {
        let slots = SchedulerSlots::new(1);
        let (reading, began) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(Arc::clone(&slots).scope(async {
            with_scheduler_slot_released(async {
                reading.send(()).expect("read started");
                std::future::pending::<()>().await;
            })
            .await;
        }));
        began.await.expect("store I/O reached");
        task.abort();
        assert!(task.await.expect_err("cancelled task").is_cancelled());
        assert_eq!(slots.active(), 0);
        assert!(slots.try_acquire().is_some());
    }
}
