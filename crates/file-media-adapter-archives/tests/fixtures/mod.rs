//! Generated archive fixtures for the contract in `docs/spec/file-and-media.md`.

use std::{error::Error, io::Write, num::NonZeroU64};

use flate2::{Compression, GzBuilder, write::GzEncoder};
use signalbox_file_media_runtime::{
    AttachmentKind, DeclaredMediaType, FileDigest, FileUse, SourceReadError, SourceReadFuture,
    VerifiedBlobSource,
};
use tar::{Builder, EntryType, Header};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

const PAYLOAD: &[u8] = b"generated archive payload";

pub struct ArchiveFixture {
    bytes: Vec<u8>,
    media_type: &'static str,
    expected_format: &'static str,
    expected_name: &'static str,
}

impl ArchiveFixture {
    pub fn zip() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zip_bytes(&[("docs/readme.txt", PAYLOAD, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "docs/readme.txt",
        })
    }

    pub fn zip_with_preamble() -> Result<Self, Box<dyn Error>> {
        let mut bytes = b"bounded self-extractor preamble".to_vec();
        bytes.extend_from_slice(&zip_bytes(&[(
            "docs/readme.txt",
            PAYLOAD,
            ZipEntryKind::File,
        )])?);
        Ok(Self {
            bytes,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "docs/readme.txt",
        })
    }

    pub fn zip_with_gzip_signature_preamble() -> Result<Self, Box<dyn Error>> {
        let mut bytes = b"\x1f\x8b\x08signature-like ZIP preamble".to_vec();
        bytes.extend_from_slice(&zip_bytes(&[(
            "docs/readme.txt",
            PAYLOAD,
            ZipEntryKind::File,
        )])?);
        Ok(Self {
            bytes,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "docs/readme.txt",
        })
    }

    pub fn zip_after_long_preamble() -> Result<Self, Box<dyn Error>> {
        let mut bytes = vec![b'x'; 1_025];
        bytes.extend_from_slice(&zip_bytes(&[(
            "docs/readme.txt",
            PAYLOAD,
            ZipEntryKind::File,
        )])?);
        Ok(Self {
            bytes,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "docs/readme.txt",
        })
    }

    pub fn zip_after_signature_like_long_preamble() -> Result<Self, Box<dyn Error>> {
        let mut bytes = b"\x1f\x8b\x08".to_vec();
        bytes.resize(1_025, b'x');
        bytes.extend_from_slice(&zip_bytes(&[(
            "docs/readme.txt",
            PAYLOAD,
            ZipEntryKind::File,
        )])?);
        Ok(Self {
            bytes,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "docs/readme.txt",
        })
    }

    pub fn legacy_named_zip() -> Result<Self, Box<dyn Error>> {
        let mut bytes = zip_bytes(&[("cafe.txt", PAYLOAD, ZipEntryKind::File)])?;
        set_legacy_filename(&mut bytes, b"caf\x82.txt")?;
        Ok(Self {
            bytes,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "café.txt",
        })
    }

