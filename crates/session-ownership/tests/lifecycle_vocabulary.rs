use signalbox_session_ownership::{
    SessionTemplateContentDigest, SessionTemplateName, SessionTemplateProvenance,
};

#[test]
fn template_provenance_is_constructible_from_seam_exports() {
    let name = SessionTemplateName::try_new(String::from("repository-watch"))
        .expect("fixture template name is valid");
    let digest = SessionTemplateContentDigest::from_bytes([7; 32]);

    let provenance = SessionTemplateProvenance::new(name, digest);

    assert_eq!(provenance.name().as_str(), "repository-watch");
    assert_eq!(provenance.content_digest(), digest);
}
