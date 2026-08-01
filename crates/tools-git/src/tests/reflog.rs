//! Reference-log publication and rollback properties.

use std::fs;

use git2::Repository;

use crate::commit::publish_commit_reference_with_hook;
use crate::failure::LocalGitFailure;
use crate::packed_reference::packed_reference_target;
use crate::reference_lock::ReferenceLock;
use crate::reference_read::resolve_pinned_reference_chain;
use crate::reflog::ReferenceLogLock;
use crate::tests::support::{Fixture, commit_with_parents, identity, raw_commit_with_tree};

#[test]
fn commit_restores_reflogs_when_reference_publication_fails() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let tree = repository
        .find_commit(fixture.initial)
        .expect("fixture commit opens")
        .tree_id();
    let new = raw_commit_with_tree(&repository, tree, fixture.initial);
    let executor = fixture.executor();
    let (chain, old) = resolve_pinned_reference_chain(&executor.repository_authority, None)
        .expect("fixture reference chain resolves");
    let update_reference = chain.last().expect("fixture branch target exists");
    let update_lock = ReferenceLock::acquire(&executor.repository_authority, update_reference)
        .expect("fixture target reference locks");
    let reference_path = fixture.root().join(".git").join(update_reference);
    let lock_path = reference_path.with_extension("lock");
    let retired_lock = reference_path.with_extension("retired-lock");
    let head_log = fixture.root().join(".git/logs/HEAD");
    let branch_log = fixture.root().join(".git/logs").join(update_reference);
    let original_head_log = fs::read(&head_log).expect("original HEAD reflog reads");
    let original_branch_log = fs::read(&branch_log).expect("original branch reflog reads");
    let replacement_lock = b"replacement reference lock\n";
    let signature = identity()
        .signature()
        .expect("fixture signature constructs");

    let failure = publish_commit_reference_with_hook(
        &executor.repository_authority,
        update_lock,
        update_reference,
        old.expect("fixture parent exists"),
        new,
        &signature,
        || {
            fs::rename(&lock_path, &retired_lock).expect("target reference lock retires");
            fs::write(&lock_path, replacement_lock).expect("replacement lock writes");
        },
    )
    .expect_err("replaced target lock rejects publication");
    fs::remove_file(&lock_path).expect("replacement reference lock removes");
    fs::remove_file(&retired_lock).expect("retired reference lock removes");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(&head_log).expect("restored HEAD reflog reads"),
        original_head_log
    );
    assert_eq!(
        fs::read(&branch_log).expect("restored branch reflog reads"),
        original_branch_log
    );
    assert_eq!(
        fs::read_to_string(reference_path).expect("unchanged reference reads"),
        format!("{}\n", fixture.initial)
    );
}

#[test]
fn commit_publication_preserves_a_concurrent_destination_reference() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let tree = repository
        .find_commit(fixture.initial)
        .expect("fixture commit opens")
        .tree_id();
    let new = raw_commit_with_tree(&repository, tree, fixture.initial);
    let replacement = commit_with_parents(
        &repository,
        &[fixture.initial],
        "concurrent replacement reference",
    );
    let executor = fixture.executor();
    let (chain, old) = resolve_pinned_reference_chain(&executor.repository_authority, None)
        .expect("fixture reference chain resolves");
    let update_reference = chain.last().expect("fixture branch target exists");
    let update_lock = ReferenceLock::acquire(&executor.repository_authority, update_reference)
        .expect("fixture target reference locks");
    let reference_path = fixture.root().join(".git").join(update_reference);
    let head_log = fixture.root().join(".git/logs/HEAD");
    let branch_log = fixture.root().join(".git/logs").join(update_reference);
    let original_head_log = fs::read(&head_log).expect("original HEAD reflog reads");
    let original_branch_log = fs::read(&branch_log).expect("original branch reflog reads");
    let signature = identity()
        .signature()
        .expect("fixture signature constructs");

    let failure = publish_commit_reference_with_hook(
        &executor.repository_authority,
        update_lock,
        update_reference,
        old.expect("fixture parent exists"),
        new,
        &signature,
        || {
            fs::write(&reference_path, format!("{replacement}\n"))
                .expect("concurrent destination reference writes");
        },
    )
    .expect_err("concurrent destination reference rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(&head_log).expect("restored HEAD reflog reads"),
        original_head_log
    );
    assert_eq!(
        fs::read(&branch_log).expect("restored branch reflog reads"),
        original_branch_log
    );
    assert_eq!(
        fs::read_to_string(reference_path).expect("concurrent reference reads"),
        format!("{replacement}\n")
    );
}

