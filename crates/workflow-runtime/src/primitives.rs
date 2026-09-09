//! Live primitives whose requests and answers pass through the shared host journal.

use crate::{LiveDeliveryFailure, LiveDeliverySource};
use signalbox_domain::program_primitives::{
    AwaitProgramEvent, ProgramEvent, RandomValue, SleepUntil, UnixMillis,
};
use signalbox_domain::{DeliveryKind, RejectReason, RequestFrame, RequestKind};
use signalbox_persistence::program_journal::{ProgramJournalRepository, ProgramJournalWake};
use std::{
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Host-side nondeterminism, consulted only for unanswered primitive requests.
pub trait PrimitiveClock {
    fn now(&mut self) -> Result<UnixMillis, LiveDeliveryFailure>;
    fn random(&mut self) -> Result<RandomValue, LiveDeliveryFailure>;
}

/// Wall time and operating-system randomness for daemon execution.
pub struct SystemPrimitiveClock;

impl PrimitiveClock for SystemPrimitiveClock {
    fn now(&mut self) -> Result<UnixMillis, LiveDeliveryFailure> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(failure)?;
        Ok(UnixMillis(
            u64::try_from(elapsed.as_millis()).map_err(failure)?,
        ))
    }

    fn random(&mut self) -> Result<RandomValue, LiveDeliveryFailure> {
        let mut bytes = [0; size_of::<u64>()];
        getrandom::fill(&mut bytes).map_err(failure)?;
        Ok(RandomValue(u64::from_le_bytes(bytes)))
    }
}

/// Retained source reads and disposable wake hints supplied by the host.
pub trait PrimitiveEvents {
    type Wake: PrimitiveWake;
    fn next_event(
        &mut self,
        wait: AwaitProgramEvent,
    ) -> impl Future<Output = Result<Option<ProgramEvent>, LiveDeliveryFailure>>;
    fn listen(&mut self) -> impl Future<Output = Result<Self::Wake, LiveDeliveryFailure>>;
}

/// A notification never substitutes for reading retained source state.
pub trait PrimitiveWake {
    fn changed(&mut self) -> impl Future<Output = Result<(), LiveDeliveryFailure>>;
}

impl PrimitiveEvents for ProgramJournalRepository {
    type Wake = ProgramJournalWake;
    async fn next_event(
        &mut self,
        wait: AwaitProgramEvent,
    ) -> Result<Option<ProgramEvent>, LiveDeliveryFailure> {
        ProgramJournalRepository::next_event(self, wait)
            .await
            .map_err(failure)
    }
    async fn listen(&mut self) -> Result<Self::Wake, LiveDeliveryFailure> {
        ProgramJournalRepository::listen(self)
            .await
            .map_err(failure)
    }
}

impl PrimitiveWake for ProgramJournalWake {
    async fn changed(&mut self) -> Result<(), LiveDeliveryFailure> {
        ProgramJournalWake::changed(self).await.map_err(failure)
    }
}

/// Absolute deadlines and retained journal answer events, with notifications used as hints.
pub struct DurablePrimitives<C = SystemPrimitiveClock, E = ProgramJournalRepository> {
    journal: E,
    clock: C,
}

impl<C: PrimitiveClock, E: PrimitiveEvents> DurablePrimitives<C, E> {
    pub const fn new(journal: E, clock: C) -> Self {
        Self { journal, clock }
    }

    async fn ready(
        &mut self,
        outstanding: &[RequestFrame],
    ) -> Result<Option<DeliveryKind>, LiveDeliveryFailure> {
        for frame in outstanding {
            let resolves = frame.ordinal();
            let payload = match frame.kind() {
                RequestKind::Now(payload) if payload.as_bytes().is_empty() => {
                    Some(self.clock.now()?.encode())
                }
                RequestKind::Random(payload) if payload.as_bytes().is_empty() => {
                    Some(self.clock.random()?.encode())
                }
                RequestKind::Sleep(payload) => {
                    let Some(deadline) = SleepUntil::decode(payload) else {
                        return Ok(Some(refuse(frame)));
                    };
                    if self.clock.now()? >= deadline.0 {
                        return Ok(Some(DeliveryKind::Wake {
                            resolves,
                            payload: deadline.0.encode(),
                        }));
                    }
                    continue;
                }
                RequestKind::AwaitEvent(payload) => {
                    let Some(wait) = AwaitProgramEvent::decode(payload) else {
                        return Ok(Some(refuse(frame)));
                    };
                    self.journal
                        .next_event(wait)
                        .await
                        .map_err(failure)?
                        .map(|event| event.encode())
                }
                _ => return Ok(Some(refuse(frame))),
            };
            if let Some(payload) = payload {
                return Ok(Some(DeliveryKind::Answer { resolves, payload }));
            }
        }
        Ok(None)
    }

