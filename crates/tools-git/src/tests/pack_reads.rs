//! Packed delta reads retain the content bound for their dependencies.

use std::{io::Write, path::Path};

use flate2::{Compression, write::ZlibEncoder};
use git2::{Indexer, ObjectFormat, ObjectType, Oid, Repository};
use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::arguments::LocalOperation;
use crate::diff::diff_object_buffer;
use crate::failure::LocalGitFailure;
use crate::limits::MAX_OBJECT_BYTES;
use crate::pinning::{PinnedObjectDatabase, PinnedRepository};
use crate::tests::support::{Fixture, Sha256Fixture, execute};

#[test]
fn packed_delta_read_rejects_an_oversized_base() {
    for encoding in [DeltaEncoding::Offset, DeltaEncoding::Reference] {
        let fixture = Fixture::new();
        let target = plant_delta_chain(fixture.root(), encoding, &[MAX_OBJECT_BYTES + 1, 1]);
        let executor = fixture.executor();
        execute(&executor, LocalOperation::Status);
        let failure = read_packed_blob(&fixture.executor().repository_authority, target)
            .expect_err("small delta cannot expand an oversized base");

        assert_eq!(failure, LocalGitFailure::Operation, "{encoding:?}");
    }
}

#[test]
fn packed_delta_read_rejects_an_oversized_transitive_base() {
    for encoding in [DeltaEncoding::Offset, DeltaEncoding::Reference] {
        let fixture = Fixture::new();
        let target = plant_delta_chain(fixture.root(), encoding, &[MAX_OBJECT_BYTES + 1, 8, 1]);

        let failure = read_packed_blob(&fixture.executor().repository_authority, target)
            .expect_err("bounded immediate base cannot hide an oversized ancestor");

        assert_eq!(failure, LocalGitFailure::Operation, "{encoding:?}");
    }
}

#[test]
fn packed_delta_read_accepts_a_bounded_dependency_chain() {
    for encoding in [DeltaEncoding::Offset, DeltaEncoding::Reference] {
        let fixture = Fixture::new();
        let target = plant_delta_chain(fixture.root(), encoding, &[MAX_OBJECT_BYTES, 8, 1]);

        let content = read_packed_blob(&fixture.executor().repository_authority, target)
            .expect("every decoded base fits the content bound");

        assert_eq!(content, b"x", "{encoding:?}");
    }
}

#[test]
fn packed_delta_read_rejects_an_oversized_delta_program() {
    for encoding in [DeltaEncoding::Offset, DeltaEncoding::Reference] {
        let fixture = Fixture::new();
        // Literal instructions add overhead beyond the bounded reconstructed blob.
        let target = plant_delta_chain(fixture.root(), encoding, &[1, MAX_OBJECT_BYTES]);

        let failure = read_packed_blob(&fixture.executor().repository_authority, target)
            .expect_err("inflated delta instructions exceed the content bound");

        assert_eq!(failure, LocalGitFailure::Operation, "{encoding:?}");
    }
}

#[test]
fn sha256_packed_delta_reads_bound_the_base() {
    for encoding in [DeltaEncoding::Offset, DeltaEncoding::Reference] {
        let fixture = Sha256Fixture::new();
        let oversized = plant_delta_chain(fixture.root(), encoding, &[MAX_OBJECT_BYTES + 1, 1]);
        let bounded = plant_delta_chain(fixture.root(), encoding, &[8, 2]);
        let executor = fixture.executor();

        let failure = read_packed_blob(&executor.repository_authority, oversized)
            .expect_err("SHA-256 delta cannot expand an oversized base");
        let content = read_packed_blob(&executor.repository_authority, bounded)
            .expect("SHA-256 bounded delta remains readable");

        assert_eq!(failure, LocalGitFailure::Operation, "{encoding:?}");
        assert_eq!(content, b"xx", "{encoding:?}");
    }
}

