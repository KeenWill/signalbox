//! PostgreSQL storage for replaceable session metadata.
//!
//! The adapter keeps mutable organization state outside the conversational
//! aggregate, retains complete durable replacement receipts, and implements
//! bounded keyset pages from one repeatable-read snapshot.

use std::{error::Error, fmt};

use rust_decimal::Decimal;
use signalbox_application::{
    ReplaceSessionMetadataOutcome, ReplaceSessionMetadataTransaction, SessionMetadataListItem,
    SessionMetadataListQuery, SessionMetadataLister, SessionMetadataPageReader,
    SessionMetadataReader,
};
use signalbox_domain::{
    Actor, DirectModelSelection, DurableCommandId, ModelAlias, ModelSelectionRequest,
    ReconstitutedReplaceSessionMetadata, ReplaceSessionMetadata,
    ReplaceSessionMetadataReconstitutionFailure, ReplaceSessionMetadataReconstitutionInput,
    ReplaceSessionMetadataResult, SessionId, SessionMetadataContent, SessionMetadataContentError,
    SessionMetadataLastWriter, SessionMetadataSnapshot, SessionMetadataUpdatedAt, ToolRequestId,
    TurnId,
};
use sqlx::{PgConnection, PgPool, Postgres, Row, Transaction, postgres::PgRow, types::Uuid};

use crate::{
    command_registry::{
        self, CommandKind, REPLACE_SESSION_METADATA_KIND, RegistryCorruption,
        RegistryInspectionError,
    },
    commit_failure_is_ambiguous, lock_inventory,
    mapping::{
        PositiveOrdinalMappingError, dangerous_tool_auto_approval_from_str,
        defaults_version_from_numeric, durable_command_id_to_uuid, session_id_from_uuid,
        session_id_to_uuid, tool_request_id_from_uuid,
    },
};

const STORAGE_VERSION: i16 = 1;
const APPLIED: &str = "applied";
const REJECTED: &str = "rejected";
const SESSION_NOT_FOUND: &str = "session_not_found";
const REPEATABLE_READ_ONLY: &str = "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY";

/// The committed outcome of handling one metadata-replacement command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplaceSessionMetadataHandlingOutcome {
    /// First handling or equal replay returns the recorded result.
    Recorded(ReplaceSessionMetadataResult),
    /// The identifier already has a structurally different user-global use.
    ConflictingReuse {
        /// The user-global identifier whose existing meaning is retained.
        command_id: DurableCommandId,
    },
}

/// A durable metadata shape that cannot construct the public domain values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionMetadataCorruption {
    /// One required row or field is absent.
    Missing(&'static str),
    /// A closed discriminator or representation version is unsupported.
    Unsupported {
        /// The record field that could not be decoded.
        field: &'static str,
        /// The durable spelling that was observed.
        value: String,
    },
    /// Typed record relationships or variant fields disagree.
    Inconsistent(&'static str),
    /// A stored defaults version is not a positive domain ordinal.
    InvalidDefaultsVersion(PositiveOrdinalMappingError),
    /// A stored metadata string collection violates the domain boundary.
    InvalidContent(SessionMetadataContentError),
    /// A stored database timestamp is not a nonnegative integral u64 value.
    InvalidUpdatedAt,
    /// Complete receipt values fail domain-owned correlation.
    Domain(ReplaceSessionMetadataReconstitutionFailure),
}

impl fmt::Display for SessionMetadataCorruption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(field) => write!(formatter, "missing session metadata {field}"),
            Self::Unsupported { field, value } => {
                write!(formatter, "unsupported session metadata {field}: {value}")
            }
            Self::Inconsistent(relationship) => {
                write!(formatter, "inconsistent session metadata {relationship}")
            }
            Self::InvalidDefaultsVersion(reason) => {
                write!(
                    formatter,
                    "invalid metadata-list defaults version: {reason}"
                )
            }
            Self::InvalidContent(reason) => {
                write!(formatter, "invalid stored metadata content: {reason:?}")
            }
            Self::InvalidUpdatedAt => {
                formatter.write_str("invalid stored metadata update timestamp")
            }
            Self::Domain(reason) => {
                write!(
                    formatter,
                    "metadata receipt reconstitution failed: {reason:?}"
                )
            }
        }
    }
}

impl Error for SessionMetadataCorruption {}

/// A database failure, ambiguous commit, wrong load purpose, or corruption.
#[derive(Debug)]
pub enum SessionMetadataRepositoryError {
    /// PostgreSQL could not complete the operation.
    Database(sqlx::Error),
    /// PostgreSQL did not reveal whether the final commit took effect.
    CommitAmbiguous(sqlx::Error),
    /// A purpose-specific load named a valid command of another admitted kind.
    DifferentCommandKind {
        /// The user-global identifier that names another kind.
        command_id: DurableCommandId,
    },
    /// Durable rows cannot reconstruct the requested domain value.
    Corruption(SessionMetadataCorruption),
}

impl fmt::Display for SessionMetadataRepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => {
                write!(formatter, "session metadata database failure: {error}")
            }
            Self::CommitAmbiguous(error) => {
                write!(
                    formatter,
                    "session metadata commit outcome is ambiguous: {error}"
                )
            }
            Self::DifferentCommandKind { command_id } => write!(
                formatter,
                "durable command {command_id:?} does not name ReplaceSessionMetadata"
            ),
            Self::Corruption(error) => error.fmt(formatter),
        }
    }
}

impl Error for SessionMetadataRepositoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Database(error) | Self::CommitAmbiguous(error) => Some(error),
            Self::DifferentCommandKind { .. } => None,
            Self::Corruption(error) => Some(error),
        }
    }
}

impl From<sqlx::Error> for SessionMetadataRepositoryError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<SessionMetadataCorruption> for SessionMetadataRepositoryError {
    fn from(error: SessionMetadataCorruption) -> Self {
        Self::Corruption(error)
    }
}

/// PostgreSQL implementation of metadata replacement, reads, and list pages.
#[derive(Clone, Debug)]
pub struct SessionMetadataRepository {
    pool: PgPool,
}

