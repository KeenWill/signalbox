//! Terminal application boundary for a pinned lost-runner replacement.

use std::future::Future;

use signalbox_domain::{
    ContextFrontierId, DurableCommandId, PinnedRunnerReplacementResult, ReplaceLostRunner,
    RunnerGeneration, RunnerReplacementTarget, SemanticTranscriptEntryId, SessionId,
};

use crate::InvalidDurableCommandId;

/// Complete admitted request for one pinned lost-runner replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedRunnerReplacementRequest {
    command: DurableCommandId,
    session: SessionId,
    expected_placement_revision: RunnerGeneration,
    replacement: RunnerReplacementTarget,
}

impl PinnedRunnerReplacementRequest {
    /// Rejects reserved durable-command identities before transaction entry.
    pub fn try_new(
        command: DurableCommandId,
        session: SessionId,
        expected_placement_revision: RunnerGeneration,
        replacement: RunnerReplacementTarget,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }
        Ok(Self {
            command,
            session,
            expected_placement_revision,
            replacement,
        })
    }

    /// Returns the user-global durable command identity.
    pub const fn command(&self) -> DurableCommandId {
        self.command
    }

    /// Returns the exact session whose lost placement is targeted.
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the placement revision observed by the caller.
    pub const fn expected_placement_revision(&self) -> RunnerGeneration {
        self.expected_placement_revision
    }

    /// Returns the exact selected successor target.
    pub const fn replacement(&self) -> RunnerReplacementTarget {
        self.replacement
    }
}

/// Fresh identities reserved for the placement transcript boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PinnedRunnerReplacementIdentities {
    semantic_entry: SemanticTranscriptEntryId,
    context_frontier: ContextFrontierId,
}

impl PinnedRunnerReplacementIdentities {
    pub const fn new(
        semantic_entry: SemanticTranscriptEntryId,
        context_frontier: ContextFrontierId,
    ) -> Self {
        Self {
            semantic_entry,
            context_frontier,
        }
    }

    /// Returns the identity of the injected placement entry.
    pub const fn semantic_entry(self) -> SemanticTranscriptEntryId {
        self.semantic_entry
    }

    /// Returns the identity of the prefix-extending placement frontier.
    pub const fn context_frontier(self) -> ContextFrontierId {
        self.context_frontier
    }
}

/// Supplies fresh identities for a placement transcript boundary.
pub trait PinnedRunnerReplacementIdGenerator {
    fn next_identities(&mut self) -> PinnedRunnerReplacementIdentities;
}

/// Production UUIDv7 generator for pinned replacement boundary identities.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7PinnedRunnerReplacementIdGenerator;

impl PinnedRunnerReplacementIdGenerator for UuidV7PinnedRunnerReplacementIdGenerator {
    fn next_identities(&mut self) -> PinnedRunnerReplacementIdentities {
        PinnedRunnerReplacementIdentities::new(
            SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7()),
            ContextFrontierId::from_uuid(uuid::Uuid::now_v7()),
        )
    }
}

/// Atomic durable completion boundary for one pinned replacement.
pub trait PinnedRunnerReplacementTransaction {
    type Error;

    fn complete(
        &mut self,
        command: ReplaceLostRunner,
        identities: PinnedRunnerReplacementIdentities,
    ) -> impl Future<Output = Result<PinnedRunnerReplacementOutcome, Self::Error>> + Send;
}

/// Staged work, a terminal result, another placement kind, or command conflict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PinnedRunnerReplacementOutcome {
    /// The command is durable and awaits its safe observation boundary.
    Staged { command: DurableCommandId },
    /// The exact command owns this terminal durable result.
    Recorded(PinnedRunnerReplacementResult),
    /// The current placement belongs to another replacement transaction.
    NotApplicable,
    /// The user-global command identity names different intent.
    ConflictingReuse { command: DurableCommandId },
}

/// Coordinates one canonical pinned lost-runner replacement command.
#[derive(Debug)]
pub struct PinnedRunnerReplacementService<Transaction, Ids> {
    transaction: Transaction,
    ids: Ids,
}

impl<Transaction, Ids> PinnedRunnerReplacementService<Transaction, Ids> {
    pub const fn new(transaction: Transaction, ids: Ids) -> Self {
        Self { transaction, ids }
    }
}

