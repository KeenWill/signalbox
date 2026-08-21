//! Operator-commissioned dispatch of one session under an immutable authority fence.
//!
//! Repository watch is not the only source of dispatched work: an operator may
//! commission a session directly, stating up front the repository authority the
//! session acts under. Without a recorded fence such a session reaches the
//! approval judge as an ordinary anonymous one, and the judge — correctly —
//! escalates its first mutable Git operation to a human who is not there. The
//! commissioned dispatch records the same repository/head/base fence a
//! repository-watch dispatch records, in the same transaction that creates the
//! session, submits its first input, and commissions its goal, so the judge
//! consumes it identically and the unattended-escalation closeout covers it.

use std::{error::Error, fmt};

use signalbox_domain::{
    AcceptedInputId, BranchName, CommissionedDispatchId, CommitSha, ContextFrontierId,
    CreateSession, DeliveryRequest, DurableCommandId, GoalStatement, GoalUserAction,
    GoalUserCommand, ModelSelectionOverride, PerInputConfigurationChoices, PreparedCreateSession,
    PullRequestNumber, RepositorySlug, SemanticTranscriptEntryId, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, SessionTemplateName, SessionTemplateProvenance, SubmitInput, TranscriptAncestry,
    TurnId, UserContent, UserContentPart,
};

use crate::create_session::InvalidDurableCommandId;

/// Immutable repository authority an operator asserts for one commissioned session.
///
/// The shapes mirror the repository-watch dispatch fence exactly, because the
/// approval judge consumes both through one authority rendering: a pull-request
/// fence names the pull request, its exact head commit, the repository and
/// branch holding that head, and the base branch; a branch fence names the
/// repository and branch alone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommissionedDispatchFence {
    /// Exact pull-request authority the session is commissioned with.
    PullRequest {
        /// Repository whose pull request the session is commissioned against.
        repository: RepositorySlug,
        /// Pull-request number within the repository.
        pull_request: PullRequestNumber,
        /// Exact head commit authorized at commissioning time.
        head_sha: CommitSha,
        /// Repository containing the authorized head branch.
        head_repository: RepositorySlug,
        /// Authorized head branch.
        head_branch: BranchName,
        /// Authorized base branch.
        base_branch: BranchName,
    },
    /// Exact branch authority the session is commissioned with.
    Branch {
        /// Repository whose branch the session is commissioned against.
        repository: RepositorySlug,
        /// Authorized branch.
        branch: BranchName,
    },
}

/// Candidate identity supply for one commissioned dispatch.
pub trait CommissionedDispatchIdGenerator {
    fn next_dispatch_id(&mut self) -> CommissionedDispatchId;
    fn next_command_id(&mut self) -> DurableCommandId;
    fn next_session_id(&mut self) -> SessionId;
    fn next_accepted_input_id(&mut self) -> AcceptedInputId;
    fn next_turn_id(&mut self) -> TurnId;
    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId;
    fn next_context_frontier_id(&mut self) -> ContextFrontierId;
}

/// Production UUIDv7 identity source for commissioned dispatch.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7CommissionedDispatchIdGenerator;

