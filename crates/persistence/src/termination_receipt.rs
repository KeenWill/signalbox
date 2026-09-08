//! Recorded cascade metadata for process-protocol stop receipts.

use signalbox_domain::{DescendantTerminationScope, DurableCommandId, SessionId};
use sqlx::{PgPool, Row};

/// The immutable choice and recorded descendant dispositions of an applied stop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminationReceipt {
    pub descendant_scope: DescendantTerminationScope,
    pub descendant_count: u64,
}

#[derive(Debug, signalbox_derive::OperatorError)]
pub enum TerminationReceiptError {
    #[error("termination receipt database failure: {field_0}")]
    Database(#[source] sqlx::Error),
    #[error("termination receipt is inconsistent")]
    Corruption,
}

impl From<sqlx::Error> for TerminationReceiptError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

/// Reads applied stop facts without traversing or evaluating descendants.
pub async fn load_termination_receipt(
    pool: &PgPool,
    command: DurableCommandId,
    session: SessionId,
) -> Result<TerminationReceipt, TerminationReceiptError> {
    let row = sqlx::query(
        "WITH applied AS (
            SELECT session_id, descendant_scope, 'goal_command' AS source_kind
              FROM goal_command
             WHERE command_id = $1 AND operation_kind = 'stop' AND result_kind = 'applied'
            UNION ALL
            SELECT session_id, descendant_scope, 'turn_command' AS source_kind
              FROM submit_input_command
             WHERE command_id = $1 AND delivery_kind = 'interrupt' AND result_kind = 'applied'
         )
         SELECT applied.session_id, applied.descendant_scope,
                cascade.disposition_count::text AS descendant_count,
                cascade.root_session_id, cascade.root_source_kind,
                cascade.descendant_scope AS cascade_scope
           FROM applied
           LEFT JOIN session_delegation_termination_cascade AS cascade
             ON cascade.root_command_id = $1
          WHERE applied.session_id = $2
            AND (cascade.root_source_kind IS NULL OR cascade.root_source_kind = applied.source_kind)",
    )
    .bind(command.into_uuid())
    .bind(session.into_uuid())
    .fetch_all(pool)
    .await?;
    let [row] = row.as_slice() else {
        return Err(TerminationReceiptError::Corruption);
    };
    let scope: String = row.try_get("descendant_scope")?;
    let count: Option<String> = row.try_get("descendant_count")?;
    let root: Option<uuid::Uuid> = row.try_get("root_session_id")?;
    let cascade_scope: Option<String> = row.try_get("cascade_scope")?;
    match (scope.as_str(), count, root, cascade_scope.as_deref()) {
        ("parent_alone", None, None, None) => Ok(TerminationReceipt {
            descendant_scope: DescendantTerminationScope::ParentAlone,
            descendant_count: 0,
        }),
        ("parent_and_descendants", Some(count), Some(root), Some("parent_and_descendants"))
            if root == session.into_uuid() =>
        {
            Ok(TerminationReceipt {
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
                descendant_count: count
                    .parse()
                    .map_err(|_| TerminationReceiptError::Corruption)?,
            })
        }
        _ => Err(TerminationReceiptError::Corruption),
    }
}
