//! Atomic PostgreSQL creation and checked loading for imported-seeded sessions.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    CreateSessionFromImportedFrontierOutcome, CreateSessionFromImportedFrontierTransaction,
};
use signalbox_domain::{
    BoundedImportedSessionReconstitutionFailure, BoundedImportedSessionReconstitutionInput,
    ContextFrontierId, CreateSessionFromImportedFrontier,
    CreateSessionFromImportedFrontierPreparationFailure,
    CreateSessionFromImportedFrontierReconstitutionFailure,
    CreateSessionFromImportedFrontierReconstitutionInput, DirectModelSelection, DurableCommandId,
    ImportedConversation, ImportedConversationId, ImportedSessionRelationship,
    ImportedSessionSeedHeaderReconstitutionInput, ImportedSessionSeedReconstitutionInput,
    ImportedTranscriptEntryId, ImportedTranscriptPosition, ModelAlias, ModelSelectionRequest,
    PreparedCreateSessionFromImportedFrontier, ReconstitutedSessionCreationFromImportedFrontier,
    ResolvedContextFrontierReconstitutionInput, SemanticTranscriptEntryId,
    SemanticTranscriptEntryPayload, SemanticTranscriptEntryReconstitutionInput,
    SemanticTranscriptEntryRef, Session, SessionConfigurationDefaults,
    SessionConfigurationDefaultsVersion, SessionCreationCause, SessionCreationProvenance,
    SessionId, TranscriptAncestry,
};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{
        self, CREATE_SESSION_FROM_IMPORTED_FRONTIER_KIND, CommandKind, RegistryCorruption,
        RegistryInspectionError,
    },
    conversation_import::{
        self, ImportedConversationCorruption, ImportedConversationIdentityCollision,
        ImportedConversationRepositoryError,
    },
    mapping::{
        DurableCommandIdMappingError, PositiveOrdinalMappingError, defaults_version_from_numeric,
        defaults_version_to_numeric, durable_command_id_from_uuid, durable_command_id_to_uuid,
        session_id_from_uuid, session_id_to_uuid,
    },
    outbox,
};

const STORAGE_VERSION: i16 = 1;
const OWNER_INITIATED: &str = "owner_initiated";
const IMPORTED_ANCESTRY: &str = "imported_conversation";
const APPLIED: &str = "applied";

/// A durable imported-session shape that cannot reconstruct its domain value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportedSessionCorruption {
    /// One required durable value is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// Durable field being decoded.
        field: &'static str,
        /// Unsupported durable spelling.
        value: String,
    },
    /// Independently stored values disagree.
    Inconsistent(&'static str),
    /// A stored positive ordinal cannot construct its domain value.
    InvalidOrdinal {
        /// Durable field carrying the ordinal.
        field: &'static str,
        /// Why the numeric value is invalid.
        reason: PositiveOrdinalMappingError,
    },
    /// A stored command identity is a reserved sentinel UUID.
    InvalidCommandIdentity {
        /// Durable field carrying the identity.
        field: &'static str,
        /// Why the UUID cannot construct a command identity.
        reason: DurableCommandIdMappingError,
    },
    /// The referenced imported aggregate cannot be reconstructed.
    ImportedConversation(ImportedConversationCorruption),
    /// Stored creation facts fail domain-owned correlation.
    CreationDomain(CreateSessionFromImportedFrontierReconstitutionFailure),
    /// Stored bounded current-session facts fail domain-owned correlation.
    BoundedCurrentDomain(BoundedImportedSessionReconstitutionFailure),
}

impl fmt::Display for ImportedSessionCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing imported-session {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported imported-session {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent imported-session {relationship}")
            }
            Self::InvalidOrdinal { field, reason } => {
                write!(formatter, "invalid imported-session {field}: {reason}")
            }
            Self::InvalidCommandIdentity { field, reason } => {
                write!(formatter, "invalid imported-session {field}: {reason}")
            }
            Self::ImportedConversation(error) => error.fmt(formatter),
            Self::CreationDomain(failure) => {
                write!(
                    formatter,
                    "imported-session creation reconstitution failed: {failure:?}"
                )
            }
            Self::BoundedCurrentDomain(failure) => {
                write!(
                    formatter,
                    "bounded current imported-session reconstitution failed: {failure:?}"
                )
            }
        }
    }
}

impl Error for ImportedSessionCorruption {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidCommandIdentity { reason, .. } => Some(reason),
            Self::ImportedConversation(error) => Some(error),
            _ => None,
        }
    }
}

/// Which generated imported-session identity collided with durable state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportedSessionIdentityCollision {
    /// The proposed session identity already exists.
    Session,
    /// A proposed semantic-entry identity already exists.
    SemanticEntry,
    /// The proposed seed context-frontier identity already exists.
    SeedFrontier,
}

impl fmt::Display for ImportedSessionIdentityCollision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let identity = match self {
            Self::Session => "session",
            Self::SemanticEntry => "semantic-entry",
            Self::SeedFrontier => "seed context-frontier",
        };
        write!(formatter, "{identity} identity already exists")
    }
}

impl Error for ImportedSessionIdentityCollision {}