impl SessionMetadataRepository {
    /// Uses the supplied pool for independent commands and snapshots.
    pub const fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Claims and handles an unseen command, or resolves its recorded meaning.
    ///
    /// User-global registry inspection is the first durable read. An unseen
    /// command serializes with other metadata writers by locking the session
    /// row before a separate statement samples its timestamp and replaces
    /// satellites.
    pub async fn handle(
        &self,
        command: ReplaceSessionMetadata,
    ) -> Result<ReplaceSessionMetadataHandlingOutcome, SessionMetadataRepositoryError> {
        let command_id = command.command_id();
        let mut transaction = self.pool.begin().await?;

        if let Some(kind) = inspect_registry(&mut transaction, command_id).await? {
            let outcome = existing_or_conflicting(&mut transaction, &command, kind).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let issuer = crate::command_registry::issuer_columns(
            signalbox_domain::CommandPrincipal::for_actor(command.actor()),
        );
        let claimed = sqlx::query(
            "INSERT INTO durable_command
                (command_id, command_kind, storage_version, claimed_at,
                 issuer_kind, issuer_module)
             VALUES ($1, $2, $3, transaction_timestamp(), $4, $5)
             ON CONFLICT DO NOTHING",
        )
        .bind(durable_command_id_to_uuid(command_id))
        .bind(REPLACE_SESSION_METADATA_KIND)
        .bind(STORAGE_VERSION)
        .bind(issuer.0)
        .bind(issuer.1)
        .execute(&mut *transaction)
        .await?
        .rows_affected()
            == 1;

        if !claimed {
            let kind = inspect_registry(&mut transaction, command_id)
                .await?
                .ok_or(SessionMetadataCorruption::Inconsistent(
                    "winner command claim disappeared",
                ))?;
            let outcome = existing_or_conflicting(&mut transaction, &command, kind).await?;
            transaction.rollback().await?;
            return Ok(outcome);
        }

        let session_exists =
            sqlx::query_scalar::<_, Uuid>(lock_inventory::REPLACE_SESSION_METADATA)
                .bind(session_id_to_uuid(command.session()))
                .fetch_optional(&mut *transaction)
                .await?
                .is_some();

        let updated_at = if session_exists {
            Some(replacement_statement_timestamp(&mut transaction).await?)
        } else {
            None
        };
        let prepared = if let Some(updated_at) = updated_at {
            command.prepare_applied(updated_at)
        } else {
            command.prepare_session_not_found()
        };

        if let Some(updated_at) = updated_at {
            replace_current_snapshot(&mut transaction, prepared.command(), updated_at).await?;
        }
        insert_typed_record(&mut transaction, &prepared, updated_at).await?;
        let result = prepared.result().clone();

        match transaction.commit().await {
            Ok(()) => Ok(ReplaceSessionMetadataHandlingOutcome::Recorded(result)),
            Err(error) if commit_failure_is_ambiguous(&error) => {
                Err(SessionMetadataRepositoryError::CommitAmbiguous(error))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Loads one complete durable receipt, or `None` only for an unseen ID.
    pub async fn load_command(
        &self,
        command_id: DurableCommandId,
    ) -> Result<Option<ReconstitutedReplaceSessionMetadata>, SessionMetadataRepositoryError> {
        let mut connection = self.pool.acquire().await?;
        match inspect_registry(&mut connection, command_id).await? {
            None => Ok(None),
            Some(CommandKind::ReplaceSessionMetadata) => {
                load_command_from_connection(&mut connection, command_id).await
            }
            Some(
                CommandKind::CreateSession
                | CommandKind::CreateSessionFromImportedFrontier
                | CommandKind::ReplaceSessionDefaults
                | CommandKind::SubmitInput
                | CommandKind::DecideToolRequest
                | CommandKind::OverrideDeniedToolRequest
                | CommandKind::ReviewWorkflow
                | CommandKind::ReviewOrchestration
                | CommandKind::CompactSession
                | CommandKind::Goal
                | CommandKind::UpdateSessionPlacement
                | CommandKind::RegisterWorkspace
                | CommandKind::MintGitRemote
                | CommandKind::WithdrawGitRemote
                | CommandKind::SessionLifecycle,
            ) => Err(SessionMetadataRepositoryError::DifferentCommandKind { command_id }),
        }
    }

    /// Loads one current complete snapshot, including machine attributes.
    pub async fn load_session_metadata(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionMetadataSnapshot>, SessionMetadataRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let row = sqlx::query(
            "SELECT
                session_row.session_id,
                metadata.session_id AS metadata_session_id,
                metadata.title,
                metadata.archived,
                floor(
                    extract(epoch FROM metadata.updated_at) * 1000000
                )::numeric(20, 0) AS updated_at_unix_micros,
                metadata.actor_kind,
                metadata.actor_turn_id,
                metadata.actor_tool_request_id,
                COALESCE(
                    ARRAY(
                        SELECT tag.tag
                          FROM session_metadata_tag AS tag
                         WHERE tag.session_id = session_row.session_id
                         ORDER BY tag.tag
                    ),
                    ARRAY[]::text[]
                ) AS tags,
                COALESCE(
                    ARRAY(
                        SELECT attribute.attribute_key
                          FROM session_metadata_attribute AS attribute
                         WHERE attribute.session_id = session_row.session_id
                         ORDER BY attribute.attribute_key
                    ),
                    ARRAY[]::text[]
                ) AS attribute_keys,
                COALESCE(
                    ARRAY(
                        SELECT attribute.attribute_value
                          FROM session_metadata_attribute AS attribute
                         WHERE attribute.session_id = session_row.session_id
                         ORDER BY attribute.attribute_key
                    ),
                    ARRAY[]::text[]
                ) AS attribute_values
               FROM session AS session_row
               LEFT JOIN session_metadata AS metadata
                 ON metadata.session_id = session_row.session_id
              WHERE session_row.session_id = $1",
        )
        .bind(session_id_to_uuid(session))
        .fetch_optional(&mut *transaction)
        .await?;
        let snapshot = row
            .map(|row| decode_current_snapshot(&row, session))
            .transpose()?;
        transaction.commit().await?;
        Ok(snapshot)
    }

    /// Opens one bounded repeatable-read metadata list page.
    pub async fn open_page(
        &self,
        query: SessionMetadataListQuery,
    ) -> Result<PostgresSessionMetadataPage, SessionMetadataRepositoryError> {
        let mut transaction = self.pool.begin().await?;
        sqlx::query(REPEATABLE_READ_ONLY)
            .execute(&mut *transaction)
            .await?;
        let next_session_after = query.after_session().map(session_id_to_uuid);
        Ok(PostgresSessionMetadataPage {
            transaction: Some(transaction),
            query,
            next_session_after,
            yielded: 0,
            last_emitted: None,
            continuation: None,
            complete: false,
        })
    }
}

impl ReplaceSessionMetadataTransaction for SessionMetadataRepository {
    type Error = SessionMetadataRepositoryError;

    async fn handle(
        &mut self,
        command: ReplaceSessionMetadata,
    ) -> Result<ReplaceSessionMetadataOutcome, Self::Error> {
        Ok(
            match SessionMetadataRepository::handle(self, command).await? {
                ReplaceSessionMetadataHandlingOutcome::Recorded(result) => {
                    ReplaceSessionMetadataOutcome::Recorded(result)
                }
                ReplaceSessionMetadataHandlingOutcome::ConflictingReuse { command_id } => {
                    ReplaceSessionMetadataOutcome::ConflictingReuse { command_id }
                }
            },
        )
    }
}

impl SessionMetadataReader for SessionMetadataRepository {
    type Error = SessionMetadataRepositoryError;

    async fn load_session_metadata(
        &self,
        session: SessionId,
    ) -> Result<Option<SessionMetadataSnapshot>, Self::Error> {
        SessionMetadataRepository::load_session_metadata(self, session).await
    }
}

impl SessionMetadataLister for SessionMetadataRepository {
    type Error = SessionMetadataRepositoryError;
    type Page = PostgresSessionMetadataPage;

    async fn open_session_metadata_page(
        &self,
        query: SessionMetadataListQuery,
    ) -> Result<Self::Page, Self::Error> {
        self.open_page(query).await
    }
}

/// One bounded metadata page backed by an open repeatable-read transaction.
#[derive(Debug)]
pub struct PostgresSessionMetadataPage {
    transaction: Option<Transaction<'static, Postgres>>,
    query: SessionMetadataListQuery,
    next_session_after: Option<Uuid>,
    yielded: u64,
    last_emitted: Option<SessionId>,
    continuation: Option<SessionId>,
    complete: bool,
}

impl PostgresSessionMetadataPage {
    /// Yields one matching summary in strict session-identity order.
    pub async fn next_item(
        &mut self,
    ) -> Result<Option<SessionMetadataListItem>, SessionMetadataRepositoryError> {
        if self.complete {
            return Ok(None);
        }
        if self.yielded == self.query.page_size() {
            let has_more = self.has_later_match().await?;
            self.continuation = has_more.then_some(self.last_emitted.ok_or(
                SessionMetadataCorruption::Inconsistent(
                    "nonempty full page lacks last emitted session",
                ),
            )?);
            self.finish().await?;
            return Ok(None);
        }

        let required_tags: Vec<String> = self.query.required_tags().map(str::to_owned).collect();
        let title_contains = self.query.title_contains().map(str::to_owned);
        let include_archived = self.query.include_archived();
        let next_session_after = self.next_session_after;
        let transaction = self.transaction_mut()?;
        let row = sqlx::query(
            "SELECT
                session_row.session_id,
                current_defaults.current_version,
                selected_defaults.model_selection_kind,
                selected_defaults.direct_model_selection_id,
                selected_defaults.model_alias_id,
                selected_defaults.dangerous_tool_auto_approval,
                metadata.session_id AS metadata_session_id,
                metadata.title,
                COALESCE(metadata.archived, false) AS archived,
                floor(
                    extract(epoch FROM metadata.updated_at) * 1000000
                )::numeric(20, 0) AS updated_at_unix_micros,
                metadata.actor_kind,
                metadata.actor_turn_id,
                metadata.actor_tool_request_id,
                COALESCE(
                    ARRAY(
                        SELECT tag.tag
                          FROM session_metadata_tag AS tag
                         WHERE tag.session_id = session_row.session_id
                         ORDER BY tag.tag
                    ),
                    ARRAY[]::text[]
                ) AS tags,
                COALESCE(
                    ARRAY(
                        SELECT attribute.attribute_key
                          FROM session_metadata_attribute AS attribute
                         WHERE attribute.session_id = session_row.session_id
                         ORDER BY attribute.attribute_key
                    ),
                    ARRAY[]::text[]
                ) AS attribute_keys,
                COALESCE(
                    ARRAY(
                        SELECT attribute.attribute_value
                          FROM session_metadata_attribute AS attribute
                         WHERE attribute.session_id = session_row.session_id
                         ORDER BY attribute.attribute_key
                    ),
                    ARRAY[]::text[]
                ) AS attribute_values
               FROM session AS session_row
               LEFT JOIN session_current_defaults AS current_defaults
                 ON current_defaults.session_id = session_row.session_id
               LEFT JOIN session_defaults_version AS selected_defaults
                 ON selected_defaults.session_id = current_defaults.session_id
                AND selected_defaults.version = current_defaults.current_version
               LEFT JOIN session_metadata AS metadata
                 ON metadata.session_id = session_row.session_id
              WHERE ($1::uuid IS NULL OR session_row.session_id > $1)
                AND ($2 OR NOT COALESCE(metadata.archived, false))
                AND ($3::text IS NULL OR strpos(metadata.title, $3) > 0)
                AND NOT EXISTS (
                    SELECT 1
                      FROM unnest($4::text[]) AS required(tag)
                     WHERE NOT EXISTS (
                         SELECT 1
                           FROM session_metadata_tag AS stored
                          WHERE stored.session_id = session_row.session_id
                            AND stored.tag = required.tag
                     )
                )
              ORDER BY session_row.session_id
              LIMIT 1",
        )
        .bind(next_session_after)
        .bind(include_archived)
        .bind(title_contains)
        .bind(required_tags)
        .fetch_optional(&mut **transaction)
        .await?;

        let Some(row) = row else {
            self.finish().await?;
            return Ok(None);
        };
        let item = decode_list_item(&row)?;
        self.next_session_after = Some(session_id_to_uuid(item.session()));
        self.last_emitted = Some(item.session());
        self.yielded =
            self.yielded
                .checked_add(1)
                .ok_or(SessionMetadataCorruption::Inconsistent(
                    "bounded metadata page count overflowed",
                ))?;
        Ok(Some(item))
    }

    /// Returns the continuation only after [`Self::next_item`] yielded `None`.
    pub const fn next_after_session(&self) -> Option<SessionId> {
        self.continuation
    }

    async fn has_later_match(&mut self) -> Result<bool, SessionMetadataRepositoryError> {
        let required_tags: Vec<String> = self.query.required_tags().map(str::to_owned).collect();
        let title_contains = self.query.title_contains().map(str::to_owned);
        let include_archived = self.query.include_archived();
        let next_session_after = self.next_session_after;
        let transaction = self.transaction_mut()?;
        sqlx::query_scalar(
            "SELECT EXISTS (
                SELECT 1
                  FROM session AS session_row
                  LEFT JOIN session_metadata AS metadata
                    ON metadata.session_id = session_row.session_id
                 WHERE ($1::uuid IS NULL OR session_row.session_id > $1)
                   AND ($2 OR NOT COALESCE(metadata.archived, false))
                   AND ($3::text IS NULL OR strpos(metadata.title, $3) > 0)
                   AND NOT EXISTS (
                       SELECT 1
                         FROM unnest($4::text[]) AS required(tag)
                        WHERE NOT EXISTS (
                            SELECT 1
                              FROM session_metadata_tag AS stored
                             WHERE stored.session_id = session_row.session_id
                               AND stored.tag = required.tag
                        )
                   )
            )",
        )
        .bind(next_session_after)
        .bind(include_archived)
        .bind(title_contains)
        .bind(required_tags)
        .fetch_one(&mut **transaction)
        .await
        .map_err(Into::into)
    }

    async fn finish(&mut self) -> Result<(), SessionMetadataRepositoryError> {
        let transaction = self
            .transaction
            .take()
            .ok_or(SessionMetadataCorruption::Missing(
                "metadata page transaction",
            ))?;
        transaction.commit().await?;
        self.complete = true;
        Ok(())
    }

    fn transaction_mut(
        &mut self,
    ) -> Result<&mut Transaction<'static, Postgres>, SessionMetadataRepositoryError> {
        self.transaction
            .as_mut()
            .ok_or_else(|| SessionMetadataCorruption::Missing("metadata page transaction").into())
    }
}

impl SessionMetadataPageReader for PostgresSessionMetadataPage {
    type Error = SessionMetadataRepositoryError;

    async fn next_item(&mut self) -> Result<Option<SessionMetadataListItem>, Self::Error> {
        PostgresSessionMetadataPage::next_item(self).await
    }

    fn next_after_session(&self) -> Option<SessionId> {
        PostgresSessionMetadataPage::next_after_session(self)
    }
}

async fn existing_or_conflicting(
    connection: &mut PgConnection,
    command: &ReplaceSessionMetadata,
    kind: CommandKind,
) -> Result<ReplaceSessionMetadataHandlingOutcome, SessionMetadataRepositoryError> {
    match kind {
        CommandKind::ReplaceSessionMetadata => {}
        CommandKind::CreateSession
        | CommandKind::CreateSessionFromImportedFrontier
        | CommandKind::ReplaceSessionDefaults
        | CommandKind::SubmitInput
        | CommandKind::DecideToolRequest
        | CommandKind::OverrideDeniedToolRequest
        | CommandKind::ReviewWorkflow
        | CommandKind::ReviewOrchestration
        | CommandKind::CompactSession
        | CommandKind::Goal
        | CommandKind::UpdateSessionPlacement
        | CommandKind::RegisterWorkspace
        | CommandKind::MintGitRemote
        | CommandKind::WithdrawGitRemote
        | CommandKind::SessionLifecycle => {
            return Ok(ReplaceSessionMetadataHandlingOutcome::ConflictingReuse {
                command_id: command.command_id(),
            });
        }
    }
    let recorded = load_command_from_connection(connection, command.command_id())
        .await?
        .ok_or(SessionMetadataCorruption::Inconsistent(
            "registry entry disappeared",
        ))?;
    Ok(if command == recorded.command() {
        ReplaceSessionMetadataHandlingOutcome::Recorded(recorded.result().clone())
    } else {
        ReplaceSessionMetadataHandlingOutcome::ConflictingReuse {
            command_id: command.command_id(),
        }
    })
}

async fn replacement_statement_timestamp(
    connection: &mut PgConnection,
) -> Result<SessionMetadataUpdatedAt, SessionMetadataRepositoryError> {
    let value: Decimal = sqlx::query_scalar(
        "SELECT floor(
            extract(epoch FROM statement_timestamp()) * 1000000
         )::numeric(20, 0)",
    )
    .fetch_one(&mut *connection)
    .await?;
    decode_updated_at(value)
}

async fn replace_current_snapshot(
    connection: &mut PgConnection,
    command: &ReplaceSessionMetadata,
    updated_at: SessionMetadataUpdatedAt,
) -> Result<(), SessionMetadataRepositoryError> {
    let actor = encode_actor(command.actor());
    sqlx::query(
        "INSERT INTO session_metadata
            (session_id, source_command_id, title, archived, updated_at, actor_kind,
             actor_turn_id, actor_tool_request_id)
         VALUES
            ($1, $2, $3, $4, to_timestamp($5::numeric / 1000000), $6, $7, $8)
         ON CONFLICT (session_id) DO UPDATE
         SET source_command_id = EXCLUDED.source_command_id,
             title = EXCLUDED.title,
             archived = EXCLUDED.archived,
             updated_at = EXCLUDED.updated_at,
             actor_kind = EXCLUDED.actor_kind,
             actor_turn_id = EXCLUDED.actor_turn_id,
             actor_tool_request_id = EXCLUDED.actor_tool_request_id",
    )
    .bind(session_id_to_uuid(command.session()))
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(command.replacement().title())
    .bind(command.replacement().archived())
    .bind(Decimal::from(updated_at.as_unix_micros()))
    .bind(actor.kind)
    .bind(actor.turn)
    .bind(actor.tool_request)
    .execute(&mut *connection)
    .await?;

    sqlx::query("DELETE FROM session_metadata_tag WHERE session_id = $1")
        .bind(session_id_to_uuid(command.session()))
        .execute(&mut *connection)
        .await?;
    let tags: Vec<String> = command.replacement().tags().map(str::to_owned).collect();
    sqlx::query(
        "INSERT INTO session_metadata_tag (session_id, tag)
         SELECT $1, supplied.tag
           FROM UNNEST($2::text[]) AS supplied(tag)",
    )
    .bind(session_id_to_uuid(command.session()))
    .bind(tags)
    .execute(&mut *connection)
    .await?;

    sqlx::query("DELETE FROM session_metadata_attribute WHERE session_id = $1")
        .bind(session_id_to_uuid(command.session()))
        .execute(&mut *connection)
        .await?;
    let (attribute_keys, attribute_values): (Vec<String>, Vec<String>) = command
        .replacement()
        .attributes()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .unzip();
    sqlx::query(
        "INSERT INTO session_metadata_attribute
            (session_id, attribute_key, attribute_value)
         SELECT $1, supplied.attribute_key, supplied.attribute_value
           FROM UNNEST($2::text[], $3::text[])
             AS supplied(attribute_key, attribute_value)",
    )
    .bind(session_id_to_uuid(command.session()))
    .bind(attribute_keys)
    .bind(attribute_values)
    .execute(&mut *connection)
    .await?;
    Ok(())
}

async fn insert_typed_record(
    connection: &mut PgConnection,
    prepared: &signalbox_domain::PreparedReplaceSessionMetadata,
    updated_at: Option<SessionMetadataUpdatedAt>,
) -> Result<(), SessionMetadataRepositoryError> {
    let command = prepared.command();
    let actor = encode_actor(command.actor());
    let applied = matches!(prepared.result(), ReplaceSessionMetadataResult::Applied(_));
    let (result_kind, rejection_kind) = if applied {
        (APPLIED, None)
    } else {
        (REJECTED, Some(SESSION_NOT_FOUND))
    };
    // Deferred child foreign keys allow the satellites to precede the parent;
    // inserting the immutable parent last seals the complete receipt.
    let tags: Vec<String> = command.replacement().tags().map(str::to_owned).collect();
    sqlx::query(
        "INSERT INTO replace_session_metadata_command_tag (command_id, tag)
         SELECT $1, supplied.tag
           FROM UNNEST($2::text[]) AS supplied(tag)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(tags)
    .execute(&mut *connection)
    .await?;
    let (attribute_keys, attribute_values): (Vec<String>, Vec<String>) = command
        .replacement()
        .attributes()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .unzip();
    sqlx::query(
        "INSERT INTO replace_session_metadata_command_attribute
            (command_id, attribute_key, attribute_value)
         SELECT $1, supplied.attribute_key, supplied.attribute_value
           FROM UNNEST($2::text[], $3::text[])
             AS supplied(attribute_key, attribute_value)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(attribute_keys)
    .bind(attribute_values)
    .execute(&mut *connection)
    .await?;

    sqlx::query(
        "INSERT INTO replace_session_metadata_command
            (command_id, command_kind, storage_version, session_id,
             actor_kind, actor_turn_id, actor_tool_request_id,
             replacement_title, replacement_archived,
             result_kind, rejection_kind, result_session_id,
             result_applied_session_id, result_updated_at, result_actor_kind,
             result_actor_turn_id, result_actor_tool_request_id,
             issuer_kind, issuer_tool_request_id)
         VALUES
            ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
             CASE WHEN $13::numeric IS NOT NULL THEN $4 ELSE NULL END,
             CASE
                 WHEN $13::numeric IS NOT NULL
                 THEN to_timestamp($13::numeric / 1000000)
                 ELSE NULL
             END,
             CASE WHEN $13::numeric IS NOT NULL THEN $5 ELSE NULL END,
             CASE WHEN $13::numeric IS NOT NULL THEN $6 ELSE NULL END,
             CASE WHEN $13::numeric IS NOT NULL THEN $7 ELSE NULL END, $14, $15)",
    )
    .bind(durable_command_id_to_uuid(command.command_id()))
    .bind(REPLACE_SESSION_METADATA_KIND)
    .bind(STORAGE_VERSION)
    .bind(session_id_to_uuid(command.session()))
    .bind(actor.kind)
    .bind(actor.turn)
    .bind(actor.tool_request)
    .bind(command.replacement().title())
    .bind(command.replacement().archived())
    .bind(result_kind)
    .bind(rejection_kind)
    .bind(session_id_to_uuid(command.session()))
    .bind(updated_at.map(|value| Decimal::from(value.as_unix_micros())))
    .bind(actor.kind)
    .bind(actor.tool_request)
    .execute(&mut *connection)
    .await?;

    Ok(())
}

async fn load_command_from_connection(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<ReconstitutedReplaceSessionMetadata>, SessionMetadataRepositoryError> {
    let row = sqlx::query(
        "SELECT
            registry.command_kind AS registry_kind,
            registry.storage_version AS registry_version,
            typed.command_id AS typed_command_id,
            typed.command_kind AS typed_kind,
            typed.storage_version AS typed_version,
            typed.session_id,
            typed.actor_kind,
            typed.actor_turn_id,
            typed.actor_tool_request_id,
            typed.issuer_kind,
            typed.issuer_tool_request_id,
            typed.replacement_title,
            typed.replacement_archived,
            typed.result_kind,
            typed.rejection_kind,
            typed.result_session_id,
            typed.result_applied_session_id,
            floor(
                extract(epoch FROM typed.result_updated_at) * 1000000
            )::numeric(20, 0) AS result_updated_at_unix_micros,
            typed.result_actor_kind,
            typed.result_actor_turn_id,
            typed.result_actor_tool_request_id,
            COALESCE(
                ARRAY(
                    SELECT tag.tag
                      FROM replace_session_metadata_command_tag AS tag
                     WHERE tag.command_id = registry.command_id
                     ORDER BY tag.tag
                ),
                ARRAY[]::text[]
            ) AS replacement_tags,
            COALESCE(
                ARRAY(
                    SELECT attribute.attribute_key
                      FROM replace_session_metadata_command_attribute AS attribute
                     WHERE attribute.command_id = registry.command_id
                     ORDER BY attribute.attribute_key
                ),
                ARRAY[]::text[]
            ) AS replacement_attribute_keys,
            COALESCE(
                ARRAY(
                    SELECT attribute.attribute_value
                      FROM replace_session_metadata_command_attribute AS attribute
                     WHERE attribute.command_id = registry.command_id
                     ORDER BY attribute.attribute_key
                ),
                ARRAY[]::text[]
            ) AS replacement_attribute_values
           FROM durable_command AS registry
           LEFT JOIN replace_session_metadata_command AS typed
             ON typed.command_id = registry.command_id
          WHERE registry.command_id = $1",
    )
    .bind(durable_command_id_to_uuid(command_id))
    .fetch_optional(&mut *connection)
    .await?;
    row.map(|row| decode_command(&row, command_id)).transpose()
}

fn decode_command(
    row: &PgRow,
    command_id: DurableCommandId,
) -> Result<ReconstitutedReplaceSessionMetadata, SessionMetadataRepositoryError> {
    require_spelling(row, "registry_kind", REPLACE_SESSION_METADATA_KIND)?;
    require_supported_version(row, "registry_version")?;
    let _: Uuid = required(row, "typed_command_id")?;
    require_spelling(row, "typed_kind", REPLACE_SESSION_METADATA_KIND)?;
    require_supported_version(row, "typed_version")?;

    let session = session_id_from_uuid(required(row, "session_id")?);
    let issuer_kind: String = required(row, "issuer_kind")?;
    let issuer_tool: Option<Uuid> = row.try_get("issuer_tool_request_id")?;
    let content = decode_content(
        row.try_get("replacement_title")?,
        required(row, "replacement_tags")?,
        required(row, "replacement_attribute_keys")?,
        required(row, "replacement_attribute_values")?,
        required(row, "replacement_archived")?,
    )?;
    let command = match (issuer_kind.as_str(), issuer_tool) {
        ("user", None) => ReplaceSessionMetadata::new(command_id, session, content),
        ("tool", Some(request)) => ReplaceSessionMetadata::new_for_tool(
            command_id,
            session,
            tool_request_id_from_uuid(request),
            content,
        ),
        ("user", Some(_)) | ("tool", None) => {
            return Err(SessionMetadataCorruption::Inconsistent("command issuer shape").into());
        }
        (other, _) => {
            return Err(SessionMetadataCorruption::Unsupported {
                field: "command issuer",
                value: other.to_owned(),
            }
            .into());
        }
    };
    let actor_kind: String = required(row, "actor_kind")?;
    let command_actor = decode_actor(
        actor_kind.clone(),
        row.try_get("actor_turn_id")?,
        row.try_get("actor_tool_request_id")?,
        "command actor",
    )?;
    match command_actor {
        Actor::User | Actor::Tool { .. } => {}
        Actor::Core | Actor::Model { .. } | Actor::Recovery => {
            return Err(SessionMetadataCorruption::Unsupported {
                field: "command actor",
                value: actor_kind,
            }
            .into());
        }
    }
    let result_kind: String = required(row, "result_kind")?;
    let rejection_kind: Option<String> = row.try_get("rejection_kind")?;
    let result_session = session_id_from_uuid(required(row, "result_session_id")?);
    let result_applied_session: Option<Uuid> = row.try_get("result_applied_session_id")?;
    let result_updated_at: Option<Decimal> = row.try_get("result_updated_at_unix_micros")?;
    let result_actor_kind: Option<String> = row.try_get("result_actor_kind")?;
    let result_actor_turn: Option<Uuid> = row.try_get("result_actor_turn_id")?;
    let result_actor_tool: Option<Uuid> = row.try_get("result_actor_tool_request_id")?;

    let input = match (result_kind.as_str(), rejection_kind.as_deref()) {
        (APPLIED, None) => {
            let applied_session = session_id_from_uuid(result_applied_session.ok_or(
                SessionMetadataCorruption::Missing("applied result session proof"),
            )?);
            if applied_session != session || applied_session != result_session {
                return Err(SessionMetadataCorruption::Inconsistent(
                    "applied result session proof",
                )
                .into());
            }
            let updated_at = decode_updated_at(result_updated_at.ok_or(
                SessionMetadataCorruption::Missing("result updated timestamp"),
            )?)?;
            let result_actor = decode_actor(
                result_actor_kind.ok_or(SessionMetadataCorruption::Missing("result actor kind"))?,
                result_actor_turn,
                result_actor_tool,
                "result actor",
            )?;
            ReplaceSessionMetadataReconstitutionInput::applied(
                command,
                command_actor,
                result_session,
                updated_at,
                result_actor,
            )
        }
        (REJECTED, Some(SESSION_NOT_FOUND)) => {
            if result_updated_at.is_some()
                || result_applied_session.is_some()
                || result_actor_kind.is_some()
                || result_actor_turn.is_some()
                || result_actor_tool.is_some()
            {
                return Err(
                    SessionMetadataCorruption::Inconsistent("rejected result fields").into(),
                );
            }
            ReplaceSessionMetadataReconstitutionInput::rejected_session_not_found(
                command,
                command_actor,
                result_session,
            )
        }
        (REJECTED, Some(other)) => {
            return Err(SessionMetadataCorruption::Unsupported {
                field: "rejection_kind",
                value: other.to_owned(),
            }
            .into());
        }
        (other, _) => {
            return Err(SessionMetadataCorruption::Unsupported {
                field: "result_kind",
                value: other.to_owned(),
            }
            .into());
        }
    };
    input
        .reconstitute()
        .map_err(|error| SessionMetadataCorruption::Domain(error.failure()).into())
}

fn decode_current_snapshot(
    row: &PgRow,
    requested_session: SessionId,
) -> Result<SessionMetadataSnapshot, SessionMetadataRepositoryError> {
    let stored_session = session_id_from_uuid(required(row, "session_id")?);
    if stored_session != requested_session {
        return Err(SessionMetadataCorruption::Inconsistent("requested session identity").into());
    }
    let metadata_session: Option<Uuid> = row.try_get("metadata_session_id")?;
    let tags: Vec<String> = required(row, "tags")?;
    let keys: Vec<String> = required(row, "attribute_keys")?;
    let values: Vec<String> = required(row, "attribute_values")?;
    let Some(metadata_session) = metadata_session else {
        if !tags.is_empty() || !keys.is_empty() || !values.is_empty() {
            return Err(SessionMetadataCorruption::Inconsistent(
                "metadata satellites without root",
            )
            .into());
        }
        return Ok(SessionMetadataSnapshot::initial(requested_session));
    };
    if session_id_from_uuid(metadata_session) != requested_session {
        return Err(SessionMetadataCorruption::Inconsistent("metadata root identity").into());
    }
    let content = decode_content(
        row.try_get("title")?,
        tags,
        keys,
        values,
        required(row, "archived")?,
    )?;
    let writer = decode_writer(row)?;
    Ok(SessionMetadataSnapshot::from_recorded_write(
        requested_session,
        content,
        writer,
    ))
}

fn decode_list_item(
    row: &PgRow,
) -> Result<SessionMetadataListItem, SessionMetadataRepositoryError> {
    let session = session_id_from_uuid(required(row, "session_id")?);
    let version = defaults_version_from_numeric(required(row, "current_version")?)
        .map_err(SessionMetadataCorruption::InvalidDefaultsVersion)?;
    let (model_selection, dangerous_tool_auto_approval) = decode_defaults(
        required(row, "model_selection_kind")?,
        row.try_get("direct_model_selection_id")?,
        row.try_get("model_alias_id")?,
        required(row, "dangerous_tool_auto_approval")?,
    )?;
    let metadata_session: Option<Uuid> = row.try_get("metadata_session_id")?;
    let tags: Vec<String> = required(row, "tags")?;
    let attribute_keys: Vec<String> = required(row, "attribute_keys")?;
    let attribute_values: Vec<String> = required(row, "attribute_values")?;
    let snapshot = if let Some(metadata_session) = metadata_session {
        if session_id_from_uuid(metadata_session) != session {
            return Err(SessionMetadataCorruption::Inconsistent("list metadata identity").into());
        }
        let content = decode_content(
            row.try_get("title")?,
            tags,
            attribute_keys,
            attribute_values,
            required(row, "archived")?,
        )?;
        SessionMetadataSnapshot::from_recorded_write(session, content, decode_writer(row)?)
    } else {
        if !tags.is_empty() || !attribute_keys.is_empty() || !attribute_values.is_empty() {
            return Err(SessionMetadataCorruption::Inconsistent(
                "list metadata satellites without root",
            )
            .into());
        }
        SessionMetadataSnapshot::initial(session)
    };
    Ok(SessionMetadataListItem::new(
        &snapshot,
        version,
        model_selection,
        dangerous_tool_auto_approval,
    ))
}

fn decode_content(
    title: Option<String>,
    tags: Vec<String>,
    attribute_keys: Vec<String>,
    attribute_values: Vec<String>,
    archived: bool,
) -> Result<SessionMetadataContent, SessionMetadataRepositoryError> {
    if attribute_keys.len() != attribute_values.len() {
        return Err(
            SessionMetadataCorruption::Inconsistent("attribute key/value cardinality").into(),
        );
    }
    SessionMetadataContent::try_new(
        title,
        tags,
        attribute_keys.into_iter().zip(attribute_values).collect(),
        archived,
    )
    .map_err(|error| SessionMetadataCorruption::InvalidContent(error).into())
}

fn decode_writer(row: &PgRow) -> Result<SessionMetadataLastWriter, SessionMetadataRepositoryError> {
    let updated_at = decode_updated_at(required(row, "updated_at_unix_micros")?)?;
    let actor = decode_actor(
        required(row, "actor_kind")?,
        row.try_get("actor_turn_id")?,
        row.try_get("actor_tool_request_id")?,
        "last writer actor",
    )?;
    Ok(SessionMetadataLastWriter::new(updated_at, actor))
}

/// Decodes the prompt-free list projection of the current defaults: exactly
/// the selection and posture the wire summary states, never the epoch's
/// optional system prompt (docs/spec/sessions-and-transcript.md).
fn decode_defaults(
    kind: String,
    direct: Option<Uuid>,
    alias: Option<Uuid>,
    tool_approval: String,
) -> Result<
    (
        ModelSelectionRequest,
        signalbox_domain::DangerousToolAutoApproval,
    ),
    SessionMetadataRepositoryError,
> {
    let model = match (kind.as_str(), direct, alias) {
        ("direct", Some(value), None) => {
            ModelSelectionRequest::Direct(DirectModelSelection::from_uuid(value))
        }
        ("alias", None, Some(value)) => ModelSelectionRequest::Alias(ModelAlias::from_uuid(value)),
        ("direct" | "alias", _, _) => {
            return Err(SessionMetadataCorruption::Inconsistent(
                "metadata-list model selection fields",
            )
            .into());
        }
        _ => {
            return Err(SessionMetadataCorruption::Unsupported {
                field: "model_selection_kind",
                value: kind,
            }
            .into());
        }
    };
    let dangerous_tool_auto_approval = dangerous_tool_auto_approval_from_str(&tool_approval)
        .ok_or(SessionMetadataCorruption::Unsupported {
            field: "dangerous_tool_auto_approval",
            value: tool_approval,
        })?;
    Ok((model, dangerous_tool_auto_approval))
}

struct EncodedActor {
    kind: &'static str,
    turn: Option<Uuid>,
    tool_request: Option<Uuid>,
}

fn encode_actor(actor: Actor) -> EncodedActor {
    match actor {
        Actor::User => EncodedActor {
            kind: "user",
            turn: None,
            tool_request: None,
        },
        Actor::Core => EncodedActor {
            kind: "core",
            turn: None,
            tool_request: None,
        },
        Actor::Model { turn } => EncodedActor {
            kind: "model",
            turn: Some(turn.into_uuid()),
            tool_request: None,
        },
        Actor::Recovery => EncodedActor {
            kind: "recovery",
            turn: None,
            tool_request: None,
        },
        Actor::Tool { request } => EncodedActor {
            kind: "tool",
            turn: None,
            tool_request: Some(request.into_uuid()),
        },
    }
}

fn decode_actor(
    kind: String,
    turn: Option<Uuid>,
    tool_request: Option<Uuid>,
    relationship: &'static str,
) -> Result<Actor, SessionMetadataRepositoryError> {
    match (kind.as_str(), turn, tool_request) {
        ("user", None, None) => Ok(Actor::User),
        ("core", None, None) => Ok(Actor::Core),
        ("model", Some(turn), None) => Ok(Actor::Model {
            turn: TurnId::from_uuid(turn),
        }),
        ("recovery", None, None) => Ok(Actor::Recovery),
        ("tool", None, Some(request)) => Ok(Actor::Tool {
            request: ToolRequestId::from_uuid(request),
        }),
        ("user" | "model" | "recovery" | "tool", _, _) => {
            Err(SessionMetadataCorruption::Inconsistent(relationship).into())
        }
        _ => Err(SessionMetadataCorruption::Unsupported {
            field: relationship,
            value: kind,
        }
        .into()),
    }
}

fn decode_updated_at(
    value: Decimal,
) -> Result<SessionMetadataUpdatedAt, SessionMetadataRepositoryError> {
    if !value.fract().is_zero() || value < Decimal::ZERO {
        return Err(SessionMetadataCorruption::InvalidUpdatedAt.into());
    }
    let micros = u64::try_from(value).map_err(|_| SessionMetadataCorruption::InvalidUpdatedAt)?;
    Ok(SessionMetadataUpdatedAt::from_unix_micros(micros))
}

fn required<T>(row: &PgRow, field: &'static str) -> Result<T, SessionMetadataRepositoryError>
where
    for<'decode> T: sqlx::Decode<'decode, Postgres> + sqlx::Type<Postgres>,
{
    row.try_get::<Option<T>, _>(field)?
        .ok_or_else(|| SessionMetadataCorruption::Missing(field).into())
}

fn require_spelling(
    row: &PgRow,
    field: &'static str,
    expected: &'static str,
) -> Result<(), SessionMetadataRepositoryError> {
    let value: String = required(row, field)?;
    if value == expected {
        Ok(())
    } else {
        Err(SessionMetadataCorruption::Unsupported { field, value }.into())
    }
}

fn require_supported_version(
    row: &PgRow,
    field: &'static str,
) -> Result<(), SessionMetadataRepositoryError> {
    let value: i16 = required(row, field)?;
    if value == STORAGE_VERSION {
        Ok(())
    } else {
        Err(SessionMetadataCorruption::Unsupported {
            field,
            value: value.to_string(),
        }
        .into())
    }
}

async fn inspect_registry(
    connection: &mut PgConnection,
    command_id: DurableCommandId,
) -> Result<Option<CommandKind>, SessionMetadataRepositoryError> {
    command_registry::inspect(connection, command_id)
        .await
        .map_err(map_registry_error)
}

fn map_registry_error(error: RegistryInspectionError) -> SessionMetadataRepositoryError {
    match error {
        RegistryInspectionError::Database(error) => error.into(),
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedKind(value)) => {
            SessionMetadataCorruption::Unsupported {
                field: "registry_kind",
                value,
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::UnsupportedVersion(value)) => {
            SessionMetadataCorruption::Unsupported {
                field: "registry_version",
                value: value.to_string(),
            }
            .into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::MissingTypedRecord(_)) => {
            SessionMetadataCorruption::Missing("typed_command_id").into()
        }
        RegistryInspectionError::Corruption(RegistryCorruption::ConflictingTypedRecords) => {
            SessionMetadataCorruption::Inconsistent("typed command family").into()
        }
    }
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use signalbox_domain::{Actor, ToolRequestId, TurnId};
    use sqlx::types::Uuid;

    use super::{
        SessionMetadataCorruption, SessionMetadataRepositoryError, decode_actor, decode_updated_at,
        encode_actor,
    };

    #[track_caller]
    fn assert_actor_storage_round_trip(actor: Actor) {
        let encoded = encode_actor(actor);
        let decoded = decode_actor(
            encoded.kind.to_owned(),
            encoded.turn,
            encoded.tool_request,
            "fixture actor",
        )
        .expect("encoded actor is complete");
        assert_eq!(decoded, actor);
    }

    #[test]
    fn actor_storage_round_trips_every_variant() {
        assert_actor_storage_round_trip(Actor::User);
        assert_actor_storage_round_trip(Actor::Core);
        assert_actor_storage_round_trip(Actor::Model {
            turn: TurnId::from_uuid(Uuid::from_u128(1)),
        });
        assert_actor_storage_round_trip(Actor::Recovery);
        assert_actor_storage_round_trip(Actor::Tool {
            request: ToolRequestId::from_uuid(Uuid::from_u128(2)),
        });
    }

    #[test]
    fn invalid_actor_shapes_fail_closed() {
        let error = decode_actor(
            encode_actor(Actor::User).kind.to_owned(),
            Some(Uuid::from_u128(1)),
            None,
            "fixture actor",
        )
        .expect_err("legacy user encoding cannot carry a turn");

        assert!(matches!(
            error,
            SessionMetadataRepositoryError::Corruption(SessionMetadataCorruption::Inconsistent(
                "fixture actor"
            ))
        ));
    }

    #[test]
    fn timestamp_mapping_admits_zero_and_u64_max() {
        assert_eq!(
            decode_updated_at(Decimal::ZERO)
                .expect("epoch is a valid timestamp")
                .as_unix_micros(),
            0
        );
        assert_eq!(
            decode_updated_at(Decimal::from(u64::MAX))
                .expect("maximum domain timestamp remains representable")
                .as_unix_micros(),
            u64::MAX
        );
    }

    #[track_caller]
    fn assert_updated_at_is_rejected(value: Decimal) {
        assert!(matches!(
            decode_updated_at(value),
            Err(SessionMetadataRepositoryError::Corruption(
                SessionMetadataCorruption::InvalidUpdatedAt
            ))
        ));
    }

    #[test]
    fn timestamp_mapping_rejects_negative_fractional_and_overflow() {
        assert_updated_at_is_rejected(Decimal::NEGATIVE_ONE);
        assert_updated_at_is_rejected(Decimal::new(1, 1));
        assert_updated_at_is_rejected(Decimal::from(u64::MAX) + Decimal::ONE);
    }
}