impl CommissionedDispatchIdGenerator for UuidV7CommissionedDispatchIdGenerator {
    fn next_dispatch_id(&mut self) -> CommissionedDispatchId {
        CommissionedDispatchId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_command_id(&mut self) -> DurableCommandId {
        DurableCommandId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_session_id(&mut self) -> SessionId {
        SessionId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_accepted_input_id(&mut self) -> AcceptedInputId {
        AcceptedInputId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_turn_id(&mut self) -> TurnId {
        TurnId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_semantic_entry_id(&mut self) -> SemanticTranscriptEntryId {
        SemanticTranscriptEntryId::from_uuid(uuid::Uuid::now_v7())
    }

    fn next_context_frontier_id(&mut self) -> ContextFrontierId {
        ContextFrontierId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// One operator request to commission a session under a recorded fence.
///
/// The command identity is the caller's idempotency key for the whole
/// composite: it becomes the created session's durable create command, so a
/// retried request replays against the committed commission instead of
/// creating a second session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommissionDispatchRequest {
    command_id: DurableCommandId,
    template: SessionTemplateName,
    fence: CommissionedDispatchFence,
    statement: GoalStatement,
    context: UserContent,
}

impl CommissionDispatchRequest {
    /// Validates the caller-supplied idempotency identity.
    pub fn try_new(
        command_id: DurableCommandId,
        template: SessionTemplateName,
        fence: CommissionedDispatchFence,
        statement: GoalStatement,
        context: UserContent,
    ) -> Result<Self, InvalidDurableCommandId> {
        if command_id.as_uuid().is_nil() {
            return Err(InvalidDurableCommandId::Nil);
        }
        if command_id.as_uuid().is_max() {
            return Err(InvalidDurableCommandId::Max);
        }
        Ok(Self {
            command_id,
            template,
            fence,
            statement,
            context,
        })
    }

    /// Returns the caller-supplied idempotency identity.
    #[must_use]
    pub const fn command_id(&self) -> DurableCommandId {
        self.command_id
    }

    /// Borrows the requested session-template name.
    #[must_use]
    pub const fn template(&self) -> &SessionTemplateName {
        &self.template
    }

    /// Borrows the asserted authority fence.
    #[must_use]
    pub const fn fence(&self) -> &CommissionedDispatchFence {
        &self.fence
    }

    /// Borrows the commissioned goal statement.
    #[must_use]
    pub const fn statement(&self) -> &GoalStatement {
        &self.statement
    }

    /// Returns the digest binding this request's initial content to replay.
    #[must_use]
    pub fn initial_content_digest(&self) -> [u8; 32] {
        initial_content_digest(&self.context)
    }

    /// Prepares the complete composite the durable transaction commits.
    ///
    /// The composition is the repository-watch dispatch action's, minus the
    /// rule machinery: one created session from the resolved template, one
    /// initial input through the start-when-idle path carrying the operator's
    /// context, and one goal commissioned from the operator's statement that
    /// adopts the reserved turn as its own first turn. The fence rides beside
    /// them into the same transaction, so no commissioned session is durably
    /// visible without the authority it was commissioned under.
    pub fn prepare(
        self,
        ids: &mut impl CommissionedDispatchIdGenerator,
        template_provenance: SessionTemplateProvenance,
        resolved_defaults: SessionConfigurationDefaults,
    ) -> Result<PreparedCommissionedDispatch, CommissionDispatchPreparationError> {
        if template_provenance.name() != &self.template {
            return Err(CommissionDispatchPreparationError::TemplateMismatch);
        }
        let command = CreateSession::new_from_template(
            self.command_id,
            SessionCreationProvenance::new(
                SessionCreationCause::UserInitiated,
                TranscriptAncestry::None,
            ),
            template_provenance,
            resolved_defaults,
        );
        let prepared_session = command
            .prepare(ids.next_session_id())
            .map_err(|_| CommissionDispatchPreparationError::SessionPreparation)?;
        let session = prepared_session.applied_result().session();
        let initial_input = SubmitInput::new(
            ids.next_command_id(),
            session,
            self.context,
            DeliveryRequest::StartWhenNoActiveTurn {
                configuration: PerInputConfigurationChoices::new(
                    SessionConfigurationDefaultsVersion::first(),
                    ModelSelectionOverride::UseSessionDefault,
                ),
            },
        );
        let goal = GoalUserCommand::new(
            ids.next_command_id(),
            session,
            GoalUserAction::Attach(self.statement),
        );
        Ok(PreparedCommissionedDispatch {
            dispatch_id: ids.next_dispatch_id(),
            fence: self.fence,
            prepared_session,
            initial_input,
            accepted_input: ids.next_accepted_input_id(),
            turn: ids.next_turn_id(),
            cancellation_entry: ids.next_semantic_entry_id(),
            cancellation_frontier: ids.next_context_frontier_id(),
            goal,
        })
    }
}

/// Digests one exact initial content for commission replay equality.
///
/// The digest is domain-separated SHA-256 over the complete ordered content
/// structure, so a retried request whose content differs from the committed
/// commission is distinguishable without persisting the content twice.
fn initial_content_digest(content: &UserContent) -> [u8; 32] {
    use sha2::Digest as _;

    let mut digest = sha2::Sha256::new();
    digest.update(b"signalbox/commissioned-dispatch/initial-content/v2");
    digest.update((content.parts().len() as u64).to_be_bytes());
    for part in content.parts() {
        match part {
            UserContentPart::Text { value } => {
                digest.update([0]);
                update_digest_bytes(&mut digest, value.as_str().as_bytes());
            }
            UserContentPart::Attachment {
                digest: blob_digest,
                kind,
                media_type,
                display_filename,
            } => {
                digest.update([1]);
                digest.update(blob_digest.as_bytes());
                digest.update([*kind as u8]);
                update_digest_bytes(&mut digest, media_type.as_str().as_bytes());
                match display_filename {
                    Some(filename) => {
                        digest.update([1]);
                        update_digest_bytes(&mut digest, filename.as_str().as_bytes());
                    }
                    None => digest.update([0]),
                }
            }
        }
    }
    digest.finalize().into()
}

fn update_digest_bytes(digest: &mut sha2::Sha256, bytes: &[u8]) {
    use sha2::Digest as _;
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

/// One commissioned dispatch whose session creation has been domain-prepared.
///
/// The turn reserved here is the only one the commissioned session receives.
/// It carries the operator's context, and the goal commissioned in the same
/// transaction adopts it as that generation's own first turn, exactly as a
/// repository-watch dispatch action does.
#[derive(Debug)]
pub struct PreparedCommissionedDispatch {
    dispatch_id: CommissionedDispatchId,
    fence: CommissionedDispatchFence,
    prepared_session: PreparedCreateSession,
    initial_input: SubmitInput,
    accepted_input: AcceptedInputId,
    turn: TurnId,
    cancellation_entry: SemanticTranscriptEntryId,
    cancellation_frontier: ContextFrontierId,
    goal: GoalUserCommand,
}

impl PreparedCommissionedDispatch {
    /// Returns the minted append-only dispatch identity.
    #[must_use]
    pub const fn dispatch_id(&self) -> CommissionedDispatchId {
        self.dispatch_id
    }

    /// Borrows the asserted authority fence.
    #[must_use]
    pub const fn fence(&self) -> &CommissionedDispatchFence {
        &self.fence
    }

    /// Borrows the prepared session creation.
    #[must_use]
    pub const fn prepared_session(&self) -> &PreparedCreateSession {
        &self.prepared_session
    }

    /// Borrows the commission this dispatch composed for the created session.
    #[must_use]
    pub const fn goal(&self) -> &GoalUserCommand {
        &self.goal
    }

    /// Returns the digest binding the composed initial content to replay.
    #[must_use]
    pub fn initial_content_digest(&self) -> [u8; 32] {
        initial_content_digest(self.initial_input.content())
    }

    /// Returns the created session's identity.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.prepared_session.applied_result().session()
    }

    /// Decomposes into exactly the parts the durable transaction commits.
    #[allow(
        clippy::type_complexity,
        reason = "one-shot decomposition mirrors the dispatch action"
    )]
    pub fn into_parts(
        self,
    ) -> (
        CommissionedDispatchId,
        CommissionedDispatchFence,
        PreparedCreateSession,
        SubmitInput,
        AcceptedInputId,
        TurnId,
        SemanticTranscriptEntryId,
        ContextFrontierId,
        GoalUserCommand,
    ) {
        (
            self.dispatch_id,
            self.fence,
            self.prepared_session,
            self.initial_input,
            self.accepted_input,
            self.turn,
            self.cancellation_entry,
            self.cancellation_frontier,
            self.goal,
        )
    }
}

/// Why a commissioned dispatch could not be prepared for its atomic port.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommissionDispatchPreparationError {
    /// The resolved template's name is not the requested template.
    TemplateMismatch,
    /// Session preparation refused the composed creation command.
    SessionPreparation,
}

impl fmt::Display for CommissionDispatchPreparationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TemplateMismatch => {
                "commissioned dispatch resolved a template other than the requested one"
            }
            Self::SessionPreparation => "commissioned dispatch session preparation failed",
        })
    }
}

