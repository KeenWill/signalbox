use super::support::*;
use crate::*;

#[test]
fn program_registration_rejects_uploaded_native_code_and_unknown_grants()
-> Result<(), Box<dyn std::error::Error>> {
    let registration = ProgramRegistrationInput {
        name: "clock".into(),
        revision: "1".into(),
        executable: ProgramExecutableInput::Native {
            entry: "clock".into(),
            revision: "1".into(),
        },
        grants: vec![ProgramGrant::Time],
    };
    let mut value = serde_json::to_value(&registration)?;
    value["executable"]["artifact"] = serde_json::json!("uploaded code");
    assert!(serde_json::from_value::<ProgramRegistrationInput>(value).is_err());
    let mut value = serde_json::to_value(&registration)?;
    value["grants"] = serde_json::json!(["arbitrary_authority"]);
    assert!(serde_json::from_value::<ProgramRegistrationInput>(value).is_err());
    Ok(())
}

#[test]
fn program_run_read_requires_result_only_for_success() -> Result<(), Box<dyn std::error::Error>> {
    let message = ServerMessage::ProgramRunRead {
        run_id: uuid(1),
        run: ProgramRun {
            registration_id: uuid(2),
            input: vec![0, 255],
            input_extent: ProgramByteExtent::Complete {},
            outcome: ProgramRunState::Succeeded {
                result: vec![128, 0],
                result_extent: ProgramByteExtent::Complete {},
            },
        },
    };
    let frame = ServerFrame::try_new_for_version(ProtocolVersion::One, request(3)?, message)?;
    assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
    assert!(
        serde_json::from_value::<ProgramRunState>(serde_json::json!({"state":"succeeded"}))
            .is_err()
    );
    assert!(
        serde_json::from_value::<ProgramRunState>(
            serde_json::json!({"state":"cancelled","result":[]})
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn program_cancellation_has_a_closed_version_one_receipt() -> Result<(), Box<dyn std::error::Error>>
{
    let command_id = command(1)?;
    let run_id = uuid(2);
    let request = ClientFrame::try_new_for_version(
        ProtocolVersion::One,
        request(3)?,
        ClientRequest::CancelProgramRun { command_id, run_id },
    )?;
    assert_eq!(decode_client_line(&encode_client_line(&request)?)?, request);
    let message = ServerMessage::ProgramRunCancellationReceipt {
        command_id,
        run_id,
        outcome: ProgramRunCancellationOutcome::Applied {
            terminal_state: ProgramRunCancelledState::Cancelled,
            result: (),
        },
    };
    let mut value = serde_json::to_value(&message)?;
    assert_eq!(
        value["outcome"],
        serde_json::json!({"kind":"applied","terminal_state":"cancelled","result":null})
    );
    value["outcome"]["terminal_state"] = serde_json::json!("faulted");
    assert!(serde_json::from_value::<ServerMessage>(value.clone()).is_err());
    value["outcome"]["terminal_state"] = serde_json::json!("cancelled");
    value["outcome"]
        .as_object_mut()
        .expect("outcome object")
        .remove("result");
    assert!(serde_json::from_value::<ServerMessage>(value.clone()).is_err());
    value["outcome"]["result"] = serde_json::json!("invented result");
    assert!(serde_json::from_value::<ServerMessage>(value).is_err());
    Ok(())
}

#[test]
fn successful_program_cancellation_receipt_round_trips_exact_result_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let result = vec![0, 255, 128];
    let message = ServerMessage::ProgramRunCancellationReceipt {
        command_id: command(1)?,
        run_id: uuid(2),
        outcome: ProgramRunCancellationOutcome::AlreadyTerminal(
            ProgramRunTerminalState::Succeeded {
                result: result.clone(),
            },
        ),
    };
    let value = serde_json::to_value(&message)?;
    assert_eq!(
        value["outcome"],
        serde_json::json!({"kind":"already_terminal","terminal_state":"succeeded","result":result})
    );
    assert_eq!(serde_json::from_value::<ServerMessage>(value)?, message);
    Ok(())
}

#[test]
fn already_terminal_receipts_round_trip_only_the_result_for_their_state()
-> Result<(), Box<dyn std::error::Error>> {
    for (state, expected) in [
        (
            ProgramRunTerminalState::Cancelled { result: () },
            serde_json::json!({"kind":"already_terminal","terminal_state":"cancelled","result":null}),
        ),
        (
            ProgramRunTerminalState::Faulted { result: () },
            serde_json::json!({"kind":"already_terminal","terminal_state":"faulted","result":null}),
        ),
        (
            ProgramRunTerminalState::Succeeded { result: Vec::new() },
            serde_json::json!({"kind":"already_terminal","terminal_state":"succeeded","result":[]}),
        ),
    ] {
        let outcome = ProgramRunCancellationOutcome::AlreadyTerminal(state);
        assert_eq!(serde_json::to_value(&outcome)?, expected);
        let frame = ServerFrame::try_new_for_version(
            ProtocolVersion::One,
            request(3)?,
            ServerMessage::ProgramRunCancellationReceipt {
                command_id: command(1)?,
                run_id: uuid(2),
                outcome,
            },
        )?;
        assert_eq!(decode_server_line(&encode_server_line(&frame)?)?, frame);
    }
    Ok(())
}

#[test]
fn already_terminal_receipts_reject_missing_or_inconsistent_results()
-> Result<(), Box<dyn std::error::Error>> {
    for outcome in [
        serde_json::json!({"kind":"already_terminal","terminal_state":"succeeded","result":null}),
        serde_json::json!({"kind":"already_terminal","terminal_state":"succeeded"}),
        serde_json::json!({"kind":"already_terminal","terminal_state":"cancelled","result":[0,255]}),
        serde_json::json!({"kind":"already_terminal","terminal_state":"faulted","result":[0,255]}),
        serde_json::json!({"kind":"already_terminal","terminal_state":"cancelled"}),
        serde_json::json!({"kind":"already_terminal","terminal_state":"faulted"}),
        serde_json::json!({"kind":"already_terminal","terminal_state":"succeeded","result":[],"extra":true}),
    ] {
        let message = serde_json::json!({
            "type": "program_run_cancellation_receipt",
            "command_id": command(1)?, "run_id": uuid(2), "outcome": outcome,
        });
        assert!(
            serde_json::from_value::<ServerMessage>(message).is_err(),
            "{outcome}"
        );
    }
    Ok(())
}

#[test]
fn program_read_truncation_requires_a_total_and_rejects_unknown_markers() {
    for malformed in [
        serde_json::json!({"kind": "truncated"}),
        serde_json::json!({"kind": "complete", "total_bytes": 0}),
        serde_json::json!({"kind": "unknown"}),
    ] {
        assert!(
            serde_json::from_value::<ProgramByteExtent>(malformed.clone()).is_err(),
            "{malformed}"
        );
    }
    assert_eq!(
        serde_json::from_value::<ProgramByteExtent>(
            serde_json::json!({"kind":"truncated","total_bytes":5000000})
        )
        .expect("typed truncation"),
        ProgramByteExtent::Truncated {
            total_bytes: 5000000
        }
    );
}