fn read_packed_blob(authority: &PinnedRepository, target: Oid) -> Result<Vec<u8>, LocalGitFailure> {
    let snapshot = PinnedObjectDatabase::capture(authority)
        .expect("unrelated delta and oversized base admit capture");
    let database =
        git2::Odb::new_ext(authority.object_format).expect("snapshot database constructs");
    snapshot.add_to(&database).expect("snapshot attaches");
    let repository = authority
        .open_repository_shell()
        .expect("repository shell opens");
    repository
        .set_odb(&database, &snapshot)
        .expect("snapshot binds");
    diff_object_buffer(&repository, target, 0o100644)
}

#[derive(Clone, Copy, Debug)]
pub(super) enum DeltaEncoding {
    Offset,
    Reference,
}

pub(super) fn plant_delta_chain(root: &Path, encoding: DeltaEncoding, sizes: &[usize]) -> Oid {
    let repository = Repository::open(root).expect("fixture repository opens");
    let format = repository.object_format();
    let mut pack = b"PACK".to_vec();
    pack.extend_from_slice(&2_u32.to_be_bytes());
    pack.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
    let mut previous_offset = 0;
    let mut previous_size = 0;
    let mut previous_oid = Oid::ZERO_SHA1;
    for (position, &size) in sizes.iter().enumerate() {
        let offset = pack.len();
        let content = vec![b'x'; size];
        let oid =
            Oid::hash_object_ext(ObjectType::Blob, &content, format).expect("fixture blob hashes");
        if position == 0 {
            append_entry_header(&mut pack, 3, size);
            append_compressed(&mut pack, &content);
        } else {
            let mut delta = Vec::new();
            append_size(&mut delta, previous_size);
            append_size(&mut delta, size);
            for literal in content.chunks(127) {
                delta.push(literal.len() as u8);
                delta.extend_from_slice(literal);
            }
            match encoding {
                DeltaEncoding::Offset => {
                    append_entry_header(&mut pack, 6, delta.len());
                    let mut distance = offset - previous_offset;
                    let mut encoded = vec![(distance & 0x7f) as u8];
                    while distance > 0x7f {
                        distance = (distance >> 7) - 1;
                        encoded.push(0x80 | (distance & 0x7f) as u8);
                    }
                    pack.extend(encoded.into_iter().rev());
                }
                DeltaEncoding::Reference => {
                    append_entry_header(&mut pack, 7, delta.len());
                    pack.extend_from_slice(previous_oid.as_bytes());
                }
            }
            append_compressed(&mut pack, &delta);
        }
        previous_offset = offset;
        previous_size = size;
        previous_oid = oid;
    }
    let checksum = match format {
        ObjectFormat::Sha1 => Sha1::digest(&pack).to_vec(),
        ObjectFormat::Sha256 => Sha256::digest(&pack).to_vec(),
    };
    pack.extend_from_slice(&checksum);
    let pack_directory = root.join(".git/objects/pack");
    let mut indexer = Indexer::new_ext(None, &pack_directory, 0o600, true, format)
        .expect("fixture pack indexer constructs");
    indexer.write_all(&pack).expect("fixture pack indexes");
    indexer.commit().expect("fixture pack publishes");
    assert_eq!(
        repository
            .find_blob(previous_oid)
            .expect("fixture delta reconstructs")
            .content(),
        vec![b'x'; *sizes.last().expect("fixture chain is nonempty")]
    );
    previous_oid
}

fn append_entry_header(bytes: &mut Vec<u8>, kind: u8, mut size: usize) {
    let first = (kind << 4) | (size & 0x0f) as u8;
    size >>= 4;
    bytes.push(first | if size == 0 { 0 } else { 0x80 });
    if size != 0 {
        append_size(bytes, size);
    }
}

fn append_size(bytes: &mut Vec<u8>, mut size: usize) {
    while size >= 0x80 {
        bytes.push(0x80 | (size & 0x7f) as u8);
        size >>= 7;
    }
    bytes.push(size as u8);
}

fn append_compressed(bytes: &mut Vec<u8>, content: &[u8]) {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(content)
        .expect("fixture content compresses");
    bytes.extend(encoder.finish().expect("fixture zlib stream finishes"));
}
