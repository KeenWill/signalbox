//! Catalog declaration and effect-class properties.

use std::fs;
use std::os::unix::fs::MetadataExt;

use signalbox_application::ToolCatalog;
use signalbox_domain::{NormalizedToolArguments, ToolName};
use signalbox_tools_workspace::LocalWorkspaceFileSystem;

use crate::catalog::LocalGitTools;
use crate::names::GIT_BRANCH_CREATE_NAME;
use crate::tests::support::{FIX_BRANCH, Fixture, Sha256Fixture, identity};

#[test]
fn sha256_catalog_admits_only_full_width_sha256_object_ids() {
    let fixture = Sha256Fixture::new();
    let catalog = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect("SHA-256 suite constructs")
        .into_parts()
        .0;
    let branch_create =
        ToolName::try_new(GIT_BRANCH_CREATE_NAME.to_owned()).expect("fixture name is admitted");
    let full_width = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"name": FIX_BRANCH, "start": fixture.initial.to_string()}).to_string(),
    )
    .expect("full-width SHA-256 arguments normalize");
    let sha1_width = NormalizedToolArguments::try_from_provider_text(
        serde_json::json!({"name": FIX_BRANCH, "start": "0".repeat(40)}).to_string(),
    )
    .expect("SHA-1-width arguments normalize");

    assert_eq!(
        catalog.validate_arguments(&branch_create, &full_width),
        Ok(())
    );
    assert!(
        catalog
            .validate_arguments(&branch_create, &sha1_width)
            .is_err()
    );
}

/// A composition recording which directories a suite bound reads them from the
/// suite rather than resolving the pathname a second time, so the pair names
/// the worktree and the `.git` directory the layout validation accepted.
///
/// The administration directory is replaced after construction and before the
/// pair is read: an implementation that resolved the pathname again would
/// return the replacement, so this is what distinguishes a captured identity
/// from a re-resolved one.
#[test]
fn pinned_directories_name_the_validated_worktree_and_administration_directory() {
    let fixture = Fixture::new();
    let worktree = fs::symlink_metadata(fixture.root()).expect("the fixture worktree is present");
    let administration = fs::symlink_metadata(fixture.root().join(".git"))
        .expect("the fixture administration directory is present");
    let suite = LocalGitTools::try_new(LocalWorkspaceFileSystem, fixture.root(), identity())
        .expect("suite constructs");
    fs::rename(
        fixture.root().join(".git"),
        fixture.root().join(".git.displaced"),
    )
    .expect("the administration directory moves aside");
    fs::create_dir(fixture.root().join(".git")).expect("a replacement stands at the pathname");
    let replacement =
        fs::symlink_metadata(fixture.root().join(".git")).expect("the replacement is present");

    let pinned = suite.pinned_directories();

    assert_eq!(pinned.root.device, worktree.dev());
    assert_eq!(pinned.root.inode, worktree.ino());
    assert_eq!(pinned.administration.device, administration.dev());
    assert_eq!(pinned.administration.inode, administration.ino());
    assert_ne!(pinned.administration.inode, replacement.ino());
}
