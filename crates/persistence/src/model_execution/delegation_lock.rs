use super::live_turn::lock_session;
use super::{ModelCallCorruption, ModelCallRepositoryError};
use crate::mapping::{
    session_id_from_uuid, session_id_to_uuid, turn_id_from_uuid, turn_id_to_uuid,
};
use signalbox_domain::{ModelCallId, SessionId, TurnId};
use sqlx::postgres::PgRow;
use sqlx::types::Uuid;
use sqlx::{PgConnection, Row};

/// Locks the terminal-observation frontier, then reports whether a cascade
/// already delivered this delegated turn's logical terminal.
///
/// An ordinary call retains the model-execution scheduler lock. A delegated
/// call instead shares peer-message ordering: canonical endpoint sessions,
/// canonical endpoint schedulers, then the relationship. Besides making the
/// logical-terminal read authoritative, this prevents a message transaction
/// holding the parent session from waiting on a child scheduler held by an
/// observation that is itself waiting for that parent session.
pub(super) async fn locked_delegation_logical_terminal(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<bool, ModelCallRepositoryError> {
    if lock_model_call_terminal_frontier(connection, session, call)
        .await?
        .is_none()
    {
        return Ok(false);
    }
    model_call_is_delegation_logically_terminal(connection, session, call).await
}

pub(super) async fn lock_model_call_terminal_frontier(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<Option<TurnId>, ModelCallRepositoryError> {
    let turn: Option<Uuid> = sqlx::query_scalar(
        "SELECT turn_id
           FROM model_call
          WHERE session_id = $1
            AND model_call_id = $2",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .fetch_optional(&mut *connection)
    .await?;
    let Some(turn) = turn else {
        lock_session(connection, session).await?;
        return Ok(None);
    };
    let turn = turn_id_from_uuid(turn);
    lock_delegated_turn_terminal_frontier(connection, session, turn).await?;
    Ok(Some(turn))
}

pub(crate) async fn lock_delegated_turn_terminal_frontier(
    connection: &mut PgConnection,
    session: SessionId,
    turn: TurnId,
) -> Result<(), ModelCallRepositoryError> {
    let relation = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION_IDENTITY,
        session_id_to_uuid(session),
        turn_id_to_uuid(turn),
    )
    .await?;
    let Some(relation) = relation else {
        lock_session(connection, session).await?;
        return Ok(());
    };
    let parent = session_id_from_uuid(relation.parent_session_id);
    let (first, second) = crate::lock_inventory::ordered_session_pair(session, parent);
    lock_delegation_terminal_session(connection, first).await?;
    if second != first {
        lock_delegation_terminal_session(connection, second).await?;
    }
    lock_session(connection, first).await?;
    if second != first {
        lock_session(connection, second).await?;
    }
    let locked = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION,
        session_id_to_uuid(session),
        turn_id_to_uuid(turn),
    )
    .await?;
    if locked != Some(relation) {
        return Err(ModelCallCorruption::Inconsistent(
            "delegated terminal relationship changed while locking",
        )
        .into());
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct DelegationTerminalRelationRow {
    pub(super) spawning_tool_request_id: Uuid,
    pub(super) parent_session_id: Uuid,
}

pub(super) async fn load_delegation_terminal_relation(
    connection: &mut PgConnection,
    statement: &'static str,
    child: Uuid,
    turn: Uuid,
) -> Result<Option<DelegationTerminalRelationRow>, ModelCallRepositoryError> {
    sqlx::query(statement)
        .bind(child)
        .bind(turn)
        .fetch_optional(connection)
        .await?
        .map(|row| {
            Ok(DelegationTerminalRelationRow {
                spawning_tool_request_id: delegation_terminal_relation_uuid(
                    &row,
                    "spawning_tool_request_id",
                )?,
                parent_session_id: delegation_terminal_relation_uuid(&row, "parent_session_id")?,
            })
        })
        .transpose()
}

fn delegation_terminal_relation_uuid(
    row: &PgRow,
    column: &'static str,
) -> Result<Uuid, ModelCallRepositoryError> {
    match row.try_get(column) {
        Ok(value) => Ok(value),
        Err(error @ (sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_))) => {
            Err(delegation_terminal_relation_decode_error(error))
        }
        Err(error) => Err(error.into()),
    }
}

pub(super) fn delegation_terminal_relation_decode_error(
    error: sqlx::Error,
) -> ModelCallRepositoryError {
    debug_assert!(matches!(
        error,
        sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_)
    ));
    ModelCallCorruption::Inconsistent("delegated terminal relationship identity").into()
}

async fn lock_delegation_terminal_session(
    connection: &mut PgConnection,
    session: SessionId,
) -> Result<(), ModelCallRepositoryError> {
    let locked =
        sqlx::query_scalar::<_, Uuid>(crate::lock_inventory::DELEGATION_TERMINAL_ENDPOINT_SESSION)
            .bind(session_id_to_uuid(session))
            .fetch_optional(connection)
            .await?;
    if locked.is_some_and(|locked| session_id_from_uuid(locked) == session) {
        Ok(())
    } else {
        Err(ModelCallCorruption::Missing("delegated terminal endpoint session").into())
    }
}

async fn model_call_is_delegation_logically_terminal(
    connection: &mut PgConnection,
    session: SessionId,
    call: ModelCallId,
) -> Result<bool, ModelCallRepositoryError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (
            SELECT 1
              FROM model_call AS call
              JOIN session_delegation_initial_task AS task
                ON task.child_session_id = call.session_id
               AND task.turn_id = call.turn_id
              JOIN session_delegation_logical_terminal AS terminal
                ON terminal.spawning_tool_request_id = task.spawning_tool_request_id
               AND terminal.child_session_id = task.child_session_id
               AND terminal.child_turn_id = task.turn_id
             WHERE call.session_id = $1
               AND call.model_call_id = $2
        )",
    )
    .bind(session_id_to_uuid(session))
    .bind(call.into_uuid())
    .fetch_one(&mut *connection)
    .await?)
}