/// PostgreSQL or fail-closed imported-session repository failure.
#[derive(Debug)]
pub enum ImportedSessionRepositoryError {
    /// PostgreSQL failed before a commit could have succeeded.
    Database(sqlx::Error),
    /// PostgreSQL obscured whether the requested commit succeeded.
    CommitAmbiguous(sqlx::Error),
    /// The command identity is valid but belongs to another command family.
    DifferentCommandKind {
        /// The cross-kind command identity.
        command_id: DurableCommandId,
    },
    /// Application-supplied identities could not form a checked candidate.
    Preparation(CreateSessionFromImportedFrontierPreparationFailure),
    /// A supplied fresh identity already names a durable record.
    IdentityCollision(ImportedSessionIdentityCollision),
    /// Durable facts cannot reconstruct their admitted domain values.
    Corruption(ImportedSessionCorruption),
}

impl fmt::Display for ImportedSessionRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "imported-session database failure: {error}")
            }
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "imported-session commit outcome is ambiguous: {error}"
                )
            }
            Self::DifferentCommandKind { command_id } => write!(
                formatter,
                "durable command {command_id:?} does not name CreateSessionFromImportedFrontier"
            ),
            Self::Preparation(failure) => {
                write!(
                    formatter,
                    "imported-session candidate preparation failed: {failure:?}"
                )
            }
            Self::IdentityCollision(error) => error.fmt(formatter),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for ImportedSessionRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::DifferentCommandKind { .. } | Self::Preparation(_) => None,
            Self::IdentityCollision(error) => Some(error),
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for ImportedSessionRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ImportedSessionCorruption> for ImportedSessionRepositoryError {
    fn from(error: ImportedSessionCorruption) -> Self {
        Self::Corruption(error)
    }
}

impl ImportedSessionRepositoryError {
    fn from_commit_failure(error: sqlx::Error) -> Self {
        if crate::commit_failure_is_ambiguous(&error) {
            Self::CommitAmbiguous(error)
        } else {
            Self::Database(error)
        }
    }

    fn from_insert_failure(error: sqlx::Error) -> Self {
        identity_collision(&error).map_or_else(|| Self::Database(error), Self::IdentityCollision)
    }
}

/// PostgreSQL implementation of later session creation from imported history.
#[derive(Clone, Debug)]
pub struct ImportedSessionRepository {
    pool: PgPool,
}

impl ImportedSessionRepository {
    /// Uses the supplied pool for claim-first creation and checked replay.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Handles one canonical imported-frontier creation atomically.
    pub async fn handle<NextSemanticEntryId>(
        &self,
        command: CreateSessionFromImportedFrontier,
        session: SessionId,
        seed_frontier: ContextFrontierId,
        mut next_semantic_entry_id: NextSemanticEntryId,
    ) -> Result<CreateSessionFromImportedFrontierOutcome, ImportedSessionRepositoryError>
    where
        NextSemanticEntryId: FnMut() -> SemanticTranscriptEntryId + Send,
    {
        let command_id = command.command_id();
        let mut transaction = self.pool.begin().await?;

        if let Some(kind) = inspect_registry(&mut transaction, command_id).await? {
            let outcome = existing_outcome(&mut transaction, command, kind).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let conversation =
            match load_imported_conversation(&mut transaction, command.imported_conversation())
                .await?
            {
                Some(conversation) => conversation,
                None => {
                    transaction.rollback().await?;
                    return Ok(
                        CreateSessionFromImportedFrontierOutcome::ImportedConversationNotFound {
                            conversation: command.imported_conversation(),
                        },
                    );
                }
            };
        if conversation.prefix(command.imported_frontier()).is_none() {
            transaction.rollback().await?;
            return Ok(
                CreateSessionFromImportedFrontierOutcome::ImportedFrontierNotFound {
                    frontier: command.imported_frontier(),
                },
            );
        }

        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at)
             VALUES ($1, $2, $3, transaction_timestamp())
             ON CONFLICT DO NOTHING",
        )
        .bind(durable_command_id_to_uuid(command_id))
        .bind(CREATE_SESSION_FROM_IMPORTED_FRONTIER_KIND)
        .bind(STORAGE_VERSION)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;

        if !claimed {
            let kind = inspect_registry(&mut transaction, command_id)
                .await?
                .ok_or(ImportedSessionCorruption::Inconsistent(
                    "winner claim disappeared",
                ))?;
            let outcome = existing_outcome(&mut transaction, command, kind).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let prepared = match command.prepare(
            &conversation,
            session,
            seed_frontier,
            &mut next_semantic_entry_id,
        ) {
            Ok(prepared) => prepared,
            Err(error) => {
                transaction.rollback().await?;
                return Err(ImportedSessionRepositoryError::Preparation(error.failure()));
            }
        };
        let result = prepared.applied_result();
        if let Err(error) = insert_prepared(&mut transaction, prepared).await {
            transaction.rollback().await?;
            return Err(error);
        }
        transaction
            .commit()
            .await
            .map_err(ImportedSessionRepositoryError::from_commit_failure)?;
        Ok(CreateSessionFromImportedFrontierOutcome::Applied(result))
    }