#[test]
fn reference_publication_preserves_a_concurrent_packed_destination() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let tree = repository
        .find_commit(fixture.initial)
        .expect("fixture commit opens")
        .tree_id();
    let new = raw_commit_with_tree(&repository, tree, fixture.initial);
    let replacement = commit_with_parents(
        &repository,
        &[fixture.initial],
        "concurrent packed replacement",
    );
    let executor = fixture.executor();
    let (chain, _) = resolve_pinned_reference_chain(&executor.repository_authority, None)
        .expect("fixture reference chain resolves");
    let update_reference = chain.last().expect("fixture branch target exists");
    let reference_path = fixture.root().join(".git").join(update_reference);
    let packed_references = fixture.root().join(".git/packed-refs");
    fs::write(
        &packed_references,
        format!("{} {update_reference}\n", fixture.initial),
    )
    .expect("initial packed destination writes");
    fs::remove_file(&reference_path).expect("loose destination removes");
    let mut update_lock = ReferenceLock::acquire(&executor.repository_authority, update_reference)
        .expect("packed destination locks");
    let expected = update_lock
        .read(&executor.repository_authority)
        .expect("packed destination reads");
    update_lock
        .prepare(&executor.repository_authority, new)
        .expect("replacement reference prepares");

    let failure = update_lock
        .publish_with_hook(&executor.repository_authority, &expected, || {
            fs::write(
                &packed_references,
                format!("{replacement} {update_reference}\n"),
            )
            .expect("concurrent packed destination writes");
        })
        .expect_err("concurrent packed destination rejects publication");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert!(!reference_path.exists());
    assert_eq!(
        packed_reference_target(&executor.repository_authority, update_reference)
            .expect("concurrent packed destination reads"),
        Some(replacement)
    );
}

#[test]
fn reflog_rollback_removes_new_nested_hierarchy() {
    let fixture = Fixture::new();
    let executor = fixture.executor();
    let signature = identity()
        .signature()
        .expect("fixture signature constructs");
    let nested_reference = "refs/heads/topic/v1";
    let nested_parent = fixture.root().join(".git/logs/refs/heads/topic");
    let mut log = ReferenceLogLock::acquire(&executor.repository_authority, nested_reference)
        .expect("nested fixture reflog locks");
    log.append(
        git2::Oid::ZERO_SHA1,
        fixture.initial,
        &signature,
        "fixture action",
    )
    .expect("nested fixture reflog appends");
    log.publish().expect("nested fixture reflog publishes");

    log.rollback().expect("nested fixture reflog rolls back");
    drop(log);

    assert!(!nested_parent.exists());
}

#[test]
fn reflog_rollback_preserves_a_replacement_published_path() {
    let fixture = Fixture::new();
    let repository = Repository::open(fixture.root()).expect("fixture repository opens");
    let tree = repository
        .find_commit(fixture.initial)
        .expect("fixture commit opens")
        .tree_id();
    let new = raw_commit_with_tree(&repository, tree, fixture.initial);
    let executor = fixture.executor();
    let mut log = ReferenceLogLock::acquire(&executor.repository_authority, "HEAD")
        .expect("fixture HEAD reflog locks");
    let signature = identity()
        .signature()
        .expect("fixture signature constructs");
    log.append(fixture.initial, new, &signature, "fixture action")
        .expect("fixture reflog appends");
    log.publish().expect("fixture reflog publishes");
    let head_log = fixture.root().join(".git/logs/HEAD");
    let retired_log = fixture.root().join(".git/logs/HEAD.retired");
    let replacement_content = b"replacement reflog remains exact\n";
    fs::rename(&head_log, &retired_log).expect("published reflog retires");
    fs::write(&head_log, replacement_content).expect("replacement reflog writes");

    let failure = log
        .rollback()
        .expect_err("replacement published reflog rejects rollback");

    assert_eq!(failure, LocalGitFailure::Operation);
    assert_eq!(
        fs::read(head_log).expect("replacement reflog reads"),
        replacement_content
    );
    assert!(retired_log.exists());
}