    pub fn unsupported_compression_zip() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::zip()?;
        set_compression_method(&mut fixture.bytes, 12)?;
        Ok(fixture)
    }

    pub fn tar() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: tar_file("docs/readme.txt", PAYLOAD)?,
            media_type: "application/x-tar",
            expected_format: "tar",
            expected_name: "docs/readme.txt",
        })
    }

    pub fn concatenated_tar_with_hostile_second_segment() -> Result<Self, Box<dyn Error>> {
        let mut bytes = tar_file("docs/readme.txt", PAYLOAD)?;
        bytes.extend_from_slice(&tar_file("host\\payload", PAYLOAD)?);
        Ok(Self {
            bytes,
            media_type: "application/x-tar",
            expected_format: "tar",
            expected_name: "docs/readme.txt",
        })
    }

    pub fn empty_tar() -> Self {
        Self {
            bytes: vec![0; 1_024],
            media_type: "application/x-tar",
            expected_format: "tar",
            expected_name: "",
        }
    }

    pub fn data_bearing_tar_directory() -> Result<Self, Box<dyn Error>> {
        let mut header = Header::new_gnu();
        header.set_path("payload/")?;
        header.set_entry_type(EntryType::Directory);
        header.set_size(u64::try_from(PAYLOAD.len())?);
        header.set_mode(0o755);
        header.set_cksum();
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend_from_slice(PAYLOAD);
        bytes.resize(2_048, 0);
        Ok(Self {
            bytes,
            media_type: "application/x-tar",
            expected_format: "tar",
            expected_name: "payload/",
        })
    }

    pub fn gzip() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: gzip_bytes("payload.txt", PAYLOAD)?,
            media_type: "application/gzip",
            expected_format: "gzip",
            expected_name: "payload.txt",
        })
    }

    pub fn gzip_with_zip_signature_in_extra() -> Result<Self, Box<dyn Error>> {
        let mut bytes = gzip_bytes("payload.txt", PAYLOAD)?;
        insert_gzip_extra(&mut bytes, b"PK\x03\x04")?;
        Ok(Self {
            bytes,
            media_type: "application/gzip",
            expected_format: "gzip",
            expected_name: "payload.txt",
        })
    }

    pub fn latin1_named_gzip() -> Result<Self, Box<dyn Error>> {
        let mut bytes = gzip_bytes("cafe.txt", PAYLOAD)?;
        let filename = bytes
            .get_mut(10..18)
            .ok_or("GZIP fixture filename absent")?;
        filename.copy_from_slice(b"caf\xe9.txt");
        Ok(Self {
            bytes,
            media_type: "application/gzip",
            expected_format: "gzip",
            expected_name: "café.txt",
        })
    }

    pub fn gzip_with_hostile_later_member() -> Result<Self, Box<dyn Error>> {
        let mut bytes = gzip_bytes("payload.txt", PAYLOAD)?;
        bytes.extend_from_slice(&gzip_bytes("../../host", PAYLOAD)?);
        Ok(Self {
            bytes,
            media_type: "application/gzip",
            expected_format: "gzip",
            expected_name: "payload.txt",
        })
    }

    pub fn gzip_with_split_zip_signature() -> Result<Self, Box<dyn Error>> {
        let nested = zip_bytes(&[("nested.txt", PAYLOAD, ZipEntryKind::File)])?;
        let mut bytes = gzip_bytes("payload.bin", &nested[..2])?;
        bytes.extend_from_slice(&gzip_bytes("payload.bin", &nested[2..])?);
        Ok(Self {
            bytes,
            media_type: "application/gzip",
            expected_format: "gzip",
            expected_name: "payload.bin",
        })
    }

    pub fn zstd() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zstd::stream::encode_all(PAYLOAD, 1)?,
            media_type: "application/zstd",
            expected_format: "zstd",
            expected_name: "content",
        })
    }

    pub fn zstd_with_skippable_frame() -> Result<Self, Box<dyn Error>> {
        let compressed = zstd::stream::encode_all(PAYLOAD, 1)?;
        let mut bytes = b"\x50\x2a\x4d\x18\x00\x00\x00\x00".to_vec();
        bytes.extend_from_slice(&compressed);
        Ok(Self {
            bytes,
            media_type: "application/zstd",
            expected_format: "zstd",
            expected_name: "content",
        })
    }

    pub fn zstd_with_zip_signature_in_skippable_frame() -> Result<Self, Box<dyn Error>> {
        let compressed = zstd::stream::encode_all(PAYLOAD, 1)?;
        let mut bytes = b"\x50\x2a\x4d\x18\x04\x00\x00\x00PK\x03\x04".to_vec();
        bytes.extend_from_slice(&compressed);
        Ok(Self {
            bytes,
            media_type: "application/zstd",
            expected_format: "zstd",
            expected_name: "content",
        })
    }

    pub fn zstd_with_only_skippable_frames() -> Self {
        Self {
            bytes: b"\x50\x2a\x4d\x18\x00\x00\x00\x00".to_vec(),
            media_type: "application/zstd",
            expected_format: "zstd",
            expected_name: "content",
        }
    }

    pub fn dictionary_zstd() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            // This frame header declares dictionary ID 1.
            bytes: b"\x28\xb5\x2f\xfd\x21\x01\x00\x01\x00\x00".to_vec(),
            media_type: "application/zstd",
            expected_format: "zstd",
            expected_name: "content",
        })
    }

    pub fn concatenated_dictionary_zstd() -> Result<Self, Box<dyn Error>> {
        let mut bytes = zstd::stream::encode_all(PAYLOAD, 1)?;
        bytes.extend_from_slice(b"\x28\xb5\x2f\xfd\x21\x01\x00\x01\x00\x00");
        Ok(Self {
            bytes,
            media_type: "application/zstd",
            expected_format: "zstd",
            expected_name: "content",
        })
    }

    pub fn truncated_zip() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::zip()?;
        fixture.bytes.truncate(12);
        Ok(fixture)
    }

    pub fn truncated_tar() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::tar()?;
        fixture.bytes.truncate(520);
        Ok(fixture)
    }

    pub fn truncated_gzip() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::gzip()?;
        fixture.bytes.truncate(11);
        Ok(fixture)
    }

    pub fn truncated_zstd() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::zstd()?;
        fixture.bytes.truncate(5);
        Ok(fixture)
    }

    pub fn locked_zip() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::zip()?;
        set_encryption_flags(&mut fixture.bytes)?;
        Ok(fixture)
    }

    pub fn zip_slip() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zip_bytes(&[("../../host", PAYLOAD, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "../../host",
        })
    }

    pub fn zip_symlink() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zip_bytes(&[("host-link", b"../../host", ZipEntryKind::Symlink)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "host-link",
        })
    }

    pub fn data_bearing_zip_directory() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zip_bytes(&[("payload/", PAYLOAD, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload/",
        })
    }

    pub fn mode_only_data_bearing_zip_directory() -> Result<Self, Box<dyn Error>> {
        let mut bytes = zip_bytes(&[("payload", PAYLOAD, ZipEntryKind::File)])?;
        set_unix_mode(&mut bytes, 0o040_755)?;
        Ok(Self {
            bytes,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload",
        })
    }

    pub fn zero_sized_data_bearing_zip_directory() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::data_bearing_zip_directory()?;
        set_uncompressed_size(&mut fixture.bytes, 0)?;
        Ok(fixture)
    }

    pub fn tar_symlink() -> Result<Self, Box<dyn Error>> {
        let mut builder = Builder::new(Vec::new());
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        builder.append_link(&mut header, "host-link", "../../host")?;
        Ok(Self {
            bytes: builder.into_inner()?,
            media_type: "application/x-tar",
            expected_format: "tar",
            expected_name: "host-link",
        })
    }

    pub fn recursive_zip() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zip_bytes(&[("nested.tar", PAYLOAD, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "nested.tar",
        })
    }

    pub fn disguised_recursive_zip() -> Result<Self, Box<dyn Error>> {
        let nested = gzip_bytes("nested.txt", PAYLOAD)?;
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", &nested, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn zip_with_gzip_signature_text_payload() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zip_bytes(&[(
                "payload.bin",
                b"\x1f\x8b\x08not a complete GZIP stream",
                ZipEntryKind::File,
            )])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn zip_with_zstd_signature_text_payload() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zip_bytes(&[(
                "payload.bin",
                b"\x28\xb5\x2f\xfdnot a complete Zstandard frame",
                ZipEntryKind::File,
            )])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn zip_with_oversized_nested_gzip() -> Result<Self, Box<dyn Error>> {
        let payload = vec![b'x'; 8 * 1024 * 1024 + 1];
        let nested = gzip_bytes("payload.txt", &payload)?;
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", &nested, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn zip_with_corrupt_oversized_nested_gzip() -> Result<Self, Box<dyn Error>> {
        let payload = vec![b'x'; 8 * 1024 * 1024 + 1];
        let mut nested = gzip_bytes("payload.txt", &payload)?;
        let trailer_byte = nested
            .len()
            .checked_sub(8)
            .ok_or("GZIP fixture must contain a trailer")?;
        nested[trailer_byte] ^= 0xff;
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", &nested, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn zip_with_oversized_nested_zstd() -> Result<Self, Box<dyn Error>> {
        let payload = vec![b'x'; 8 * 1024 * 1024 + 1];
        let nested = zstd::stream::encode_all(payload.as_slice(), 1)?;
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", &nested, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn zip_with_corrupt_dictionary_zstd_payload() -> Result<Self, Box<dyn Error>> {
        let nested = b"\x28\xb5\x2f\xfd\x21\x01";
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", nested, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn zip_with_tar_signature_text_payload() -> Result<Self, Box<dyn Error>> {
        let mut payload = vec![0_u8; 512];
        payload[257..262].copy_from_slice(b"ustar");
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", &payload, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn zip_with_signature_text_payload() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zip_bytes(&[(
                "payload.bin",
                b"ordinary text containing PK\x03\x04 but no nested archive",
                ZipEntryKind::File,
            )])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn disguised_empty_zip() -> Result<Self, Box<dyn Error>> {
        let writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let empty_zip = writer.finish()?.into_inner();
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", &empty_zip, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn disguised_recursive_zip_after_long_preamble() -> Result<Self, Box<dyn Error>> {
        let mut nested = vec![b'x'; 1_025];
        nested.extend_from_slice(&zip_bytes(&[("nested.txt", PAYLOAD, ZipEntryKind::File)])?);
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", &nested, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn disguised_v7_tar() -> Result<Self, Box<dyn Error>> {
        let mut header = Header::new_old();
        header.set_path("payload.txt")?;
        header.set_entry_type(EntryType::Regular);
        header.set_size(u64::try_from(PAYLOAD.len())?);
        header.set_mode(0o644);
        header.set_cksum();
        let mut nested = header.as_bytes().to_vec();
        nested.extend_from_slice(PAYLOAD);
        nested.resize(1024, 0);
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", &nested, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn disguised_empty_tar() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: zip_bytes(&[("payload.bin", &[0; 1_024], ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn mislabeled_zip() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: b"not an archive".to_vec(),
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn oversized_mislabeled_zip() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: vec![b'x'; 256 * 1_024 + 1],
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.bin",
        })
    }

    pub fn recursive_zstd() -> Result<Self, Box<dyn Error>> {
        let nested = zip_bytes(&[("nested.txt", PAYLOAD, ZipEntryKind::File)])?;
        Ok(Self {
            bytes: zstd::stream::encode_all(nested.as_slice(), 1)?,
            media_type: "application/zstd",
            expected_format: "zstd",
            expected_name: "content",
        })
    }

    pub fn hostile_gzip_name() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: gzip_bytes("../../host", PAYLOAD)?,
            media_type: "application/gzip",
            expected_format: "gzip",
            expected_name: "../../host",
        })
    }

    pub fn zip_bomb() -> Result<Self, Box<dyn Error>> {
        let payload = vec![b'x'; 8 * 1024 * 1024 + 1];
        Ok(Self {
            bytes: zip_bytes(&[("payload.txt", &payload, ZipEntryKind::File)])?,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "payload.txt",
        })
    }

    pub fn gzip_bomb() -> Result<Self, Box<dyn Error>> {
        let payload = vec![b'x'; 16 * 1024 * 1024 + 1];
        Ok(Self {
            bytes: gzip_bytes("payload.txt", &payload)?,
            media_type: "application/gzip",
            expected_format: "gzip",
            expected_name: "payload.txt",
        })
    }

    pub fn gzip_entry_bomb() -> Result<Self, Box<dyn Error>> {
        let payload = vec![b'x'; 8 * 1024 * 1024 + 1];
        Ok(Self {
            bytes: gzip_bytes("payload.txt", &payload)?,
            media_type: "application/gzip",
            expected_format: "gzip",
            expected_name: "payload.txt",
        })
    }

    pub fn zstd_bomb() -> Result<Self, Box<dyn Error>> {
        let payload = vec![b'x'; 16 * 1024 * 1024 + 1];
        Ok(Self {
            bytes: zstd::stream::encode_all(payload.as_slice(), 1)?,
            media_type: "application/zstd",
            expected_format: "zstd",
            expected_name: "content",
        })
    }

    pub fn zstd_entry_bomb() -> Result<Self, Box<dyn Error>> {
        let payload = vec![b'x'; 8 * 1024 * 1024 + 1];
        Ok(Self {
            bytes: zstd::stream::encode_all(payload.as_slice(), 1)?,
            media_type: "application/zstd",
            expected_format: "zstd",
            expected_name: "content",
        })
    }

    pub fn tar_declared_bomb() -> Result<Self, Box<dyn Error>> {
        let mut header = Header::new_gnu();
        header.set_path("payload.bin")?;
        header.set_entry_type(EntryType::Regular);
        header.set_size(8 * 1024 * 1024 + 1);
        header.set_mode(0o644);
        header.set_cksum();
        let mut bytes = header.as_bytes().to_vec();
        bytes.resize(1024, 0);
        Ok(Self {
            bytes,
            media_type: "application/x-tar",
            expected_format: "tar",
            expected_name: "payload.bin",
        })
    }

    pub fn excessive_zip_entries() -> Result<Self, Box<dyn Error>> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        for index in 0..1_001 {
            writer.start_file(format!("empty-{index}"), options)?;
        }
        Ok(Self {
            bytes: writer.finish()?.into_inner(),
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "empty-0",
        })
    }

    /// A central directory whose second record repeats the first record's name. The
    /// hidden first record is the encrypted one, so a reader that keeps only the last
    /// record of each name reports a clean inventory for an archive that is not clean.
    pub fn zip_with_duplicate_central_directory_names() -> Result<Self, Box<dyn Error>> {
        let mut bytes = zip_bytes(&[
            ("a.txt", PAYLOAD, ZipEntryKind::File),
            ("b.txt", PAYLOAD, ZipEntryKind::File),
        ])?;
        set_encryption_flags(&mut bytes)?;
        repeat_first_central_filename(&mut bytes)?;
        Ok(Self {
            bytes,
            media_type: "application/zip",
            expected_format: "zip",
            expected_name: "a.txt",
        })
    }

    pub const fn expected_format(&self) -> &'static str {
        self.expected_format
    }

    pub const fn expected_name(&self) -> &'static str {
        self.expected_name
    }

    pub fn into_source(self) -> Result<MemorySource, Box<dyn Error>> {
        MemorySource::new(self.bytes, self.media_type)
    }
}