    /// Loads one complete claimed imported creation, or `None` for an unseen
    /// command identity.
    pub async fn load(
        &self,
        command_id: DurableCommandId,
    ) -> Result<
        Option<ReconstitutedSessionCreationFromImportedFrontier>,
        ImportedSessionRepositoryError,
    > {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::CreateSessionFromImportedFrontier) => {
                load_creation_from_connection(&mut connection, command_id).await
            }
            Some(_) => Err(ImportedSessionRepositoryError::DifferentCommandKind { command_id }),
        }
    }
}

impl CreateSessionFromImportedFrontierTransaction for ImportedSessionRepository {
    type Error = ImportedSessionRepositoryError;

    async fn handle<NextSemanticEntryId>(
        &mut self,
        command: CreateSessionFromImportedFrontier,
        session: SessionId,
        seed_frontier: ContextFrontierId,
        next_semantic_entry_id: NextSemanticEntryId,
    ) -> Result<CreateSessionFromImportedFrontierOutcome, Self::Error>
    where
        NextSemanticEntryId: FnMut() -> SemanticTranscriptEntryId + Send,
    {
        ImportedSessionRepository::handle(
            self,
            command,
            session,
            seed_frontier,
            next_semantic_entry_id,
        )
        .await
    }
}

async fn existing_outcome(
    connection: &mut PgConnection,
    command: CreateSessionFromImportedFrontier,
    kind: CommandKind,
) -> Result<CreateSessionFromImportedFrontierOutcome, ImportedSessionRepositoryError> {
    if kind != CommandKind::CreateSessionFromImportedFrontier {
        return Ok(CreateSessionFromImportedFrontierOutcome::ConflictingReuse {
            command_id: command.command_id(),
        });
    }
    let recorded = load_creation_from_connection(connection, command.command_id())
        .await?
        .ok_or(ImportedSessionCorruption::Inconsistent(
            "registry entry disappeared",
        ))?;
    Ok(if &command == recorded.command() {
        CreateSessionFromImportedFrontierOutcome::Applied(recorded.applied_result())
    } else {
        CreateSessionFromImportedFrontierOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    })
}

