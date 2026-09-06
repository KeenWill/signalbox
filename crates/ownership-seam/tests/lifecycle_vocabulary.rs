use signalbox_ownership_seam::{
    CoreAgency, DispatchingModule, GoalBlockedReasonKind, SessionFailureCause,
    SessionLifecycleState, SessionParkCause, SessionParkResponder, SessionRecoveryOperation,
    SessionRetirementCause, SessionRetryableCause, SessionStructuralCause,
    SessionTemplateContentDigest, SessionTemplateName, SessionTemplateProvenance,
    SessionTerminalOutcome, SessionWait, ToolRequestId,
};

fn assert_nameable<T>() {}

#[test]
fn lifecycle_event_vocabulary_is_nameable_from_the_seam() {
    assert_nameable::<CoreAgency>();
    assert_nameable::<DispatchingModule>();
    assert_nameable::<GoalBlockedReasonKind>();
    assert_nameable::<SessionFailureCause>();
    assert_nameable::<SessionLifecycleState>();
    assert_nameable::<SessionParkCause>();
    assert_nameable::<SessionParkResponder>();
    assert_nameable::<SessionRecoveryOperation>();
    assert_nameable::<SessionRetirementCause>();
    assert_nameable::<SessionRetryableCause>();
    assert_nameable::<SessionStructuralCause>();
    assert_nameable::<SessionTemplateContentDigest>();
    assert_nameable::<SessionTemplateName>();
    assert_nameable::<SessionTemplateProvenance>();
    assert_nameable::<SessionTerminalOutcome>();
    assert_nameable::<SessionWait>();
    assert_nameable::<ToolRequestId>();
}

#[test]
fn template_provenance_is_constructible_from_seam_exports() {
    let name = SessionTemplateName::try_new(String::from("repository-watch"))
        .expect("fixture template name is valid");
    let digest = SessionTemplateContentDigest::from_bytes([7; 32]);

    let provenance = SessionTemplateProvenance::new(name, digest);

    assert_eq!(provenance.name().as_str(), "repository-watch");
    assert_eq!(provenance.content_digest(), digest);
}
