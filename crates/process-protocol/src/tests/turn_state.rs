//! Turn state protocol tests.

mod tests {
    use super::super::support::*;
    use crate::*;

    #[test]
    fn awaiting_child_turn_state_round_trips_exact_wait_provenance()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = TurnState::ActiveAwaitingChild {
            await_request_id: uuid(4),
            spawning_request_id: uuid(5),
            child_session_id: uuid(2),
        };
        let encoded = serde_json::to_value(&state)?;
        let decoded = serde_json::from_value::<TurnState>(encoded.clone())?;

        assert_eq!(decoded, state);
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "active_awaiting_child",
                "await_request_id": "00000000-0000-0000-0000-000000000004",
                "spawning_request_id": "00000000-0000-0000-0000-000000000005",
                "child_session_id": "00000000-0000-0000-0000-000000000002"
            })
        );
        // A terminal delegated turn admits only a parent-policy reason and a
        // stopped/cancelled outcome. Crossed pairs such as
        // stopped/parent_cancelled are valid under a bound relationship's own
        // termination policy and are covered by
        // `delegation_terminal_turn_state_round_trips_crossed_parent_policy`;
        // these two remain inadmissible on either half.
        assert_delegation_terminal_state_rejected("stopped", "child_completed");
        assert_delegation_terminal_state_rejected("already_terminal", "parent_cancelled");
        Ok(())
    }

    #[test]
    fn runner_recovery_turn_state_round_trips_interrupted_attempt()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = TurnState::ActiveAwaitingRunnerRecovery {
            runner_id: uuid(2),
            placement_revision: PositiveCanonicalU64::try_new(3)
                .expect("the fixture revision is positive"),
            tool_attempt_id: Some(uuid(4)),
        };
        let encoded = serde_json::to_value(&state)?;
        let decoded = serde_json::from_value::<TurnState>(encoded.clone())?;

        assert_eq!(decoded, state);
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "active_awaiting_runner_recovery",
                "runner_id": "00000000-0000-0000-0000-000000000002",
                "placement_revision": "3",
                "tool_attempt_id": "00000000-0000-0000-0000-000000000004"
            })
        );
        Ok(())
    }

    #[test]
    fn runner_recovery_revision_rejects_zero_before_state_construction() {
        assert_eq!(
            PositiveCanonicalU64::try_new(0),
            Err(CanonicalValueError::Decimal),
        );
    }

    #[test]
    fn runner_recovery_turn_state_round_trips_explicit_absent_attempt()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = TurnState::ActiveAwaitingRunnerRecovery {
            runner_id: uuid(2),
            placement_revision: PositiveCanonicalU64::try_new(3)
                .expect("the fixture revision is positive"),
            tool_attempt_id: None,
        };
        let encoded = serde_json::to_value(&state)?;
        let decoded = serde_json::from_value::<TurnState>(encoded.clone())?;

        assert_eq!(decoded, state);
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "active_awaiting_runner_recovery",
                "runner_id": "00000000-0000-0000-0000-000000000002",
                "placement_revision": "3",
                "tool_attempt_id": null
            })
        );
        Ok(())
    }

    #[test]
    fn runner_recovery_turn_state_requires_nullable_attempt_member() {
        let rejected = serde_json::from_value::<TurnState>(serde_json::json!({
            "type": "active_awaiting_runner_recovery",
            "runner_id": "00000000-0000-0000-0000-000000000002",
            "placement_revision": "3"
        }))
        .expect_err("the nullable tool attempt remains a required wire member");

        assert!(rejected.to_string().contains("tool_attempt_id"));
    }

    /// runner-recovery wire state preserves the positive placement
    /// revision required by its relational source.
    #[test]
    fn runner_recovery_turn_state_rejects_zero_placement_revision() {
        assert_server_malformed(
            r#"{"version":1,"request_id":"1","message":{"type":"transcript_turn","turn_id":"00000000-0000-0000-0000-000000000002","acceptance_position":"1","model_settings":null,"state":{"type":"active_awaiting_runner_recovery","runner_id":"00000000-0000-0000-0000-000000000003","placement_revision":"0","tool_attempt_id":null}}}"#,
        );
    }

    /// the public state type cannot be inhabited with the zero
    /// placement revision rejected by its enclosing frame.
    #[test]
    fn runner_recovery_turn_state_direct_decode_rejects_zero_revision() {
        let rejected = serde_json::from_value::<TurnState>(serde_json::json!({
            "type": "active_awaiting_runner_recovery",
            "runner_id": "00000000-0000-0000-0000-000000000003",
            "placement_revision": "0",
            "tool_attempt_id": null
        }))
        .expect_err("the public runner-recovery state requires a positive revision");

        assert!(rejected.to_string().contains("positive placement revision"));
    }

    #[test]
    fn delegation_terminal_turn_state_round_trips_parent_authority()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = TurnState::DelegationTerminated {
            spawning_request_id: uuid(4),
            outcome: DelegationOutcome::Stopped,
            reason: DelegationReason::ParentStopped,
            provenance: DelegationProvenance::ParentGoalCommand {
                parent_session_id: uuid(1),
                goal_generation: CanonicalU64::new(2),
                command_id: uuid(7),
                descendant_scope: DescendantTerminationScope::ParentAndDescendants,
            },
        };
        let encoded = serde_json::to_value(&state)?;
        let decoded = serde_json::from_value::<TurnState>(encoded.clone())?;

        assert_eq!(decoded, state);
        assert_eq!(
            encoded,
            serde_json::json!({
                "type": "delegation_terminated",
                "spawning_request_id": "00000000-0000-0000-0000-000000000004",
                "outcome": "stopped",
                "reason": "parent_stopped",
                "provenance": {
                    "type": "parent_goal_command",
                    "parent_session_id": "00000000-0000-0000-0000-000000000001",
                    "goal_generation": "2",
                    "command_id": "00000000-0000-0000-0000-000000000007",
                    "descendant_scope": "parent_and_descendants"
                }
            })
        );
        Ok(())
    }

    #[test]
    fn delegation_terminal_turn_state_round_trips_crossed_parent_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        // A bound relationship maps the parent verb through its own policy, so
        // a parent cancellation may terminalize a child with `stop` and a
        // parent stop may terminalize it with `cancel`. All four pairs must
        // survive validation and round trip, matching what `process_read`
        // projects.
        assert_delegation_terminal_state_round_trips(
            DelegationOutcome::Stopped,
            DelegationReason::ParentStopped,
        )?;
        assert_delegation_terminal_state_round_trips(
            DelegationOutcome::Stopped,
            DelegationReason::ParentCancelled,
        )?;
        assert_delegation_terminal_state_round_trips(
            DelegationOutcome::Cancelled,
            DelegationReason::ParentStopped,
        )?;
        assert_delegation_terminal_state_round_trips(
            DelegationOutcome::Cancelled,
            DelegationReason::ParentCancelled,
        )?;
        Ok(())
    }
}