impl<Transaction, Ids> PinnedRunnerReplacementService<Transaction, Ids>
where
    Transaction: PinnedRunnerReplacementTransaction,
    Ids: PinnedRunnerReplacementIdGenerator,
{
    pub async fn execute(
        &mut self,
        request: PinnedRunnerReplacementRequest,
    ) -> Result<PinnedRunnerReplacementOutcome, Transaction::Error> {
        let identities = self.ids.next_identities();
        self.transaction
            .complete(
                ReplaceLostRunner::new(
                    request.command,
                    request.session,
                    request.expected_placement_revision,
                    request.replacement,
                ),
                identities,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, convert::Infallible, future::ready};

    use signalbox_domain::{
        ContextFrontierId, DurableCommandId, ReplaceLostRunner, RunnerGeneration, RunnerId,
        RunnerReplacementTarget, SemanticTranscriptEntryId, SessionId,
    };
    use uuid::{Uuid, Variant, Version};

    use super::{
        PinnedRunnerReplacementIdGenerator, PinnedRunnerReplacementIdentities,
        PinnedRunnerReplacementOutcome, PinnedRunnerReplacementRequest,
        PinnedRunnerReplacementService, PinnedRunnerReplacementTransaction,
        UuidV7PinnedRunnerReplacementIdGenerator,
    };
    use crate::InvalidDurableCommandId;

    const COMMAND: u128 = 1;
    const SESSION: u128 = 2;
    const RUNNER: u128 = 3;
    const ENTRY: u128 = 4;
    const FRONTIER: u128 = 5;

    #[derive(Debug)]
    struct ScriptedIds {
        values: VecDeque<PinnedRunnerReplacementIdentities>,
    }

    impl PinnedRunnerReplacementIdGenerator for ScriptedIds {
        fn next_identities(&mut self) -> PinnedRunnerReplacementIdentities {
            self.values
                .pop_front()
                .expect("the service requests exactly one scripted identity pair")
        }
    }

    #[derive(Debug)]
    struct RecordingTransaction {
        expected_command: ReplaceLostRunner,
        expected_identities: PinnedRunnerReplacementIdentities,
    }

    impl PinnedRunnerReplacementTransaction for RecordingTransaction {
        type Error = Infallible;

        fn complete(
            &mut self,
            command: ReplaceLostRunner,
            identities: PinnedRunnerReplacementIdentities,
        ) -> impl Future<Output = Result<PinnedRunnerReplacementOutcome, Self::Error>> + Send
        {
            assert_eq!(command, self.expected_command);
            assert_eq!(identities, self.expected_identities);
            ready(Ok(PinnedRunnerReplacementOutcome::Staged {
                command: command.command(),
            }))
        }
    }

    /// INV-001: reserved durable-command identities fail before the pinned
    /// replacement transaction can observe a request.
    #[test]
    fn inv001_pinned_replacement_request_rejects_reserved_command_identifiers() {
        let session = SessionId::from_uuid(Uuid::from_u128(SESSION));
        let revision = RunnerGeneration::one();
        let replacement =
            RunnerReplacementTarget::Runner(RunnerId::from_uuid(Uuid::from_u128(RUNNER)));

        assert_eq!(
            PinnedRunnerReplacementRequest::try_new(
                DurableCommandId::from_uuid(Uuid::nil()),
                session,
                revision,
                replacement,
            ),
            Err(InvalidDurableCommandId::Nil)
        );
        assert_eq!(
            PinnedRunnerReplacementRequest::try_new(
                DurableCommandId::from_uuid(Uuid::max()),
                session,
                revision,
                replacement,
            ),
            Err(InvalidDurableCommandId::Max)
        );
    }

    #[tokio::test]
    async fn service_passes_one_command_and_one_boundary_identity_pair() {
        let command = DurableCommandId::from_uuid(Uuid::from_u128(COMMAND));
        let session = SessionId::from_uuid(Uuid::from_u128(SESSION));
        let revision = RunnerGeneration::one();
        let replacement =
            RunnerReplacementTarget::Runner(RunnerId::from_uuid(Uuid::from_u128(RUNNER)));
        let identities = PinnedRunnerReplacementIdentities::new(
            SemanticTranscriptEntryId::from_uuid(Uuid::from_u128(ENTRY)),
            ContextFrontierId::from_uuid(Uuid::from_u128(FRONTIER)),
        );
        let mut service = PinnedRunnerReplacementService::new(
            RecordingTransaction {
                expected_command: ReplaceLostRunner::new(command, session, revision, replacement),
                expected_identities: identities,
            },
            ScriptedIds {
                values: VecDeque::from([identities]),
            },
        );
        let request =
            PinnedRunnerReplacementRequest::try_new(command, session, revision, replacement)
                .expect("the fixture request has a non-reserved command identity");

        assert_eq!(
            service.execute(request).await,
            Ok(PinnedRunnerReplacementOutcome::Staged { command })
        );
    }

    #[test]
    fn uuid_v7_generator_produces_distinct_rfc4122_identities() {
        let mut generator = UuidV7PinnedRunnerReplacementIdGenerator;

        let identities = generator.next_identities();

        assert_eq!(
            identities.semantic_entry().as_uuid().get_version(),
            Some(Version::SortRand)
        );
        assert_eq!(
            identities.semantic_entry().as_uuid().get_variant(),
            Variant::RFC4122
        );
        assert_eq!(
            identities.context_frontier().as_uuid().get_version(),
            Some(Version::SortRand)
        );
        assert_eq!(
            identities.context_frontier().as_uuid().get_variant(),
            Variant::RFC4122
        );
        assert_ne!(
            identities.semantic_entry().as_uuid(),
            identities.context_frontier().as_uuid()
        );
    }
}
