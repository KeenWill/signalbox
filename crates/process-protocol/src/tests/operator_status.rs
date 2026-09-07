//! Operator status protocol tests.

mod tests {
    use super::super::support::*;
    use crate::*;

    #[test]
    fn operator_status_request_and_rows_round_trip_in_one_closed_vocabulary()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_client_request_round_trip(
            request(1)?,
            ClientRequest::ReadOperatorStatus {},
            r#"{"type":"read_operator_status"}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::LifecycleWeek(
                Box::new(OperatorStatusLifecycleWeekMessage {
                    week_start_date: String::from("2026-08-31"),
                    completion_failure_numerator: CanonicalU64::new(3),
                    completion_failure_denominator: CanonicalU64::new(40),
                    failed_unknown_count: CanonicalU64::new(1),
                    overflow_numerator: CanonicalU64::new(5),
                    overflow_denominator: CanonicalU64::new(44),
                    finish_given_overflow_numerator: CanonicalU64::new(4),
                    wall_numerator: CanonicalU64::new(0),
                    wall_denominator: CanonicalU64::new(38),
                    wall_occurrence_count: CanonicalU64::new(0),
                    classified_terminal_turn_count: CanonicalU64::new(980),
                    terminal_turn_count: CanonicalU64::new(985),
                    classified_known_failed_call_count: CanonicalU64::new(91),
                    known_failed_call_count: CanonicalU64::new(95),
                }),
            ))),
            r#"{"type":"operator_status","kind":"lifecycle_week","week_start_date":"2026-08-31","completion_failure_numerator":"3","completion_failure_denominator":"40","failed_unknown_count":"1","overflow_numerator":"5","overflow_denominator":"44","finish_given_overflow_numerator":"4","wall_numerator":"0","wall_denominator":"38","wall_occurrence_count":"0","classified_terminal_turn_count":"980","terminal_turn_count":"985","classified_known_failed_call_count":"91","known_failed_call_count":"95"}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::LifecycleDeadlineViolation(Box::new(
                    OperatorStatusLifecycleDeadlineViolationMessage {
                        session_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(0x2a)),
                        state: OperatorStatusLifecycleState::Parked,
                        deadline_missing: false,
                        expired_for_seconds: Some(CanonicalU64::new(90)),
                    },
                )),
            )),
            r#"{"type":"operator_status","kind":"lifecycle_deadline_violation","session_id":"00000000-0000-0000-0000-00000000002a","state":"parked","deadline_missing":false,"expired_for_seconds":"90"}"#,
        )?;
        assert_server_message_round_trip(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::End(Box::new(
                OperatorStatusEndMessage {
                    lifecycle_week_count: CanonicalU64::new(1),
                    lifecycle_deadline_violation_count: CanonicalU64::new(1),
                },
            )))),
            r#"{"type":"operator_status","kind":"end","lifecycle_week_count":"1","lifecycle_deadline_violation_count":"1"}"#,
        )?;
        Ok(())
    }

    /// A metric row whose numerator exceeds its own population is not a rate.
    #[test]
    fn operator_status_rejects_a_lifecycle_week_that_is_not_a_rate()
    -> Result<(), Box<dyn std::error::Error>> {
        let impossible = ServerFrame::try_new(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(OperatorStatusMessage::LifecycleWeek(
                Box::new(OperatorStatusLifecycleWeekMessage {
                    week_start_date: String::from("2026-08-31"),
                    completion_failure_numerator: CanonicalU64::new(41),
                    completion_failure_denominator: CanonicalU64::new(40),
                    failed_unknown_count: CanonicalU64::new(0),
                    overflow_numerator: CanonicalU64::new(0),
                    overflow_denominator: CanonicalU64::new(44),
                    finish_given_overflow_numerator: CanonicalU64::new(0),
                    wall_numerator: CanonicalU64::new(0),
                    wall_denominator: CanonicalU64::new(38),
                    wall_occurrence_count: CanonicalU64::new(0),
                    classified_terminal_turn_count: CanonicalU64::new(0),
                    terminal_turn_count: CanonicalU64::new(0),
                    classified_known_failed_call_count: CanonicalU64::new(0),
                    known_failed_call_count: CanonicalU64::new(0),
                }),
            ))),
        );
        assert!(matches!(
            impossible,
            Err(FrameValidationError::OperatorStatusShape)
        ));

        Ok(())
    }

    /// A session with no armed record has no expiry to be past, so the two
    /// fields cannot both speak.
    #[test]
    fn operator_status_rejects_a_deadline_violation_that_contradicts_itself()
    -> Result<(), Box<dyn std::error::Error>> {
        let contradictory_deadline = ServerFrame::try_new(
            request(1)?,
            ServerMessage::OperatorStatus(Box::new(
                OperatorStatusMessage::LifecycleDeadlineViolation(Box::new(
                    OperatorStatusLifecycleDeadlineViolationMessage {
                        session_id: CanonicalUuid::from_uuid(uuid::Uuid::from_u128(0x2a)),
                        state: OperatorStatusLifecycleState::Parked,
                        deadline_missing: true,
                        expired_for_seconds: Some(CanonicalU64::new(90)),
                    },
                )),
            )),
        );

        assert!(matches!(
            contradictory_deadline,
            Err(FrameValidationError::OperatorStatusShape)
        ));
        Ok(())
    }

    /// A digit-shaped value that names no day is not a week label.
    #[test]
    fn operator_status_rejects_a_week_label_that_names_no_day() {
        assert!(operator_status_calendar_date_is_valid("2026-08-31"));
        assert!(operator_status_calendar_date_is_valid("2024-02-29"));
        assert!(!operator_status_calendar_date_is_valid("2026-99-99"));
        assert!(!operator_status_calendar_date_is_valid("2026-02-29"));
        assert!(!operator_status_calendar_date_is_valid("2026-8-31"));
        assert!(!operator_status_calendar_date_is_valid("+026-08-31"));
        assert!(!operator_status_calendar_date_is_valid("2026-+8-31"));
        assert!(!operator_status_calendar_date_is_valid(
            "2026-08-31T00:00:00Z"
        ));
    }
}
