//! Temporary admission release while a pass retains its session and occupancy.

use super::InFlightPass;
use signalbox_domain::SessionId;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    future::{Future, pending},
};
use tokio::{
    sync::{mpsc, oneshot},
    task::Id,
};

tokio::task_local! {
    static CURRENT: RefCell<Option<mpsc::UnboundedSender<Change>>>;
}

pub(super) enum Change {
    Release(Id),
    Resume(Id, oneshot::Sender<()>),
}

/// Releases the current scheduler admission during I/O, then waits to reacquire
/// it before returning. Outside a scheduler pass, this simply awaits the I/O.
///
/// The pass retains its session exclusion and original occupancy deadline.
/// Cancellation that returns control to the pass must be handled inside `io`;
/// dropping this future is allowed only when terminating the entire pass.
pub async fn with_released_scheduler_admission<F: Future>(io: F) -> F::Output {
    let sender = CURRENT
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten();
    let Some(sender) = sender else {
        return io.await;
    };
    let task = tokio::task::id();
    if sender.send(Change::Release(task)).is_err() {
        return pending().await;
    }
    let output = io.await;
    let (ready, admitted) = oneshot::channel();
    if sender.send(Change::Resume(task, ready)).is_err() || admitted.await.is_err() {
        return pending().await;
    }
    CURRENT.with(|slot| *slot.borrow_mut() = Some(sender));
    output
}

pub(super) async fn scope<F: Future>(
    sender: mpsc::UnboundedSender<Change>,
    future: F,
) -> F::Output {
    CURRENT.scope(RefCell::new(Some(sender)), future).await
}

#[derive(Default)]
pub(super) struct Admission {
    released: HashSet<Id>,
    waiting: HashMap<SessionId, (Id, oneshot::Sender<()>)>,
}

impl Admission {
    pub(super) fn occupied(&self, tasks: &HashMap<Id, InFlightPass>) -> usize {
        tasks.len() - self.released.len()
    }

    pub(super) fn apply(
        &mut self,
        change: Change,
        tasks: &HashMap<Id, InFlightPass>,
        queue: &mut VecDeque<SessionId>,
        hints: &mut HashSet<SessionId>,
    ) {
        match change {
            Change::Release(task) if tasks.contains_key(&task) => {
                self.released.insert(task);
            }
            Change::Resume(task, ready) if self.released.contains(&task) => {
                if let Some(pass) = tasks.get(&task) {
                    self.waiting.insert(pass.session, (task, ready));
                    if hints.insert(pass.session) {
                        queue.push_back(pass.session);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn resume(&mut self, session: SessionId) -> bool {
        if let Some((task, ready)) = self.waiting.remove(&session) {
            self.released.remove(&task);
            let _ = ready.send(());
            true
        } else {
            false
        }
    }

    pub(super) fn retire(
        &mut self,
        task: Id,
        tasks: &HashMap<Id, InFlightPass>,
        hints: &mut HashSet<SessionId>,
        queue: &mut VecDeque<SessionId>,
    ) {
        self.released.remove(&task);
        if let Some(pass) = tasks.get(&task)
            && self.waiting.remove(&pass.session).is_some()
        {
            hints.remove(&pass.session);
            queue.retain(|session| *session != pass.session);
        }
    }
}