async fn insert_prepared(
    connection: &mut PgConnection,
    prepared: PreparedCreateSessionFromImportedFrontier,
) -> Result<(), ImportedSessionRepositoryError> {
    let command = prepared.command();
    let session = prepared.session();
    let defaults = session.configuration_defaults();
    let command_selection = encode_selection(command.initial_configuration_defaults().model());
    let stored_selection = encode_selection(defaults.defaults().model());
    let frontier = command.imported_frontier();
    let relationship = encode_relationship(command.relationship());

    sqlx::query(
        "INSERT INTO session
            (session_id, creation_cause, ancestry_kind,
             imported_conversation_id, imported_frontier_entry_id,
             imported_frontier_position, imported_relationship_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(OWNER_INITIATED)
    .bind(IMPORTED_ANCESTRY)
    .bind(frontier.conversation().into_uuid())
    .bind(frontier.through_entry().into_uuid())
    .bind(Decimal::from(frontier.through_position().as_u64()))
    .bind(relationship)
    .execute(&mut *connection)
    .await
    .map_err(ImportedSessionRepositoryError::from_insert_failure)?;

    sqlx::query("INSERT INTO session_scheduler (session_id) VALUES ($1)")
        .bind(session_id_to_uuid(session.id()))
        .execute(&mut *connection)
        .await?;

    sqlx::query(
        "INSERT INTO session_defaults_version
            (session_id, version, model_selection_kind,
             direct_model_selection_id, model_alias_id)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(defaults_version_to_numeric(defaults.version()))
    .bind(stored_selection.kind)
    .bind(stored_selection.direct)
    .bind(stored_selection.alias)
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO session_current_defaults (session_id, current_version)
         VALUES ($1, $2)",
    )
    .bind(session_id_to_uuid(session.id()))
    .bind(defaults_version_to_numeric(defaults.version()))
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO create_session_from_imported_frontier_command
            (command_id, command_kind, storage_version,
             imported_conversation_id, imported_frontier_entry_id,
             imported_frontier_position, imported_relationship_kind,
             creation_cause, ancestry_kind, initial_defaults_version,
             model_selection_kind, direct_model_selection_id, model_alias_id,
             result_kind, created_session_id)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
             $14, $15)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(CREATE_SESSION_FROM_IMPORTED_FRONTIER_KIND)
    .bind(STORAGE_VERSION)
    .bind(frontier.conversation().into_uuid())
    .bind(frontier.through_entry().into_uuid())
    .bind(Decimal::from(frontier.through_position().as_u64()))
    .bind(relationship)
    .bind(OWNER_INITIATED)
    .bind(IMPORTED_ANCESTRY)
    .bind(defaults_version_to_numeric(defaults.version()))
    .bind(command_selection.kind)
    .bind(command_selection.direct)
    .bind(command_selection.alias)
    .bind(APPLIED)
    .bind(session_id_to_uuid(prepared.applied_result().session()))
    .execute(&mut *connection)
    .await?;

    for entry in prepared.semantic_entries() {
        let SemanticTranscriptEntryPayload::Imported { imported_entry, .. } = entry.payload()
        else {
            return Err(
                ImportedSessionCorruption::Inconsistent("prepared semantic payload").into(),
            );
        };
        sqlx::query(
            "INSERT INTO semantic_transcript_entry
                (source_session_id, semantic_entry_id, payload_kind,
                 imported_conversation_id, imported_transcript_entry_id)
             VALUES ($1, $2, 'imported_entry', $3, $4)",
        )
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.identity().into_uuid())
        .bind(frontier.conversation().into_uuid())
        .bind(imported_entry.into_uuid())
        .execute(&mut *connection)
        .await
        .map_err(ImportedSessionRepositoryError::from_insert_failure)?;
    }

    let seed_snapshot = prepared.seed_snapshot();
    let seed_context = seed_snapshot.frontier();
    let member_count = u64::try_from(seed_snapshot.entry_count())
        .map_err(|_| ImportedSessionCorruption::Inconsistent("seed member count"))?;
    sqlx::query(
        "INSERT INTO context_frontier
            (owning_session_id, context_frontier_id, member_count)
         VALUES ($1, $2, $3)",
    )
    .bind(session_id_to_uuid(seed_context.owning_session()))
    .bind(seed_context.snapshot().into_uuid())
    .bind(Decimal::from(member_count))
    .execute(&mut *connection)
    .await
    .map_err(ImportedSessionRepositoryError::from_insert_failure)?;
    for (index, entry) in seed_snapshot.ordered_entries().enumerate() {
        let position = u64::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .ok_or(ImportedSessionCorruption::Inconsistent(
                "seed member position",
            ))?;
        sqlx::query(
            "INSERT INTO context_frontier_member
                (owning_session_id, context_frontier_id, member_position,
                 source_session_id, semantic_entry_id)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(session_id_to_uuid(seed_context.owning_session()))
        .bind(seed_context.snapshot().into_uuid())
        .bind(Decimal::from(position))
        .bind(session_id_to_uuid(entry.source_session()))
        .bind(entry.entry().into_uuid())
        .execute(&mut *connection)
        .await?;
    }

    let seed = prepared.imported_seed();
    sqlx::query(
        "INSERT INTO imported_session_seed
            (session_id, seed_context_frontier_id)
         VALUES ($1, $2)",
    )
    .bind(session_id_to_uuid(seed.session()))
    .bind(seed.seed_frontier().into_uuid())
    .execute(&mut *connection)
    .await?;

    outbox::append(
        connection,
        outbox::OutboxEvent::SessionCreated {
            session: session.id(),
        },
    )
    .await?;
    Ok(())
}

async fn load_creation_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedSessionCreationFromImportedFrontier>, ImportedSessionRepositoryError>
{
    let row = sqlx::query(
        "SELECT
            d.command_kind AS registry_kind,
            d.storage_version AS registry_version,
            c.command_id AS typed_command_id,
            c.command_kind AS typed_kind,
            c.storage_version AS typed_version,
            c.imported_conversation_id AS command_conversation_id,
            c.imported_frontier_entry_id AS command_frontier_entry_id,
            c.imported_frontier_position AS command_frontier_position,
            c.imported_relationship_kind AS command_relationship_kind,
            c.creation_cause AS command_cause,
            c.ancestry_kind AS command_ancestry,
            c.initial_defaults_version,
            c.model_selection_kind AS command_model_kind,
            c.direct_model_selection_id AS command_direct_id,
            c.model_alias_id AS command_alias_id,
            c.result_kind,
            c.created_session_id AS result_session_id,
            s.session_id AS stored_session_id,
            s.creation_cause AS stored_cause,
            s.ancestry_kind AS stored_ancestry,
            s.imported_conversation_id AS stored_conversation_id,
            s.imported_frontier_entry_id AS stored_frontier_entry_id,
            s.imported_frontier_position AS stored_frontier_position,
            s.imported_relationship_kind AS stored_relationship_kind,
            v.session_id AS defaults_session_id,
            v.version AS stored_defaults_version,
            v.model_selection_kind AS stored_model_kind,
            v.direct_model_selection_id AS stored_direct_id,
            v.model_alias_id AS stored_alias_id
         FROM durable_command AS d
         LEFT JOIN create_session_from_imported_frontier_command AS c
           ON c.command_id = d.command_id
         LEFT JOIN session AS s
           ON s.session_id = c.created_session_id
         LEFT JOIN session_defaults_version AS v
           ON v.session_id = c.created_session_id
          AND v.version = c.initial_defaults_version
         WHERE d.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };

    require_spelling(
        &row,
        "registry_kind",
        CREATE_SESSION_FROM_IMPORTED_FRONTIER_KIND,
    )?;
    require_version(&row, "registry_version", STORAGE_VERSION)?;
    let stored_command = durable_command_id_from_uuid(required(&row, "typed_command_id")?)
        .map_err(|reason| ImportedSessionCorruption::InvalidCommandIdentity {
            field: "typed command identity",
            reason,
        })?;
    if stored_command != command_id {
        return Err(ImportedSessionCorruption::Inconsistent("typed command identity").into());
    }
    require_spelling(
        &row,
        "typed_kind",
        CREATE_SESSION_FROM_IMPORTED_FRONTIER_KIND,
    )?;
    require_version(&row, "typed_version", STORAGE_VERSION)?;
    require_spelling(&row, "command_cause", OWNER_INITIATED)?;
    require_spelling(&row, "command_ancestry", IMPORTED_ANCESTRY)?;
    require_spelling(&row, "result_kind", APPLIED)?;

    let command_conversation =
        ImportedConversationId::from_uuid(required(&row, "command_conversation_id")?);
    let conversation = load_imported_conversation(connection, command_conversation)
        .await?
        .ok_or(ImportedSessionCorruption::Missing("imported conversation"))?;
    let command_frontier = decode_frontier(
        &conversation,
        required(&row, "command_frontier_entry_id")?,
        required(&row, "command_frontier_position")?,
        "command imported frontier",
    )?;
    let command_relationship = decode_relationship(required(&row, "command_relationship_kind")?)?;
    let initial_version = decode_ordinal(&row, "initial_defaults_version")?;
    if initial_version != SessionConfigurationDefaultsVersion::first() {
        return Err(
            ImportedSessionCorruption::Inconsistent("command initial defaults version").into(),
        );
    }
    let command_defaults = decode_selection(
        required(&row, "command_model_kind")?,
        row.try_get("command_direct_id")?,
        row.try_get("command_alias_id")?,
        "command model selection",
    )?;
    let command = CreateSessionFromImportedFrontier::new(
        stored_command,
        command_frontier,
        command_relationship,
        command_defaults,
    );

    let result_session = session_id_from_uuid(required(&row, "result_session_id")?);
    let stored_session = session_id_from_uuid(required(&row, "stored_session_id")?);
    let provenance = decode_stored_provenance(&row, &conversation)?;
    let defaults_session = session_id_from_uuid(required(&row, "defaults_session_id")?);
    let defaults_version = decode_ordinal(&row, "stored_defaults_version")?;
    let defaults = decode_selection(
        required(&row, "stored_model_kind")?,
        row.try_get("stored_direct_id")?,
        row.try_get("stored_alias_id")?,
        "stored model selection",
    )?;
    let projection = load_seed_projection(connection, stored_session, &conversation).await?;

    CreateSessionFromImportedFrontierReconstitutionInput::new(
        command,
        result_session,
        stored_session,
        provenance,
        defaults_session,
        defaults_version,
        defaults,
        conversation,
        projection.seed_records,
        projection.seed_snapshots,
        projection.semantic_entries,
    )
    .reconstitute()
    .map(Some)
    .map_err(|error| ImportedSessionCorruption::CreationDomain(error.failure()).into())
}

fn identity_collision(error: &sqlx::Error) -> Option<ImportedSessionIdentityCollision> {
    match error
        .as_database_error()
        .and_then(|database| database.constraint())
    {
        Some("session_pkey") => Some(ImportedSessionIdentityCollision::Session),
        Some("semantic_transcript_entry_pk" | "semantic_transcript_entry_id_global") => {
            Some(ImportedSessionIdentityCollision::SemanticEntry)
        }
        Some("context_frontier_pk" | "context_frontier_id_global") => {
            Some(ImportedSessionIdentityCollision::SeedFrontier)
        }
        _ => None,
    }
}

pub(crate) fn reconstitute_bounded_current(
    requested_session: SessionId,
    row: PgRow,
) -> Result<Session, ImportedSessionRepositoryError> {
    require_spelling(&row, "stored_cause", OWNER_INITIATED)?;
    require_spelling(&row, "stored_ancestry", IMPORTED_ANCESTRY)?;
    let stored_session = session_id_from_uuid(required(&row, "stored_session_id")?);
    let imported_conversation =
        ImportedConversationId::from_uuid(required(&row, "stored_conversation_id")?);
    let imported_frontier_entry =
        ImportedTranscriptEntryId::from_uuid(required(&row, "stored_frontier_entry_id")?);
    let imported_frontier_position = ImportedTranscriptPosition::try_from_u64(positive_u64(
        required(&row, "stored_frontier_position")?,
        "stored imported frontier position",
    )?)
    .ok_or(ImportedSessionCorruption::Inconsistent(
        "stored imported frontier position",
    ))?;
    let imported_relationship = decode_relationship(required(&row, "stored_relationship_kind")?)?;
    let current_defaults_session =
        session_id_from_uuid(required(&row, "current_defaults_session_id")?);
    let current_defaults_version = decode_ordinal(&row, "current_version")?;
    let defaults_session = session_id_from_uuid(required(&row, "selected_defaults_session_id")?);
    let defaults_version = decode_ordinal(&row, "selected_defaults_version")?;
    let defaults = decode_selection(
        required(&row, "model_selection_kind")?,
        row.try_get("direct_model_selection_id")?,
        row.try_get("model_alias_id")?,
        "model selection",
    )?;

    let seed_records = match (
        row.try_get::<Option<Uuid>, _>("seed_session_id")?,
        row.try_get::<Option<Uuid>, _>("seed_context_frontier_id")?,
    ) {
        (None, None) => Vec::new(),
        (Some(session), Some(frontier)) => vec![ImportedSessionSeedReconstitutionInput::new(
            session_id_from_uuid(session),
            ContextFrontierId::from_uuid(frontier),
        )],
        _ => {
            return Err(ImportedSessionCorruption::Inconsistent("seed record shape").into());
        }
    };
    let seed_headers = match (
        row.try_get::<Option<Uuid>, _>("seed_frontier_session_id")?,
        row.try_get::<Option<Uuid>, _>("seed_frontier_id")?,
        row.try_get::<Option<Decimal>, _>("seed_frontier_member_count")?,
    ) {
        (None, None, None) => Vec::new(),
        (Some(session), Some(frontier), Some(member_count)) => {
            vec![ImportedSessionSeedHeaderReconstitutionInput::new(
                session_id_from_uuid(session),
                ContextFrontierId::from_uuid(frontier),
                positive_u64(member_count, "seed frontier member count")?,
            )]
        }
        _ => {
            return Err(ImportedSessionCorruption::Inconsistent("seed frontier shape").into());
        }
    };

    BoundedImportedSessionReconstitutionInput::from_stored_imported_parts(
        requested_session,
        stored_session,
        SessionCreationCause::OwnerInitiated,
        imported_conversation,
        imported_frontier_entry,
        imported_frontier_position,
        imported_relationship,
        current_defaults_session,
        current_defaults_version,
        defaults_session,
        defaults_version,
        defaults,
        seed_records,
        seed_headers,
    )
    .reconstitute()
    .map_err(|error| ImportedSessionCorruption::BoundedCurrentDomain(error.failure()).into())
}

struct SeedProjection {
    seed_records: Vec<ImportedSessionSeedReconstitutionInput>,
    seed_snapshots: Vec<ResolvedContextFrontierReconstitutionInput>,
    semantic_entries: Vec<SemanticTranscriptEntryReconstitutionInput>,
}

async fn load_seed_projection(
    connection: &mut PgConnection,
    session: SessionId,
    conversation: &ImportedConversation,
) -> Result<SeedProjection, ImportedSessionRepositoryError> {
    let rows = sqlx::query(
        "SELECT
            seed.session_id AS seed_session_id,
            seed.seed_context_frontier_id,
            frontier.owning_session_id AS frontier_session_id,
            frontier.context_frontier_id,
            frontier.member_count,
            member.member_position,
            member.source_session_id AS member_source_session_id,
            member.semantic_entry_id AS member_semantic_entry_id
         FROM imported_session_seed AS seed
         LEFT JOIN context_frontier AS frontier
           ON frontier.owning_session_id = seed.session_id
          AND frontier.context_frontier_id = seed.seed_context_frontier_id
         LEFT JOIN context_frontier_member AS member
           ON member.owning_session_id = frontier.owning_session_id
          AND member.context_frontier_id = frontier.context_frontier_id
         WHERE seed.session_id = $1
         ORDER BY member.member_position",
    )
    .bind(session_id_to_uuid(session))
    .fetch_all(&mut *connection)
    .await?;

    let mut seed_records = Vec::new();
    let mut seed_snapshots = Vec::new();
    if let Some(first) = rows.first() {
        let seed_session = session_id_from_uuid(required(first, "seed_session_id")?);
        let seed_frontier =
            ContextFrontierId::from_uuid(required(first, "seed_context_frontier_id")?);
        seed_records.push(ImportedSessionSeedReconstitutionInput::new(
            seed_session,
            seed_frontier,
        ));

        let frontier_session: Option<Uuid> = first.try_get("frontier_session_id")?;
        let frontier_id: Option<Uuid> = first.try_get("context_frontier_id")?;
        let member_count: Option<Decimal> = first.try_get("member_count")?;
        match (frontier_session, frontier_id, member_count) {
            (None, None, None) => {}
            (Some(frontier_session), Some(frontier_id), Some(member_count)) => {
                let declared_count = positive_u64(member_count, "seed member count")?;
                let mut members = Vec::with_capacity(rows.len());
                for (index, row) in rows.iter().enumerate() {
                    let position: Option<Decimal> = row.try_get("member_position")?;
                    let source: Option<Uuid> = row.try_get("member_source_session_id")?;
                    let entry: Option<Uuid> = row.try_get("member_semantic_entry_id")?;
                    match (position, source, entry) {
                        (None, None, None) => {}
                        (Some(position), Some(source), Some(entry)) => {
                            let actual = positive_u64(position, "seed member position")?;
                            let expected = u64::try_from(index)
                                .ok()
                                .and_then(|index| index.checked_add(1))
                                .ok_or(ImportedSessionCorruption::Inconsistent(
                                    "seed member position",
                                ))?;
                            if actual != expected {
                                return Err(ImportedSessionCorruption::Inconsistent(
                                    "seed member ordering",
                                )
                                .into());
                            }
                            members.push(SemanticTranscriptEntryRef::from_source(
                                session_id_from_uuid(source),
                                SemanticTranscriptEntryId::from_uuid(entry),
                            ));
                        }
                        _ => {
                            return Err(ImportedSessionCorruption::Inconsistent(
                                "seed member shape",
                            )
                            .into());
                        }
                    }
                }
                if u64::try_from(members.len()).ok() != Some(declared_count) {
                    return Err(ImportedSessionCorruption::Inconsistent("seed member count").into());
                }
                seed_snapshots.push(ResolvedContextFrontierReconstitutionInput::new(
                    session_id_from_uuid(frontier_session),
                    ContextFrontierId::from_uuid(frontier_id),
                    members,
                ));
            }
            _ => {
                return Err(ImportedSessionCorruption::Inconsistent("seed frontier shape").into());
            }
        }
    }

    let semantic_rows = sqlx::query(
        "SELECT
            semantic.source_session_id,
            semantic.semantic_entry_id,
            semantic.imported_conversation_id,
            semantic.imported_transcript_entry_id,
            imported.imported_entry_position
         FROM semantic_transcript_entry AS semantic
         LEFT JOIN imported_transcript_entry AS imported
           ON imported.imported_conversation_id =
                  semantic.imported_conversation_id
          AND imported.imported_transcript_entry_id =
                  semantic.imported_transcript_entry_id
         WHERE semantic.source_session_id = $1
           AND semantic.payload_kind = 'imported_entry'
         ORDER BY imported.imported_entry_position",
    )
    .bind(session_id_to_uuid(session))
    .fetch_all(&mut *connection)
    .await?;
    let mut semantic_entries = Vec::with_capacity(semantic_rows.len());
    for row in semantic_rows {
        let source_session = session_id_from_uuid(required(&row, "source_session_id")?);
        let identity = SemanticTranscriptEntryId::from_uuid(required(&row, "semantic_entry_id")?);
        let semantic_conversation =
            ImportedConversationId::from_uuid(required(&row, "imported_conversation_id")?);
        if semantic_conversation != conversation.id() {
            return Err(
                ImportedSessionCorruption::Inconsistent("semantic imported conversation").into(),
            );
        }
        let imported_identity =
            ImportedTranscriptEntryId::from_uuid(required(&row, "imported_transcript_entry_id")?);
        let imported = conversation
            .entries()
            .iter()
            .find(|entry| entry.identity() == imported_identity)
            .ok_or(ImportedSessionCorruption::Inconsistent(
                "semantic imported entry",
            ))?;
        semantic_entries.push(SemanticTranscriptEntryReconstitutionInput::new(
            identity,
            source_session,
            SemanticTranscriptEntryPayload::Imported {
                imported_entry: imported_identity,
                source_speaker: imported.source_speaker().clone(),
                content: imported.content().clone(),
            },
        ));
    }

    Ok(SeedProjection {
        seed_records,
        seed_snapshots,
        semantic_entries,
    })
}

fn decode_stored_provenance(
    row: &PgRow,
    conversation: &ImportedConversation,
) -> Result<SessionCreationProvenance, ImportedSessionRepositoryError> {
    require_spelling(row, "stored_cause", OWNER_INITIATED)?;
    require_spelling(row, "stored_ancestry", IMPORTED_ANCESTRY)?;
    let frontier = decode_frontier(
        conversation,
        required(row, "stored_frontier_entry_id")?,
        required(row, "stored_frontier_position")?,
        "stored imported frontier",
    )?;
    let stored_conversation =
        ImportedConversationId::from_uuid(required(row, "stored_conversation_id")?);
    if stored_conversation != conversation.id() {
        return Err(ImportedSessionCorruption::Inconsistent("stored imported conversation").into());
    }
    Ok(SessionCreationProvenance::new(
        SessionCreationCause::OwnerInitiated,
        TranscriptAncestry::ImportedConversation {
            source_frontier: frontier,
            relationship: decode_relationship(required(row, "stored_relationship_kind")?)?,
        },
    ))
}

fn decode_frontier(
    conversation: &ImportedConversation,
    entry: Uuid,
    position: Decimal,
    field: &'static str,
) -> Result<signalbox_domain::ImportedTranscriptFrontier, ImportedSessionRepositoryError> {
    let entry = ImportedTranscriptEntryId::from_uuid(entry);
    let frontier = conversation
        .frontier_for_entry(entry)
        .ok_or(ImportedSessionCorruption::Inconsistent(field))?;
    let position = positive_u64(position, field)?;
    if frontier.through_position().as_u64() != position {
        return Err(ImportedSessionCorruption::Inconsistent(field).into());
    }
    Ok(frontier)
}

fn encode_relationship(relationship: ImportedSessionRelationship) -> &'static str {
    match relationship {
        ImportedSessionRelationship::Resume => "resume",
        ImportedSessionRelationship::Fork => "fork",
    }
}

