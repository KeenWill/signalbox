//! Coalesces review-response check facts for a provisioned pull-request head.

use crate::{PlannedCommand, StoreError};
use rust_decimal::Decimal;
use signalbox_session_ownership::RepoWatchEventTarget;
use sqlx::{Postgres, Transaction};

pub(crate) async fn head_was_dispatched(
    transaction: &mut Transaction<'_, Postgres>,
    command: &PlannedCommand,
) -> Result<bool, StoreError> {
    if command.rule_id().as_str() != "labeled-review-response" {
        return Ok(false);
    }
    let payload: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT normalized_payload FROM gh_readable_event
         WHERE event_id=$1 AND event_kind IN ('checks_completed', 'check_run_completed')",
    )
    .bind(command.event_id().into_uuid())
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(payload) = payload else {
        return Ok(false);
    };
    let event = crate::event_decode::event(command.event_id(), &payload)
        .ok_or(StoreError::InvalidRetainedEvent)?;
    let RepoWatchEventTarget::PullRequest(context) = event.target() else {
        return Ok(false);
    };
    sqlx::query_scalar(
        "SELECT EXISTS (
           SELECT 1 FROM dispatch_ledger AS prior JOIN gh_event AS event USING(event_id)
           WHERE prior.repository=$1 AND prior.rule_id=$2 AND prior.rule_revision=$3
             AND prior.command_kind='create_session' AND event.pull_request_number=$4
             AND prior.checkout_head_sha=$5)",
    )
    .bind(command.repository().as_str())
    .bind(command.rule_id().as_str())
    .bind(Decimal::from(command.rule_revision().get()))
    .bind(Decimal::from(context.number().get()))
    .bind(context.head_sha().as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(StoreError::from)
}
