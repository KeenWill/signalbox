use super::support::*;
use crate::*;

fn placement() -> serde_json::Value {
    serde_json::json!({
        "selector": {"type": "capability_class", "name": "echo"},
        "working_directory": {"type": "exact", "directory": "/tmp/runner-work"},
        "credential_profile": null,
        "workspace": {"type": "none"},
        "sandbox": "ambient",
        "permission_overrides": [{"tool_name": "echo", "permission": "confirm"}]
    })
}

#[test]
fn runner_placement_requires_explicit_independent_axes() {
    let wire = placement();
    let admitted: crate::RunnerPlacementRequest = serde_json::from_value(wire.clone()).unwrap();
    let domain = admitted.try_into_domain().unwrap();
    assert!(domain.credential_profile.is_none());
    assert_eq!(
        domain.workspace,
        signalbox_domain::WorkspaceRequirement::None
    );
    assert_eq!(
        domain
            .permission_overrides
            .get(&signalbox_domain::ToolName::try_new("echo".to_owned()).unwrap()),
        Some(signalbox_domain::RunnerToolPermissionOverride::Confirm),
    );
    assert!(matches!(
        domain.working_directory,
        signalbox_domain::WorkingDirectorySelection::Exact(_)
    ));
    for member in [
        "selector",
        "working_directory",
        "credential_profile",
        "workspace",
        "sandbox",
        "permission_overrides",
    ] {
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove(member);
        assert!(
            serde_json::from_value::<crate::RunnerPlacementRequest>(missing).is_err(),
            "{member}"
        );
    }
}

#[test]
fn duplicate_permission_overrides_reject_each_creation_frame() {
    let mut value = placement();
    let entry = value["permission_overrides"][0].clone();
    value["permission_overrides"]
        .as_array_mut()
        .unwrap()
        .push(entry);
    let runner_placement = Some(serde_json::from_value(value).unwrap());
    let requests = [
        ClientRequest::CreateSession {
            command_id: command(1).unwrap(),
            initial_model_selection: ModelSelection::Direct {
                selection_id: uuid(2),
            },
            model_settings: ModelSettingsOverlay::inherit_all(),
            system_prompt: SystemPromptMember::present(None),
            placement: SessionPlacement::Pathless {},
            lifecycle: crate::SessionLifecycleMembers::default(),
            runner_placement: runner_placement.clone(),
        },
        ClientRequest::CreateSessionFromTemplate {
            command_id: command(1).unwrap(),
            template_name: "review".to_owned(),
            placement: SessionPlacement::Pathless {},
            lifecycle: crate::SessionLifecycleMembers::default(),
            runner_placement: runner_placement.clone(),
        },
        ClientRequest::CommissionSession {
            command_id: command(1).unwrap(),
            template_name: "review".to_owned(),
            fence: CommissionedSessionFence::Branch {
                repository: "sample/repository".to_owned(),
                branch: "main".to_owned(),
            },
            statement: "Review the change".to_owned(),
            content: InputContent::new("Review context".to_owned()),
            runner_placement,
        },
    ];
    for request_value in requests {
        assert_eq!(
            ClientFrame::try_new_for_version(
                ProtocolVersion::One,
                request(1).unwrap(),
                request_value
            ),
            Err(FrameValidationError::PlacementShape)
        );
    }
}
