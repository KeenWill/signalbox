//! Configured-remote push contract and injected-transport properties.

use std::{
    fs,
    sync::{Arc, Mutex},
};

use git2::Repository;
use signalbox_application::ToolCatalog;
use signalbox_domain::{NormalizedToolArguments, ToolEffectClass, ToolName, ToolPermissionDefault};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::GIT_PUSH_CONFIGURED_NAME;
use crate::descriptor::file_identity;
use crate::push_arguments::GitPushArguments;
use crate::push_catalog::{GitPushTools, decode_push};
use crate::push_executor::GitPushFailure;
use crate::push_transport::{
    ConfiguredGitRemote, GitPushReceipt, GitPushRequest, GitPushTransport, GitPushTransportFailure,
};
use crate::tests::support::{FIX_BRANCH, Fixture};

const REMOTE_NAME: &str = "origin";
const REMOTE_URL: &str = "https://github.com/KeenWill/signalbox.git";

#[derive(Clone, Debug, Default)]
struct RecordingPushTransport(Arc<Mutex<Option<GitPushRequest>>>);

impl RecordingPushTransport {
    fn request(&self) -> GitPushRequest {
        self.0
            .lock()
            .expect("recording transport lock is available")
            .clone()
            .expect("push request was recorded")
    }

    fn has_request(&self) -> bool {
        self.0
            .lock()
            .expect("recording transport lock is available")
            .is_some()
    }
}

impl GitPushTransport for RecordingPushTransport {
    fn push(&mut self, request: GitPushRequest) -> Result<GitPushReceipt, GitPushTransportFailure> {
        let receipt = GitPushReceipt::try_new(request.commit().to_owned())
            .expect("resolved commit forms a receipt");
        *self
            .0
            .lock()
            .expect("recording transport lock is available") = Some(request);
        Ok(receipt)
    }
}

#[test]
fn push_contract_requires_non_overridable_confirmation() {
    let fixture = Fixture::new();
    let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
        .expect("configured remote is admitted");
    let catalog = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        remote,
        RecordingPushTransport::default(),
    )
    .expect("push suite constructs")
    .into_parts()
    .0;
    let name =
        ToolName::try_new(GIT_PUSH_CONFIGURED_NAME.to_owned()).expect("fixture name is admitted");
    let definition = catalog.definition(&name).expect("push definition exists");

    assert_eq!(
        definition.permission_default(),
        ToolPermissionDefault::AlwaysConfirm
    );
    assert_eq!(definition.effect_class(), ToolEffectClass::ExternalEffect);
}

#[test]
fn push_contract_rejects_a_model_supplied_destination() {
    let fixture = Fixture::new();
    let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
        .expect("configured remote is admitted");
    let catalog = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        remote,
        RecordingPushTransport::default(),
    )
    .expect("push suite constructs")
    .into_parts()
    .0;
    let name =
        ToolName::try_new(GIT_PUSH_CONFIGURED_NAME.to_owned()).expect("fixture name is admitted");
    let definition = catalog.definition(&name).expect("push definition exists");
    let schema: serde_json::Value =
        serde_json::from_str(definition.input_schema().as_str()).expect("push schema is JSON");
    let injected_destination = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"branch": FIX_BRANCH, "remote": REMOTE_URL}).to_string(),
    )
    .expect("provider JSON normalizes");

    assert_eq!(schema["required"], serde_json::json!(["branch"]));
    assert_eq!(
        schema["additionalProperties"],
        serde_json::Value::Bool(false)
    );
    assert!(decode_push(&injected_destination).is_err());
}