/// A complete ZIP carried as the payload of a Zstandard skippable frame: one source that
/// is simultaneously a decoder-valid Zstandard stream and a valid ZIP behind an
/// eight-byte preamble.
pub fn zip_inside_zstd_skippable_frame() -> Result<Vec<u8>, Box<dyn Error>> {
    let archive = zip_bytes(&[("docs/readme.txt", PAYLOAD, ZipEntryKind::File)])?;
    let mut bytes = b"\x50\x2a\x4d\x18".to_vec();
    bytes.extend_from_slice(&u32::try_from(archive.len())?.to_le_bytes());
    bytes.extend_from_slice(&archive);
    Ok(bytes)
}

/// A decoder-valid ZIP whose central directory carries the longest entry names the adapter
/// admits, each built almost entirely from the one character JSON escaping doubles. Every
/// record points at a single empty local record, so the archive stays under the source
/// ceiling and inside the entry-count ceiling while the enumerated inventory serializes
/// past the declared `entries` output bound.
pub fn zip_with_escape_expanding_entry_names() -> Result<Vec<u8>, Box<dyn Error>> {
    const NAME_BYTES: usize = 512;
    const RECORDS: usize = 469;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"PK\x03\x04");
    bytes.extend_from_slice(&20_u16.to_le_bytes());
    bytes.extend_from_slice(&0x0800_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());

    let directory_start = u32::try_from(bytes.len())?;
    for index in 0..RECORDS {
        let mut name = vec![b'"'; NAME_BYTES];
        *name
            .get_mut(index)
            .ok_or("entry name needs a unique byte")? = b'a';
        bytes.extend_from_slice(b"PK\x01\x02");
        bytes.extend_from_slice(&20_u16.to_le_bytes());
        bytes.extend_from_slice(&20_u16.to_le_bytes());
        bytes.extend_from_slice(&0x0800_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&u16::try_from(NAME_BYTES)?.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&name);
    }

    let directory_bytes = u32::try_from(bytes.len())? - directory_start;
    let records = u16::try_from(RECORDS)?;
    bytes.extend_from_slice(b"PK\x05\x06");
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&records.to_le_bytes());
    bytes.extend_from_slice(&records.to_le_bytes());
    bytes.extend_from_slice(&directory_bytes.to_le_bytes());
    bytes.extend_from_slice(&directory_start.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    Ok(bytes)
}