fn decode_relationship(
    relationship: String,
) -> Result<ImportedSessionRelationship, ImportedSessionRepositoryError> {
    match relationship.as_str() {
        "resume" => Ok(ImportedSessionRelationship::Resume),
        "fork" => Ok(ImportedSessionRelationship::Fork),
        _ => Err(ImportedSessionCorruption::Unsupported {
            field: "imported relationship",
            value: relationship,
        }
        .into()),
    }
}

struct EncodedSelection {
    kind: &'static str,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
}

fn encode_selection(selection: ModelSelectionRequest) -> EncodedSelection {
    match selection {
        ModelSelectionRequest::Direct(value) => EncodedSelection {
            kind: "direct",
            direct: Some(value.into_uuid()),
            alias: None,
        },
        ModelSelectionRequest::Alias(value) => EncodedSelection {
            kind: "alias",
            direct: None,
            alias: Some(value.into_uuid()),
        },
    }
}

fn decode_selection(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    field: &'static str,
) -> Result<SessionConfigurationDefaults, ImportedSessionRepositoryError> {
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(value))
        }
        ("alias", None, Some(value)) => ModelSelectionRequest::Alias(ModelAlias::from_uuid(value)),
        ("direct" | "alias", _, _) => {
            return Err(ImportedSessionCorruption::Inconsistent(field).into());
        }
        _ => {
            return Err(ImportedSessionCorruption::Unsupported { field, value: kind }.into());
        }
    };
    Ok(SessionConfigurationDefaults::new(model))
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, ImportedSessionRepositoryError>
where
    for<'r> T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| ImportedSessionCorruption::Missing(field).into())
}