impl Error for CommissionDispatchPreparationError {}

#[cfg(test)]
mod tests {
    use signalbox_domain::{
        DirectModelSelection, ModelSelectionRequest, SessionTemplateContentDigest,
    };

    use super::*;

    fn fence() -> CommissionedDispatchFence {
        CommissionedDispatchFence::PullRequest {
            repository: RepositorySlug::try_new(String::from("sample-user/sample-repository"))
                .expect("the fixture repository is admitted"),
            pull_request: PullRequestNumber::new(
                std::num::NonZeroU64::new(12).expect("the fixture number is positive"),
            ),
            head_sha: CommitSha::try_new(String::from("1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d"))
                .expect("the fixture head is admitted"),
            head_repository: RepositorySlug::try_new(String::from("sample-user/sample-repository"))
                .expect("the fixture head repository is admitted"),
            head_branch: BranchName::try_new(String::from("agent/sample-feature"))
                .expect("the fixture head branch is admitted"),
            base_branch: BranchName::try_new(String::from("main"))
                .expect("the fixture base branch is admitted"),
        }
    }

    fn template_name(name: &str) -> SessionTemplateName {
        SessionTemplateName::try_new(String::from(name))
            .expect("the fixture template name is admitted")
    }

    fn template_provenance(name: &str) -> SessionTemplateProvenance {
        SessionTemplateProvenance::new(
            template_name(name),
            SessionTemplateContentDigest::from_bytes([7; 32]),
        )
    }