#[derive(Clone, Copy)]
enum ZipEntryKind {
    File,
    Symlink,
}

fn zip_bytes(entries: &[(&str, &[u8], ZipEntryKind)]) -> Result<Vec<u8>, Box<dyn Error>> {
    let cursor = std::io::Cursor::new(Vec::new());
    let mut writer = ZipWriter::new(cursor);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, body, kind) in entries {
        match kind {
            ZipEntryKind::File => {
                writer.start_file(*name, options)?;
                writer.write_all(body)?;
            }
            ZipEntryKind::Symlink => writer.add_symlink(*name, "../../host", options)?,
        }
    }
    Ok(writer.finish()?.into_inner())
}

fn tar_file(name: &str, body: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut builder = Builder::new(Vec::new());
    let mut header = Header::new_gnu();
    header.set_entry_type(EntryType::Regular);
    header.set_size(u64::try_from(body.len())?);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, name, body)?;
    Ok(builder.into_inner()?)
}

fn gzip_bytes(name: &str, body: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut encoder: GzEncoder<Vec<u8>> = GzBuilder::new()
        .filename(name)
        .write(Vec::new(), Compression::fast());
    encoder.write_all(body)?;
    Ok(encoder.finish()?)
}

fn insert_gzip_extra(bytes: &mut Vec<u8>, extra: &[u8]) -> Result<(), Box<dyn Error>> {
    let flags = bytes.get_mut(3).ok_or("GZIP fixture flags absent")?;
    *flags |= 0x04;
    let length = u16::try_from(extra.len())?.to_le_bytes();
    let mut field = Vec::with_capacity(2 + extra.len());
    field.extend_from_slice(&length);
    field.extend_from_slice(extra);
    bytes.splice(10..10, field);
    Ok(())
}

