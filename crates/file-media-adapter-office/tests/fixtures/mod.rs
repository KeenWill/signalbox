use std::{error::Error, io::Write, num::NonZeroU64};

use signalbox_file_media_runtime::{
    AttachmentKind, DeclaredMediaType, FileDigest, FileUse, SourceReadError, SourceReadFuture,
    VerifiedBlobSource,
};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

const DOCX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const XLSX_MEDIA_TYPE: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";
const PPTX_MEDIA_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const CONTENT_TYPES_XML: &[u8] = br#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"/>"#;

pub struct OfficeFixture {
    bytes: Vec<u8>,
    media_type: &'static str,
    expected_text: &'static str,
    expected_entries: usize,
    expected_format: &'static str,
    expected_reason: Option<&'static str>,
}

impl OfficeFixture {
    pub fn docx() -> Result<Self, Box<dyn Error>> {
        let expected_text = "generated docx text";
        let document = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="urn:w"><w:body><w:p><w:r><w:t>{expected_text}</w:t></w:r></w:p></w:body></w:document>"#
        );
        Self::package(
            DOCX_MEDIA_TYPE,
            expected_text,
            &[("word/document.xml", document.as_bytes(), EntryKind::File)],
        )
    }

    pub fn xlsx() -> Result<Self, Box<dyn Error>> {
        let expected_text = "generated xlsx text";
        let shared = format!(
            r#"<?xml version="1.0"?><sst xmlns="urn:s"><si><t>{expected_text}</t></si></sst>"#
        );
        Self::package(
            XLSX_MEDIA_TYPE,
            expected_text,
            &[
                (
                    "xl/workbook.xml",
                    b"<?xml version=\"1.0\"?><workbook/>".as_slice(),
                    EntryKind::File,
                ),
                ("xl/sharedStrings.xml", shared.as_bytes(), EntryKind::File),
            ],
        )
    }

    pub fn pptx() -> Result<Self, Box<dyn Error>> {
        let expected_text = "generated pptx text";
        let slide = format!(
            r#"<?xml version="1.0"?><p:sld xmlns:p="urn:p" xmlns:a="urn:a"><a:p><a:r><a:t>{expected_text}</a:t></a:r></a:p></p:sld>"#
        );
        Self::package(
            PPTX_MEDIA_TYPE,
            expected_text,
            &[
                (
                    "ppt/presentation.xml",
                    b"<?xml version=\"1.0\"?><presentation/>".as_slice(),
                    EntryKind::File,
                ),
                ("ppt/slides/slide1.xml", slide.as_bytes(), EntryKind::File),
            ],
        )
    }

    pub fn truncated_docx() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::docx()?;
        let retained = fixture.bytes.len().saturating_sub(12);
        fixture.bytes.truncate(retained);
        fixture.expected_reason = Some("malformed_office_container");
        Ok(fixture)
    }

    pub fn locked_docx() -> Result<Self, Box<dyn Error>> {
        let mut fixture = Self::docx()?;
        set_encryption_flags(&mut fixture.bytes)?;
        Ok(fixture)
    }

    pub fn zip_slip_docx() -> Result<Self, Box<dyn Error>> {
        let document = b"<?xml version=\"1.0\"?><w:document/>";
        let mut fixture = Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[
                ("word/document.xml", document.as_slice(), EntryKind::File),
                ("../../host", b"host".as_slice(), EntryKind::File),
            ],
        )?;
        fixture.expected_reason = Some("hostile_entry_name");
        Ok(fixture)
    }

    pub fn symlink_docx() -> Result<Self, Box<dyn Error>> {
        let document = b"<?xml version=\"1.0\"?><w:document/>";
        let mut fixture = Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[
                ("word/document.xml", document.as_slice(), EntryKind::File),
                ("word/link", b"../../host".as_slice(), EntryKind::Symlink),
            ],
        )?;
        fixture.expected_reason = Some("symlink_entry");
        Ok(fixture)
    }

    pub fn recursive_docx() -> Result<Self, Box<dyn Error>> {
        let document = b"<?xml version=\"1.0\"?><w:document/>";
        let mut fixture = Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[
                ("word/document.xml", document.as_slice(), EntryKind::File),
                (
                    "word/embeddings/inner.docx",
                    b"PK".as_slice(),
                    EntryKind::File,
                ),
            ],
        )?;
        fixture.expected_reason = Some("recursive_container");
        Ok(fixture)
    }

    pub fn expansion_bomb_docx() -> Result<Self, Box<dyn Error>> {
        let document = vec![b' '; 4 * 1024 * 1024 + 1];
        let mut fixture = Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[("word/document.xml", &document, EntryKind::File)],
        )?;
        fixture.expected_reason = Some("decompressed_size_limit");
        Ok(fixture)
    }

    pub fn entry_count_bomb_docx() -> Result<Self, Box<dyn Error>> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        writer.start_file(CONTENT_TYPES_NAME, options)?;
        writer.write_all(CONTENT_TYPES_XML)?;
        writer.start_file("word/document.xml", options)?;
        writer.write_all(b"<?xml version=\"1.0\"?><w:document/>")?;
        for index in 0..9_999 {
            writer.start_file(format!("word/empty-{index}.xml"), options)?;
        }
        Ok(Self {
            bytes: writer.finish()?.into_inner(),
            media_type: DOCX_MEDIA_TYPE,
            expected_text: "",
            expected_entries: 10_001,
            expected_format: "docx",
            expected_reason: Some("entry_count_limit"),
        })
    }

    pub fn malformed_xml_docx() -> Result<Self, Box<dyn Error>> {
        Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[(
                "word/document.xml",
                b"<?xml version=\"1.0\"?><w:document><w:t>unterminated".as_slice(),
                EntryKind::File,
            )],
        )
    }

    pub fn output_bomb_docx() -> Result<Self, Box<dyn Error>> {
        let text = "x".repeat(768 * 1024 + 1);
        let document = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="urn:w"><w:p><w:t>{text}</w:t></w:p></w:document>"#
        );
        Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[("word/document.xml", document.as_bytes(), EntryKind::File)],
        )
    }

    fn package(
        media_type: &'static str,
        expected_text: &'static str,
        entries: &[(&str, &[u8], EntryKind)],
    ) -> Result<Self, Box<dyn Error>> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let file_options =
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file(CONTENT_TYPES_NAME, file_options)?;
        writer.write_all(CONTENT_TYPES_XML)?;
        for (name, body, kind) in entries {
            match kind {
                EntryKind::File => {
                    writer.start_file(*name, file_options)?;
                    writer.write_all(body)?;
                }
                EntryKind::Symlink => {
                    writer.add_symlink(*name, std::str::from_utf8(body)?, file_options)?;
                }
            }
        }
        Ok(Self {
            bytes: writer.finish()?.into_inner(),
            media_type,
            expected_text,
            expected_entries: entries.len() + 1,
            expected_format: format_for(media_type)?,
            expected_reason: None,
        })
    }

    pub const fn expected_text(&self) -> &'static str {
        self.expected_text
    }

    pub const fn expected_entries(&self) -> usize {
        self.expected_entries
    }

    pub const fn expected_format(&self) -> &'static str {
        self.expected_format
    }

    pub fn expected_reason(&self) -> Result<&'static str, Box<dyn Error>> {
        self.expected_reason
            .ok_or_else(|| "fixture has no expected failure reason".into())
    }

    pub fn into_source(self) -> Result<MemorySource, Box<dyn Error>> {
        MemorySource::new(self.bytes, self.media_type)
    }
}

const CONTENT_TYPES_NAME: &str = "[Content_Types].xml";

#[derive(Clone, Copy)]
enum EntryKind {
    File,
    Symlink,
}

fn format_for(media_type: &str) -> Result<&'static str, Box<dyn Error>> {
    match media_type {
        DOCX_MEDIA_TYPE => Ok("docx"),
        XLSX_MEDIA_TYPE => Ok("xlsx"),
        PPTX_MEDIA_TYPE => Ok("pptx"),
        _ => Err("unsupported Office fixture media type".into()),
    }
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
            AttachmentKind::Document,
            DeclaredMediaType::try_new(self.media_type)?,
            None,
        ))
    }
}

impl VerifiedBlobSource for MemorySource {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes([0x4f; 32])
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
