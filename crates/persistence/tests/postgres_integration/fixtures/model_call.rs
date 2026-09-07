//! Model-call reconciliation projections.

use crate::*;

pub(crate) type ReconciliationDispatch = (
    SessionId,
    TurnId,
    DispatchedReconciliationOperation,
    ContextFrontierId,
);

pub(crate) async fn drain_reconciliation_dispatches(
    pool: &PgPool,
) -> Result<Vec<ReconciliationDispatch>, OutboxDispatchError> {
    let mut reconciliations = Vec::new();
    drain_outbox(pool, |event| {
        let (
            Some(session),
            DispatchedOutboxEventKind::TurnTerminal {
                turn,
                disposition:
                    DispatchedTurnTerminalDisposition::ReconciliationRequired {
                        operation,
                        terminal_frontier,
                    },
            },
        ) = (event.session(), event.kind())
        else {
            return;
        };
        reconciliations.push((session, *turn, *operation, *terminal_frontier));
    })
    .await?;
    Ok(reconciliations)
}

#[track_caller]
pub(crate) fn assert_projected_steering_entry(
    entry: &ProcessTranscriptEntry,
    expected_input: AcceptedInputId,
    expected_turn: TurnId,
    expected_content: &str,
) {
    assert!(matches!(
        entry,
        ProcessTranscriptEntry::User {
            accepted_input,
            turn,
            content,
            ..
        } if *accepted_input == expected_input
            && *turn == expected_turn
            && content == &user_content(expected_content)
    ));
}