fn repeat_first_central_filename(bytes: &mut [u8]) -> Result<(), Box<dyn Error>> {
    let first = central_header_offset(bytes, 0)?;
    let second = central_header_offset(bytes, first + 4)?;
    let name = central_filename(bytes, first)?;
    if central_filename(bytes, second)?.len() != name.len() {
        return Err("duplicate ZIP fixture needs equal-length entry names".into());
    }
    bytes
        .get_mut(second + 46..second + 46 + name.len())
        .ok_or("ZIP central filename absent")?
        .copy_from_slice(&name);
    Ok(())
}

fn central_header_offset(bytes: &[u8], from: usize) -> Result<usize, Box<dyn Error>> {
    let position = bytes
        .get(from..)
        .ok_or("ZIP fixture omitted central header")?
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .ok_or("ZIP fixture omitted central header")?;
    Ok(from + position)
}

fn central_filename(bytes: &[u8], header: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    let length = bytes
        .get(header + 28..header + 30)
        .ok_or("ZIP central filename length absent")?;
    let length = usize::from(u16::from_le_bytes([length[0], length[1]]));
    Ok(bytes
        .get(header + 46..header + 46 + length)
        .ok_or("ZIP central filename absent")?
        .to_vec())
}

fn set_encryption_flags(bytes: &mut [u8]) -> Result<(), Box<dyn Error>> {
    let local = bytes
        .windows(4)
        .position(|window| window == b"PK\x03\x04")
        .ok_or("ZIP fixture omitted local header")?;
    let central = bytes
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .ok_or("ZIP fixture omitted central header")?;
    set_flag(bytes, local + 6)?;
    set_flag(bytes, central + 8)?;
    Ok(())
}