#[test]
fn push_resolves_a_real_branch_for_only_the_configured_transport() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let initial = repository
        .find_commit(fixture.initial)
        .expect("fixture commit exists");
    repository
        .branch(FIX_BRANCH, &initial, false)
        .expect("fixture push branch creates");
    let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
        .expect("configured remote is admitted");
    let transport = RecordingPushTransport::default();
    let mut executor = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        remote,
        transport.clone(),
    )
    .expect("push suite constructs")
    .into_parts()
    .1;

    let encoded = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .expect("synthetic push succeeds");
    let result: serde_json::Value = serde_json::from_str(&encoded).expect("push result is JSON");
    let request = transport.request();

    assert_eq!(
        file_identity(
            &fs::metadata(request.repository_root())
                .expect("pinned transport repository root resolves"),
        ),
        file_identity(&fs::metadata(fixture.root()).expect("fixture root resolves")),
    );
    assert_eq!(request.remote().name(), REMOTE_NAME);
    assert_eq!(request.remote().url(), REMOTE_URL);
    assert_eq!(request.branch(), FIX_BRANCH);
    assert_eq!(request.commit(), fixture.initial.to_string());
    assert_eq!(
        request.refspec(),
        format!("{}:refs/heads/{FIX_BRANCH}", request.commit()),
    );
    assert_eq!(result["remote"], REMOTE_NAME);
    assert_eq!(result["branch"], request.branch());
    assert_eq!(result["commit"], fixture.initial.to_string());
}

#[test]
fn push_rejects_a_replaced_workspace_before_transport_dispatch() {
    let fixture = Fixture::new();
    let remote = ConfiguredGitRemote::try_new(REMOTE_NAME, REMOTE_URL)
        .expect("configured remote is admitted");
    let transport = RecordingPushTransport::default();
    let mut executor = GitPushTools::try_new(
        &LocalWorkspaceFileSystem,
        fixture.root(),
        remote,
        transport.clone(),
    )
    .expect("push suite constructs")
    .into_parts()
    .1;
    let retired = fixture.root().with_extension("retired");
    fs::rename(fixture.root(), &retired).expect("fixture workspace retires");
    Repository::init(fixture.root()).expect("replacement repository initializes");

    let failure = executor
        .execute_push(GitPushArguments::for_test(FIX_BRANCH))
        .expect_err("replacement workspace rejects before dispatch");

    assert_eq!(failure, GitPushFailure::Repository);
    assert!(!transport.has_request());
    fs::remove_dir_all(fixture.root()).expect("replacement repository removes");
    fs::rename(retired, fixture.root()).expect("fixture workspace restores");
}

#[test]
fn push_receipts_reject_non_object_identifiers() {
    assert!(GitPushReceipt::try_new("not-an-object-id").is_err());
}

#[test]
fn push_receipts_canonicalize_equivalent_object_identifier_spelling() {
    let fixture = Fixture::new();
    let uppercase = fixture.initial.to_string().to_ascii_uppercase();

    let receipt = GitPushReceipt::try_new(uppercase).expect("uppercase object ID is admitted");

    assert_eq!(receipt.commit(), fixture.initial.to_string());
}

/// Guards the subset relation between the durable mint vocabulary and the
/// reference grammar this executor applies: a name the domain admits but
/// `gix_validate` refuses would mint a destination that can never resolve.
///
/// This test enforces the relation; it does not own it. The owning statement
/// belongs in the cross-component specification for Git remote authority,
/// which is not written yet because it must also state how a mint is scoped —
/// the decision still open on this pull request. Point this comment at that
/// statement once it lands.
#[track_caller]
fn assert_minted_name_builds_a_configured_remote(candidate: &str) {
    assert!(
        signalbox_domain::GitRemoteName::try_new(candidate.to_owned()).is_ok(),
        "the domain refuses {candidate:?}, so this case no longer covers the subset rule"
    );
    assert!(
        ConfiguredGitRemote::try_new(candidate, "https://example.test/namespace/project.git")
            .is_ok(),
        "minted remote name {candidate:?} is refused by the push executor"
    );
}

#[test]
fn every_minted_remote_name_builds_a_configured_remote() {
    assert_minted_name_builds_a_configured_remote("origin");
    assert_minted_name_builds_a_configured_remote("up-stream_2");
    assert_minted_name_builds_a_configured_remote("v1.0");
    assert_minted_name_builds_a_configured_remote("origin.lockfile");
    assert_minted_name_builds_a_configured_remote("a");
}