fn require_spelling(
    row: &PgRow,
    field: &'static str,
    expected: &str,
) -> Result<(), ImportedSessionRepositoryError> {
    let actual: String = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ImportedSessionCorruption::Unsupported {
            field,
            value: actual,
        }
        .into())
    }
}

fn require_version(
    row: &PgRow,
    field: &'static str,
    expected: i16,
) -> Result<(), ImportedSessionRepositoryError> {
    let actual: i16 = required(row, field)?;
    if actual == expected {
        Ok(())
    } else {
        Err(ImportedSessionCorruption::Unsupported {
            field,
            value: actual.to_string(),
        }
        .into())
    }
}

fn decode_ordinal(
    row: &PgRow,
    field: &'static str,
) -> Result<SessionConfigurationDefaultsVersion, ImportedSessionRepositoryError> {
    let value: Decimal = required(row, field)?;
    defaults_version_from_numeric(value)
        .map_err(|reason| ImportedSessionCorruption::InvalidOrdinal { field, reason }.into())
}

fn positive_u64(
    value: Decimal,
    field: &'static str,
) -> Result<u64, ImportedSessionRepositoryError> {
    if !value.fract().is_zero() || value <= Decimal::ZERO {
        return Err(ImportedSessionCorruption::InvalidOrdinal {
            field,
            reason: if !value.fract().is_zero() {
                PositiveOrdinalMappingError::Fractional
            } else {
                PositiveOrdinalMappingError::NonPositive
            },
        }
        .into());
    }
    u64::try_from(value).map_err(|_| {
        ImportedSessionCorruption::InvalidOrdinal {
            field,
            reason: PositiveOrdinalMappingError::OutOfRange,
        }
        .into()
    })
}