fn set_compression_method(bytes: &mut [u8], method: u16) -> Result<(), Box<dyn Error>> {
    let local = bytes
        .windows(4)
        .position(|window| window == b"PK\x03\x04")
        .ok_or("ZIP fixture omitted local header")?;
    let central = bytes
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .ok_or("ZIP fixture omitted central header")?;
    bytes
        .get_mut(local + 8..local + 10)
        .ok_or("ZIP local compression method absent")?
        .copy_from_slice(&method.to_le_bytes());
    bytes
        .get_mut(central + 10..central + 12)
        .ok_or("ZIP central compression method absent")?
        .copy_from_slice(&method.to_le_bytes());
    Ok(())
}

fn set_uncompressed_size(bytes: &mut [u8], size: u32) -> Result<(), Box<dyn Error>> {
    let local = bytes
        .windows(4)
        .position(|window| window == b"PK\x03\x04")
        .ok_or("ZIP fixture omitted local header")?;
    let central = bytes
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .ok_or("ZIP fixture omitted central header")?;
    bytes
        .get_mut(local + 22..local + 26)
        .ok_or("ZIP local uncompressed size absent")?
        .copy_from_slice(&size.to_le_bytes());
    bytes
        .get_mut(central + 24..central + 28)
        .ok_or("ZIP central uncompressed size absent")?
        .copy_from_slice(&size.to_le_bytes());
    Ok(())
}