pub(super) async fn lock_delegated_child_result_frontier(
    connection: &mut PgConnection,
    child: SessionId,
    turn: TurnId,
) -> Result<(), ModelCallRepositoryError> {
    let relation = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION_IDENTITY,
        session_id_to_uuid(child),
        turn_id_to_uuid(turn),
    )
    .await?;
    let Some(relation) = relation else {
        return Ok(());
    };
    sqlx::query(crate::lock_inventory::DELEGATION_TERMINAL_ENDPOINT_SESSION)
        .bind(relation.parent_session_id)
        .execute(&mut *connection)
        .await?;
    let locked = load_delegation_terminal_relation(
        connection,
        crate::lock_inventory::DELEGATION_TERMINAL_RELATION,
        session_id_to_uuid(child),
        turn_id_to_uuid(turn),
    )
    .await?;
    if locked != Some(relation) {
        return Err(ModelCallCorruption::Inconsistent(
            "delegated terminal relationship changed while locking",
        )
        .into());
    }
    Ok(())
}

/// Locks the immutable parent/child endpoint pair before a child scheduler can
/// be locked by a transaction that may terminalize the delegated child.
pub(crate) async fn lock_delegated_child_endpoint_sessions(
    connection: &mut PgConnection,
    child: SessionId,
) -> Result<(), ModelCallRepositoryError> {
    let parent: Option<Uuid> = sqlx::query_scalar(
        "SELECT parent_session_id
           FROM session_delegation
          WHERE child_session_id = $1",
    )
    .bind(session_id_to_uuid(child))
    .fetch_optional(&mut *connection)
    .await?;
    let Some(parent) = parent else {
        return Ok(());
    };
    let parent = session_id_from_uuid(parent);
    let (first, second) = crate::lock_inventory::ordered_session_pair(child, parent);
    sqlx::query(crate::lock_inventory::DELEGATION_TERMINAL_ENDPOINT_SESSION)
        .bind(session_id_to_uuid(first))
        .execute(&mut *connection)
        .await?;
    if second != first {
        sqlx::query(crate::lock_inventory::DELEGATION_TERMINAL_ENDPOINT_SESSION)
            .bind(session_id_to_uuid(second))
            .execute(&mut *connection)
            .await?;
    }
    Ok(())
}
