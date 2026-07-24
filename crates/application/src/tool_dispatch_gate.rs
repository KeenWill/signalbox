//! Process-local ordering between tool dispatch and immediate turn stops.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Weak},
};

use signalbox_domain::TurnId;
use tokio::sync::{Mutex, OwnedMutexGuard};

/// Cloneable turn-keyed gate shared by tool dispatch and immediate interrupts.
#[derive(Clone, Debug, Default)]
pub struct InProcessToolDispatchGate {
    turns: Arc<Mutex<HashMap<TurnId, Weak<Mutex<()>>>>>,
}

/// Opaque exclusive permit from [`InProcessToolDispatchGate`].
pub struct InProcessToolDispatchPermit {
    _guard: OwnedMutexGuard<()>,
}

impl InProcessToolDispatchGate {
    /// Acquires exclusive dispatch/stop ordering for one logical turn.
    pub fn acquire(
        &self,
        turn: TurnId,
    ) -> impl Future<Output = InProcessToolDispatchPermit> + Send {
        let turns = Arc::clone(&self.turns);
        async move {
            let turn_gate = {
                let mut known = turns.lock().await;
                known.retain(|_, gate| gate.strong_count() > 0);
                known.get(&turn).and_then(Weak::upgrade).unwrap_or_else(|| {
                    let gate = Arc::new(Mutex::new(()));
                    known.insert(turn, Arc::downgrade(&gate));
                    gate
                })
            };
            InProcessToolDispatchPermit {
                _guard: turn_gate.lock_owned().await,
            }
        }
    }
}