fn set_unix_mode(bytes: &mut [u8], mode: u32) -> Result<(), Box<dyn Error>> {
    let central = bytes
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .ok_or("ZIP fixture omitted central header")?;
    *bytes
        .get_mut(central + 5)
        .ok_or("ZIP creator system absent")? = 3;
    bytes
        .get_mut(central + 38..central + 42)
        .ok_or("ZIP external attributes absent")?
        .copy_from_slice(&(mode << 16).to_le_bytes());
    Ok(())
}

fn set_legacy_filename(bytes: &mut [u8], name: &[u8]) -> Result<(), Box<dyn Error>> {
    let local = bytes
        .windows(4)
        .position(|window| window == b"PK\x03\x04")
        .ok_or("ZIP fixture omitted local header")?;
    let central = bytes
        .windows(4)
        .position(|window| window == b"PK\x01\x02")
        .ok_or("ZIP fixture omitted central header")?;
    clear_utf8_flag(bytes, local + 6)?;
    clear_utf8_flag(bytes, central + 8)?;
    bytes
        .get_mut(local + 30..local + 30 + name.len())
        .ok_or("ZIP local filename absent")?
        .copy_from_slice(name);
    bytes
        .get_mut(central + 46..central + 46 + name.len())
        .ok_or("ZIP central filename absent")?
        .copy_from_slice(name);
    Ok(())
}

fn clear_utf8_flag(bytes: &mut [u8], offset: usize) -> Result<(), Box<dyn Error>> {
    let flags = bytes
        .get_mut(offset..offset + 2)
        .ok_or("ZIP flag offset absent")?;
    let value = u16::from_le_bytes([flags[0], flags[1]]) & !(1 << 11);
    flags.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn set_flag(bytes: &mut [u8], offset: usize) -> Result<(), Box<dyn Error>> {
    let flags = bytes
        .get_mut(offset..offset + 2)
        .ok_or("ZIP flag offset absent")?;
    let value = u16::from_le_bytes([flags[0], flags[1]]) | 1;
    flags.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[derive(Clone)]
pub struct MemorySource {
    bytes: Vec<u8>,
    byte_length: NonZeroU64,
    media_type: &'static str,
}

impl MemorySource {
    pub fn new(bytes: Vec<u8>, media_type: &'static str) -> Result<Self, Box<dyn Error>> {
        let byte_length = NonZeroU64::new(u64::try_from(bytes.len())?)
            .ok_or("fixture source must be nonempty")?;
        Ok(Self {
            bytes,
            byte_length,
            media_type,
        })
    }

    pub fn unknown(bytes: Vec<u8>) -> Result<Self, Box<dyn Error>> {
        Self::new(bytes, "application/octet-stream")
    }

    pub fn file_use(&self) -> Result<FileUse, Box<dyn Error>> {
        Ok(FileUse::new(
            self.digest(),
            self.byte_length,
            AttachmentKind::File,
            DeclaredMediaType::try_new(self.media_type)?,
            None,
        ))
    }
}

impl VerifiedBlobSource for MemorySource {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes([0x41; 32])
    }

    fn byte_length(&self) -> NonZeroU64 {
        self.byte_length
    }

    fn read_range(&self, offset: u64, length: NonZeroU64) -> SourceReadFuture<'_> {
        let outcome = usize::try_from(offset)
            .ok()
            .and_then(|start| {
                usize::try_from(length.get())
                    .ok()
                    .and_then(|length| start.checked_add(length).map(|end| (start, end)))
            })
            .and_then(|(start, end)| self.bytes.get(start..end).map(<[u8]>::to_vec))
            .ok_or(SourceReadError::RangeOutOfBounds);
        Box::pin(async move { outcome })
    }
}
