//! Scheduler admission ownership across store I/O.

use std::{
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Debug)]
pub(crate) struct SchedulerSlots {
    maximum: usize,
    state: Mutex<SlotState>,
    reacquisition_queue: tokio::sync::Mutex<()>,
    changed: watch::Sender<()>,
}

#[derive(Debug, Default)]
struct SlotState {
    active: usize,
    waiting: usize,
}

impl SchedulerSlots {
    pub(crate) fn new(maximum: usize) -> Arc<Self> {
        let (changed, _) = watch::channel(());
        Arc::new(Self {
            maximum,
            state: Mutex::new(SlotState::default()),
            reacquisition_queue: tokio::sync::Mutex::new(()),
            changed,
        })
    }

    pub(crate) fn active(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
    }

    pub(crate) fn can_reserve(&self) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.waiting == 0 && state.active < self.maximum
    }

    pub(crate) fn changes(&self) -> watch::Receiver<()> {
        self.changed.subscribe()
    }

    fn try_acquire(self: &Arc<Self>) -> Option<SchedulerSlot> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.waiting > 0 {
            return None;
        }
        self.acquire_available(&mut state)
    }

    fn acquire_available(self: &Arc<Self>, state: &mut SlotState) -> Option<SchedulerSlot> {
        if state.active == self.maximum {
            return None;
        }
        state.active += 1;
        Some(SchedulerSlot(Arc::clone(self)))
    }

    pub(crate) fn reserve(self: &Arc<Self>) -> Option<SchedulerPassSlot> {
        self.try_acquire().map(|slot| SchedulerPassSlot {
            held: Arc::new(Mutex::new(Some(slot))),
        })
    }

    async fn acquire(self: &Arc<Self>) -> SchedulerSlot {
        let _waiting = ReacquisitionWaiter::new(self);
        let _queue = self.reacquisition_queue.lock().await;
        let mut changed = self.changes();
        loop {
            let acquired = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                self.acquire_available(&mut state)
            };
            if let Some(slot) = acquired {
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

struct ReacquisitionWaiter<'a>(&'a SchedulerSlots);

impl<'a> ReacquisitionWaiter<'a> {
    fn new(slots: &'a SchedulerSlots) -> Self {
        slots
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .waiting += 1;
        Self(slots)
    }
}

impl Drop for ReacquisitionWaiter<'_> {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .waiting -= 1;
        self.0.changed.send_replace(());
    }
}

#[derive(Debug)]
pub(crate) struct SchedulerSlot(Arc<SchedulerSlots>);

impl Drop for SchedulerSlot {
    fn drop(&mut self) {
        self.0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active -= 1;
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

    async fn assert_waiting(mut future: std::pin::Pin<&mut impl Future>) {
        std::future::poll_fn(|context| {
            assert!(future.as_mut().poll(context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn returning_pass_reserves_released_capacity_before_fresh_admission() {
        let slots = SchedulerSlots::new(1);
        let occupied = slots.reserve().expect("one occupied slot");
        let mut returning = Box::pin(slots.acquire());
        assert_waiting(returning.as_mut()).await;

        drop(occupied);
        assert!(
            !slots.can_reserve(),
            "the scheduler must wait for returning passes"
        );
        assert!(
            slots.reserve().is_none(),
            "fresh passes cannot bypass the queued return"
        );
        let reacquired = returning.await;
        assert_eq!(slots.active(), 1);
        drop(reacquired);
        assert!(slots.reserve().is_some());
    }

    #[tokio::test]
    async fn returning_passes_reacquire_in_queue_order() {
        let slots = SchedulerSlots::new(1);
        let occupied = slots.reserve().expect("one occupied slot");
        let mut first = Box::pin(slots.acquire());
        let mut second = Box::pin(slots.acquire());
        assert_waiting(first.as_mut()).await;
        assert_waiting(second.as_mut()).await;

        drop(occupied);
        assert_waiting(second.as_mut()).await;
        let first_slot = first.await;
        assert!(slots.reserve().is_none());
        drop(first_slot);
        assert!(slots.reserve().is_none());
        drop(second.await);
        assert!(slots.reserve().is_some());
    }

    #[tokio::test]
    async fn cancelling_the_front_returner_preserves_the_next_returners_priority() {
        let slots = SchedulerSlots::new(1);
        let occupied = slots.reserve().expect("one occupied slot");
        let mut first = Box::pin(slots.acquire());
        let mut second = Box::pin(slots.acquire());
        assert_waiting(first.as_mut()).await;
        assert_waiting(second.as_mut()).await;

        drop(occupied);
        drop(first);
        assert!(slots.reserve().is_none());
        drop(second.await);
        assert!(slots.reserve().is_some());
    }

    #[tokio::test]
    async fn cancelling_the_last_returner_reopens_fresh_admission() {
        let slots = SchedulerSlots::new(1);
        let occupied = slots.reserve().expect("one occupied slot");
        let mut returning = Box::pin(slots.acquire());
        assert_waiting(returning.as_mut()).await;
        drop(occupied);
        assert!(slots.reserve().is_none());
        let mut changes = slots.changes();
        changes.borrow_and_update();

        drop(returning);
        assert!(changes.has_changed().expect("slot channel remains open"));
        assert!(slots.reserve().is_some());
    }

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
