//! Committed claim and result boundaries fence every dispatch correlation member.

use super::*;
use signalbox_domain::{
    RunnerLeaseState, ToolAttemptObservation, ToolResultContent, ToolResultText,
};

fn mismatches(correlation: &RunnerLeaseCorrelation) -> Vec<RunnerLeaseCorrelation> {
    let mut changed = Vec::new();
    macro_rules! field {
        ($field:ident, $value:expr) => {{
            let mut other = correlation.clone();
            other.$field = $value;
            changed.push(other);
        }};
    }
    field!(lease, RunnerLeaseId::from_uuid(Uuid::now_v7()));
    field!(
        generation,
        correlation
            .generation
            .checked_next()
            .expect("next generation")
    );
    field!(runner, RunnerId::from_uuid(Uuid::now_v7()));
    field!(tool, tool("other"));
    field!(
        registration_revision,
        correlation
            .registration_revision
            .checked_next()
            .expect("next registration")
    );
    field!(
        placement_revision,
        correlation
            .placement_revision
            .checked_next()
            .expect("next placement")
    );
    field!(
        working_directory,
        RunnerWorkingDirectory::try_new("/other".to_owned()).expect("absolute path")
    );
    field!(sandbox, RunnerSandboxProfile::WorkspaceRestricted);
    let dispatch = correlation.dispatch;
    for index in 0..6 {
        let other = ToolAttemptDispatchCorrelation::reconstitute(
            ToolAttemptDispatchCorrelationReconstitutionInput {
                session: if index == 0 {
                    SessionId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.session()
                },
                turn: if index == 1 {
                    TurnId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.turn()
                },
                issuing_attempt: if index == 2 {
                    TurnAttemptId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.issuing_attempt()
                },
                request: if index == 3 {
                    ToolRequestId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.request()
                },
                attempt: if index == 4 {
                    ToolAttemptId::from_uuid(Uuid::now_v7())
                } else {
                    dispatch.attempt()
                },
                generation: if index == 5 {
                    dispatch.generation().checked_next().expect("next dispatch")
                } else {
                    dispatch.generation()
                },
            },
        );
        field!(dispatch, other);
    }
    changed
}

fn success(text: &str) -> ToolAttemptObservation {
    ToolAttemptObservation::Completed {
        result: ToolResultContent::Text(
            ToolResultText::try_new(text.to_owned()).expect("bounded result"),
        ),
    }
}

#[tokio::test]
#[ignore = "requires Docker"]
async fn live_claim_and_result_require_every_fence_and_record_one_terminal_attempt()
-> Result<(), Box<dyn Error>> {
    let (_container, pool) = migrated_postgres().await?;
    let (store, enrollment, _, pin, epoch) =
        stored_active_pin_fixture_with_authorization(&pool, ActivePinEffectCase::EffectFree)
            .await?;
    let correlation = pin.lease.correlation();
    for other in mismatches(&correlation) {
        assert!(
            store
                .claim_tool_lease(enrollment.enrollment(), epoch, other.clone())
                .await
                .is_err(),
            "mismatched claim: {other:?}"
        );
        assert!(
            store
                .record_tool_lease_result(
                    enrollment.enrollment(),
                    epoch,
                    other.clone(),
                    success("result")
                )
                .await
                .is_err(),
            "mismatched result: {other:?}"
        );
    }
    assert!(
        store
            .record_tool_lease_result(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                success("result")
            )
            .await
            .is_err(),
        "an offer is not a claim"
    );
    let claimed = store
        .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
        .await?;
    assert_eq!(claimed.state(), RunnerLeaseState::Claimed);
    assert_eq!(
        store
            .claim_tool_lease(enrollment.enrollment(), epoch, correlation.clone())
            .await?,
        claimed
    );
    let completed = store
        .record_tool_lease_result(
            enrollment.enrollment(),
            epoch,
            correlation.clone(),
            success("result"),
        )
        .await?;
    assert_eq!(completed.state(), RunnerLeaseState::Completed);
    assert_eq!(
        store
            .record_tool_lease_result(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                success("result")
            )
            .await?,
        completed
    );
    assert!(
        store
            .record_tool_lease_result(
                enrollment.enrollment(),
                epoch,
                correlation.clone(),
                success("changed")
            )
            .await
            .is_err()
    );
    let row: (String, String) =
        sqlx::query_as("SELECT state_kind, result_text FROM tool_attempt WHERE attempt_id = $1")
            .bind(correlation.dispatch.attempt().into_uuid())
            .fetch_one(&pool)
            .await?;
    assert_eq!(row, ("terminal".to_owned(), "result".to_owned()));
    Ok(())
}