    fn defaults() -> SessionConfigurationDefaults {
        SessionConfigurationDefaults::new(ModelSelectionRequest::Direct(
            DirectModelSelection::from_uuid(uuid::Uuid::from_u128(11)),
        ))
    }

    fn request(command: u128) -> CommissionDispatchRequest {
        CommissionDispatchRequest::try_new(
            DurableCommandId::from_uuid(uuid::Uuid::from_u128(command)),
            template_name("review-response"),
            fence(),
            GoalStatement::try_new(String::from("Address the findings on pull request 12."))
                .expect("the fixture statement is admitted"),
            UserContent::try_text(String::from("Respond to the review threads."))
                .expect("the fixture context is admitted"),
        )
        .expect("the fixture command identity is admitted")
    }

    #[track_caller]
    fn refused_sentinel(sentinel: uuid::Uuid) -> InvalidDurableCommandId {
        CommissionDispatchRequest::try_new(
            DurableCommandId::from_uuid(sentinel),
            template_name("review-response"),
            fence(),
            GoalStatement::try_new(String::from("Address the findings."))
                .expect("the fixture statement is admitted"),
            UserContent::try_text(String::from("Respond."))
                .expect("the fixture context is admitted"),
        )
        .expect_err("a sentinel command identity is refused")
    }

    #[test]
    fn sentinel_command_identities_are_rejected() {
        assert_eq!(
            refused_sentinel(uuid::Uuid::nil()),
            InvalidDurableCommandId::Nil
        );
        assert_eq!(
            refused_sentinel(uuid::Uuid::max()),
            InvalidDurableCommandId::Max
        );
    }

    #[test]
    fn preparation_binds_the_command_and_adopts_one_reserved_turn() {
        let mut ids = UuidV7CommissionedDispatchIdGenerator;

        let request = request(7);
        let commanded = request.command_id();
        let prepared = request
            .prepare(&mut ids, template_provenance("review-response"), defaults())
            .expect("the fixture request prepares");

        let session = prepared.session();
        assert_eq!(
            prepared.prepared_session().command().command_id(),
            commanded
        );
        let (_, _, _, initial_input, _, _, _, _, goal) = prepared.into_parts();
        assert_eq!(initial_input.session(), session);
        assert_eq!(goal.session(), session);
        assert!(matches!(goal.action(), GoalUserAction::Attach(_)));
    }

    #[test]
    fn a_resolved_template_other_than_the_requested_one_is_refused() {
        let mut ids = UuidV7CommissionedDispatchIdGenerator;

        let refused = request(9).prepare(
            &mut ids,
            template_provenance("another-template"),
            defaults(),
        );

        assert_eq!(
            refused.map(|_| ()).unwrap_err(),
            CommissionDispatchPreparationError::TemplateMismatch
        );
    }
}