async fn load_imported_conversation(
    connection: &mut PgConnection,
    conversation: ImportedConversationId,
) -> Result<Option<ImportedConversation>, ImportedSessionRepositoryError> {
    conversation_import::load_from_connection(connection, conversation)
        .await
        .map_err(|error| match error {
            ImportedConversationRepositoryError::Database(error) => {
                ImportedSessionRepositoryError::Database(error)
            }
            ImportedConversationRepositoryError::IdentityCollision(
                ImportedConversationIdentityCollision::Conversation
                | ImportedConversationIdentityCollision::TranscriptEntry,
            ) => {
                ImportedSessionRepositoryError::Corruption(ImportedSessionCorruption::Inconsistent(
                    "imported conversation identity collision during load",
                ))
            }
            ImportedConversationRepositoryError::Corruption(error) => {
                ImportedSessionRepositoryError::Corruption(
                    ImportedSessionCorruption::ImportedConversation(error),
                )
            }
        })
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, ImportedSessionRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(map_registry_error)
}

fn map_registry_error(error: RegistryInspectionError) -> ImportedSessionRepositoryError {
    match error {
        RegistryInspectionError::Database(error) => error.into(),
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
            ImportedSessionCorruption::Unsupported {
                field: "registry_kind",
                value,
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(value)) => {
            ImportedSessionCorruption::Unsupported {
                field: "registry_version",
                value: value.to_string(),
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(_)) => {
            ImportedSessionCorruption::Missing("typed_command_id").into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::ConflictingTypedRecords) => {
            ImportedSessionCorruption::Inconsistent("typed command family").into()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::ImportedSessionRepositoryError;

    #[test]
    fn lost_commit_response_is_typed_as_ambiguous() {
        let error = ImportedSessionRepositoryError::from_commit_failure(sqlx::Error::Io(
            io::Error::new(io::ErrorKind::ConnectionReset, "commit response was lost"),
        ));

        assert!(matches!(
            error,
            ImportedSessionRepositoryError::CommitAmbiguous(_)
        ));
    }
}
