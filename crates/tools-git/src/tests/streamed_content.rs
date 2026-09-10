//! Stream consistency and publication with a bounded descriptor budget.

use super::support::{Fixture, execute};
use crate::arguments::{GitCommitArguments, GitStageArguments, LocalOperation};
use signalbox_tools_workspace::{LocalWorkspaceFileSystem, WorkspaceFileSystem, WorkspaceRoot};
use std::{fs, io::Read, path::Path};

// Two complete stream pages make an inter-page replacement observable.
const PAGE_BYTES: usize = 64 * 1024;

#[test]
fn file_stream_rejects_a_same_size_replacement_between_pages() {
    let fixture = Fixture::new();
    let path = Path::new("stream.bin");
    fs::write(fixture.root().join(path), vec![b'a'; PAGE_BYTES * 2]).expect("stream file");
    let root = WorkspaceRoot::try_new(&LocalWorkspaceFileSystem, fixture.root()).expect("root");
    let mut stream = LocalWorkspaceFileSystem
        .open_file_stream(&root, path)
        .expect("pinned stream");
    let mut page = vec![0; PAGE_BYTES];
    stream.read_exact(&mut page).expect("first page");
    assert!(page.iter().all(|byte| *byte == b'a'));
    fs::rename(
        fixture.root().join(path),
        fixture.root().join("retired.bin"),
    )
    .expect("file retires");
    fs::write(fixture.root().join(path), vec![b'b'; PAGE_BYTES * 2])
        .expect("same-size replacement");
    assert!(stream.read(&mut page).is_err());
}

#[test]
fn file_stream_rejects_an_in_place_rewrite_between_pages() {
    use std::os::unix::fs::FileExt;
    let fixture = Fixture::new();
    let path = Path::new("stream.bin");
    fs::write(fixture.root().join(path), vec![b'a'; PAGE_BYTES * 2]).expect("stream file");
    let root = WorkspaceRoot::try_new(&LocalWorkspaceFileSystem, fixture.root()).expect("root");
    let mut stream = LocalWorkspaceFileSystem
        .open_file_stream(&root, path)
        .expect("pinned stream");
    let mut page = vec![0; PAGE_BYTES];
    stream.read_exact(&mut page).expect("first page");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(fixture.root().join(path))
        .expect("same inode");
    file.write_all_at(&vec![b'b'; PAGE_BYTES], PAGE_BYTES as u64)
        .expect("second page changes");
    file.set_modified(std::time::SystemTime::UNIX_EPOCH)
        .expect("rewrite has a distinct timestamp");
    assert!(stream.read(&mut page).is_err());
}

#[test]
fn repeated_stage_batches_remain_usable_with_1024_descriptors() {
    const CHILD: &str = "SIGNALBOX_STAGE_BATCH_DESCRIPTOR_CHILD";
    const EVIDENCE: &str = "staged batches and commit passed";
    // This is the ordinary soft descriptor limit named by the regression.
    const DESCRIPTORS: usize = 1024;
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new("sh")
            .args(["-c", "ulimit -n \"$1\" && shift && exec \"$@\"", "sh"])
            .arg(DESCRIPTORS.to_string())
            .arg(std::env::current_exe().expect("test executable"))
            .args(["--exact", "tests::streamed_content::repeated_stage_batches_remain_usable_with_1024_descriptors", "--nocapture"])
            .env(CHILD, "1").output().expect("descriptor-limited child");
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains(EVIDENCE),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let fixture = Fixture::new();
    let executor = fixture.executor();
    for batch in 0..2 {
        let paths = (0..crate::limits::MAX_STAGE_PATHS)
            .map(|entry| {
                let name = format!("batch-{batch}-{entry}.txt");
                fs::write(fixture.root().join(&name), name.as_bytes())
                    .expect("distinct content writes");
                name
            })
            .collect();
        execute(
            &executor,
            LocalOperation::Stage(GitStageArguments { paths }),
        );
    }
    execute(&executor, LocalOperation::Status);
    execute(
        &executor,
        LocalOperation::Commit(GitCommitArguments {
            message: "Track both batches".to_owned(),
        }),
    );
    assert_eq!(
        execute(&executor, LocalOperation::Status)["entries"],
        serde_json::json!([])
    );
    let integrity = std::process::Command::new("git")
        .arg("-C")
        .arg(fixture.root())
        .args(["fsck", "--full", "--no-dangling"])
        .output()
        .expect("native Git verifies packs");
    assert!(
        integrity.status.success(),
        "{}",
        String::from_utf8_lossy(&integrity.stderr)
    );
    println!("{EVIDENCE}");
}

#[test]
fn metadata_is_bounded_with_unbounded_blob_configuration() {
    for packed in [false, true] {
        exercise_metadata_bound(2 * crate::limits::MAX_METADATA_OBJECT_BYTES, packed);
    }
}

#[test]
#[ignore = "generates gigabyte metadata objects and a streamed blob"]
fn metadata_bound_rejects_generated_gigabyte_objects_without_materializing_them() {
    exercise_metadata_bound(1_000_000_000, false);
}

fn exercise_metadata_bound(size: usize, packed: bool) {
    use crate::streamed_object::ObjectContent;
    use git2::{ObjectFormat, ObjectType};
    let fixture = Fixture::new();
    for kind in [
        ObjectType::Commit,
        ObjectType::Tree,
        ObjectType::Tag,
        ObjectType::Blob,
    ] {
        // Sparse content is a complete decoded payload, not a header-only refusal.
        let file = tempfile::tempfile().expect("generated payload");
        file.set_len(size as u64).expect("sparse payload size");
        let mut content = ObjectContent { file, size, kind };
        let oid = if packed {
            let oid = content.oid(ObjectFormat::Sha1).expect("object id");
            let mut once = Some(content);
            crate::streamed_object::write_pack(
                &[oid],
                |_| Ok(once.take().expect("one object")),
                ObjectFormat::Sha1,
                &fixture.root().join(".git/objects/pack"),
                None,
            )
            .expect("generated packed object");
            oid
        } else {
            content
                .store(
                    &fixture.root().join(".git/objects"),
                    ObjectFormat::Sha1,
                    None,
                )
                .expect("generated loose object")
        };
        let (_, executor) = crate::LocalGitTools::try_new(
            LocalWorkspaceFileSystem,
            fixture.root(),
            super::support::identity(),
        )
        .expect("default unbounded configuration")
        .into_parts();
        let authority = &executor.repository_authority;
        let repository = authority.open_repository_shell().expect("private shell");
        repository
            .capture_objects_on_read(authority)
            .expect("streamed capture");
        assert_eq!(repository.max_object_bytes, None);
        let result = repository.read_object_header(oid);
        if kind == ObjectType::Blob {
            assert_eq!(
                result.expect("unbounded blob remains readable"),
                (size, kind)
            );
            assert!(repository.find_commit(oid).is_err());
            assert!(repository.find_tree(oid).is_err());
            assert!(repository.find_tag(oid).is_err());
        } else {
            assert!(
                result.is_err(),
                "{kind:?} must be refused before libgit2 materialization"
            );
        }
    }
}
