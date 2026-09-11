use super::{
    composed_identity::ComposedWorkspaceIdentity,
    families::WorkspaceBoundExecutors,
    session_workspace_roots::{
        MAX_RETAINED_SESSION_WORKSPACES, RecordedSessionBinding, SessionWorkspaceRoot,
        SessionWorkspaceRoots, another_session_bound,
    },
};
use signalbox_domain::SessionId;
use signalbox_tools_exec::ProcessRunner;
use signalbox_tools_workspace::WorkspaceMutationFileSystem;
use std::collections::BTreeMap;

/// Whether a retained value is still reachable from a request in flight.
///
/// Releasing a value a request still holds does not stop that request: it lets
/// the next request for the same session compose a second value beside it, with
/// its own serialization domain. Two mutations of one tree would then run
/// concurrently under two different locks, which is exactly what per-session
/// serialization exists to prevent.
pub(super) trait RetainedInFlight {
    /// Whether any handle outside the retained set still holds this value.
    fn is_in_flight(&self) -> bool;
}

impl<FileSystem: WorkspaceMutationFileSystem, ExecRunner: ProcessRunner> RetainedInFlight
    for WorkspaceBoundExecutors<FileSystem, ExecRunner>
{
    fn is_in_flight(&self) -> bool {
        // Only the two serializing families carry an identity a second
        // composition could duplicate. The read and execution families hold no
        // lock: a read observes a pinned descriptor, and every execution
        // revalidates the root's identity around its own launch.
        !self.workspace_mutation.is_sole_handle()
            || self
                .local_git
                .as_ref()
                .is_some_and(|git| !git.is_sole_handle())
    }
}

/// One retained per-session value and the counter that orders eviction.
pub(super) struct RetainedSessionWorkspace<Executors> {
    pub(super) executors: Executors,
    last_used: u64,
}

/// Bounded set of derived per-session executor sets, keyed by session.
///
/// Generic in what it retains so the bound and the eviction order can be
/// exercised without composing real descriptor-holding executors.
pub(super) struct RetainedSessionWorkspaces<Executors> {
    pub(super) retained: BTreeMap<SessionId, RetainedSessionWorkspace<Executors>>,
    next_use: u64,
}

/// Every session's recorded binding beside the bounded set of composed
/// executors.
///
/// One lock covers both, because the binding a session is recorded with and the
/// executors retained for it are one fact: recording a derived binding while
/// another caller retained the configured composition would leave a session
/// holding two answers at once.
pub(super) struct SessionWorkspaceState<Executors> {
    pub(super) bindings: BTreeMap<SessionId, RecordedSessionBinding>,
    pub(super) retained: RetainedSessionWorkspaces<Executors>,
}

impl<Executors: Clone + RetainedInFlight> SessionWorkspaceState<Executors> {
    pub(super) fn refuses_shared_workspace(
        &self,
        roots: &SessionWorkspaceRoots,
        session: SessionId,
        composed: ComposedWorkspaceIdentity,
    ) -> bool {
        another_session_bound(&self.bindings, session, composed, |bound| {
            self.retained.retained.contains_key(&bound)
                || !matches!(roots.resolve(bound), SessionWorkspaceRoot::ConfiguredRoot)
        })
    }

    pub(super) const fn new() -> Self {
        Self {
            bindings: BTreeMap::new(),
            retained: RetainedSessionWorkspaces::new(),
        }
    }
}

impl<Executors: Clone + RetainedInFlight> RetainedSessionWorkspaces<Executors> {
    pub(super) const fn new() -> Self {
        Self {
            retained: BTreeMap::new(),
            next_use: 0,
        }
    }

    /// Releases idle entries until the set is back under the bound, or until
    /// none is releasable.
    ///
    /// Releasing one entry per retention would leave the set permanently above
    /// the bound after a burst of concurrent sessions, since each later
    /// retention released one and inserted one. The excess an in-flight request
    /// forces is temporary only if it drains once those requests return.
    fn release_idle_overflow(&mut self) {
        while self.retained.len() >= MAX_RETAINED_SESSION_WORKSPACES {
            let releasable = self
                .retained
                .iter()
                .filter(|(_, retained)| !retained.executors.is_in_flight())
                .min_by_key(|(_, retained)| retained.last_used)
                .map(|(session, _)| *session);
            let Some(releasable) = releasable else {
                return;
            };
            self.retained.remove(&releasable);
        }
    }

    fn take_use(&mut self) -> u64 {
        let use_order = self.next_use;
        self.next_use = self.next_use.saturating_add(1);
        use_order
    }

    pub(super) fn get(&mut self, session: SessionId) -> Option<Executors> {
        let use_order = self.take_use();
        let retained = self.retained.get_mut(&session)?;
        retained.last_used = use_order;
        Some(retained.executors.clone())
    }

    /// Retains one composed set, dropping the least recently used idle entry
    /// when the bound is already reached, and returns the set now retained.
    ///
    /// A concurrent resolution for the same session may have retained its own
    /// set first; that one wins, so every caller converges on one pinned
    /// instance and the loser's descriptors are released immediately.
    ///
    /// An entry a request still holds is not an eviction candidate, so the
    /// retained set may exceed the bound by the number of sessions executing a
    /// workspace-bound tool at that moment. That excess is what keeps one
    /// session's serialization domain single; it is released as soon as those
    /// requests return, at the next retention.
    pub(super) fn retain(&mut self, session: SessionId, executors: Executors) -> Executors {
        if let Some(already_retained) = self.get(session) {
            return already_retained;
        }
        self.release_idle_overflow();
        let last_used = self.take_use();
        self.retained.insert(
            session,
            RetainedSessionWorkspace {
                executors: executors.clone(),
                last_used,
            },
        );
        executors
    }
}