    fn delay(
        &mut self,
        outstanding: &[RequestFrame],
    ) -> Result<Option<Duration>, LiveDeliveryFailure> {
        let deadline = outstanding
            .iter()
            .filter_map(|frame| match frame.kind() {
                RequestKind::Sleep(payload) => {
                    SleepUntil::decode(payload).map(|deadline| deadline.0)
                }
                _ => None,
            })
            .min();
        deadline
            .map(|deadline| {
                self.clock
                    .now()
                    .map(|now| Duration::from_millis(deadline.0.saturating_sub(now.0)))
            })
            .transpose()
    }
}

impl<C: PrimitiveClock, E: PrimitiveEvents> LiveDeliverySource for DurablePrimitives<C, E> {
    fn next_delivery<'a>(
        &'a mut self,
        outstanding: &'a [RequestFrame],
    ) -> Pin<Box<dyn Future<Output = Result<DeliveryKind, LiveDeliveryFailure>> + 'a>> {
        Box::pin(async move {
            if let Some(delivery) = self.ready(outstanding).await? {
                return Ok(delivery);
            }
            let mut wake = if outstanding
                .iter()
                .any(|frame| matches!(frame.kind(), RequestKind::AwaitEvent(_)))
            {
                Some(self.journal.listen().await.map_err(failure)?)
            } else {
                None
            };
            loop {
                // Event LISTEN precedes catch-up, covering commits during subscription.
                if let Some(delivery) = self.ready(outstanding).await? {
                    return Ok(delivery);
                }
                match (self.delay(outstanding)?, wake.as_mut()) {
                    (Some(delay), Some(wake)) => tokio::select! {
                        _ = tokio::time::sleep(delay) => {},
                        result = wake.changed() => { result.map_err(failure)?; },
                    },
                    (Some(delay), None) => tokio::time::sleep(delay).await,
                    (None, Some(wake)) => wake.changed().await.map_err(failure)?,
                    (None, None) => std::future::pending().await,
                }
            }
        })
    }
}

fn refuse(frame: &RequestFrame) -> DeliveryKind {
    DeliveryKind::Reject {
        resolves: frame.ordinal(),
        reason: RejectReason::UnsupportedOperation,
    }
}

fn failure(error: impl std::fmt::Display) -> LiveDeliveryFailure {
    LiveDeliveryFailure::new(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_domain::RequestOrdinal;
    use std::{
        cell::Cell,
        rc::Rc,
        task::{Context, Waker},
    };

    struct ControlledClock(Rc<Cell<UnixMillis>>);

    impl PrimitiveClock for ControlledClock {
        fn now(&mut self) -> Result<UnixMillis, LiveDeliveryFailure> {
            Ok(self.0.get())
        }

        fn random(&mut self) -> Result<RandomValue, LiveDeliveryFailure> {
            panic!("sleep must not draw randomness")
        }
    }

    struct NoEventSource;

    impl PrimitiveEvents for NoEventSource {
        type Wake = Self;

        async fn next_event(
            &mut self,
            _: AwaitProgramEvent,
        ) -> Result<Option<ProgramEvent>, LiveDeliveryFailure> {
            panic!("sleep must not query events")
        }

        async fn listen(&mut self) -> Result<Self::Wake, LiveDeliveryFailure> {
            panic!("sleep must not open a listener")
        }
    }

    impl PrimitiveWake for NoEventSource {
        async fn changed(&mut self) -> Result<(), LiveDeliveryFailure> {
            panic!("sleep must not wait for notifications")
        }
    }

    #[tokio::test]
    async fn sleep_wait_fires_without_accessing_an_event_source() {
        let now = Rc::new(Cell::new(UnixMillis(0)));
        let deadline = SleepUntil(UnixMillis(10));
        let frame = RequestFrame::new(
            RequestOrdinal::try_from_u64(1).unwrap(),
            None,
            RequestKind::Sleep(deadline.encode()),
        );
        let mut primitives = DurablePrimitives::new(NoEventSource, ControlledClock(now.clone()));
        let mut delivery = primitives.next_delivery(std::slice::from_ref(&frame));
        assert!(
            delivery
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        now.set(deadline.0);
        assert_eq!(
            delivery.await.unwrap(),
            DeliveryKind::Wake {
                resolves: frame.ordinal(),
                payload: deadline.0.encode(),
            }
        );
    }
}
