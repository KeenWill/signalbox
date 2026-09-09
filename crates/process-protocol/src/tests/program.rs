use super::support::*;
use crate::*;

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
        outcome: ProgramRunCancellationOutcome::AlreadyTerminal {
            terminal_state: ProgramRunTerminalState::Succeeded,
            result: Some(result.clone()),
        },
    };
    let value = serde_json::to_value(&message)?;
    assert_eq!(
        value["outcome"],
        serde_json::json!({"kind":"already_terminal","terminal_state":"succeeded","result":result})
    );
    assert_eq!(serde_json::from_value::<ServerMessage>(value)?, message);
    Ok(())
}
