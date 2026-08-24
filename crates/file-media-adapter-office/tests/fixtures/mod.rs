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
const DOCX_MAIN_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml";
const DOCM_MAIN_CONTENT_TYPE: &str = "application/vnd.ms-word.document.macroEnabled.main+xml";
const XLSX_MAIN_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml";
const PPTX_MAIN_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml";

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
            r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{expected_text}</w:t></w:r></w:p></w:body></w:document>"#
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
                    b"<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet r:id=\"rId1\"/></sheets></workbook>".as_slice(),
                    EntryKind::File,
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    b"<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/><Relationship Id=\"rIdShared\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings\" Target=\"sharedStrings.xml\"/></Relationships>".as_slice(),
                    EntryKind::File,
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c t="s"><v>0</v></c></row></sheetData></worksheet>"#.as_slice(),
                    EntryKind::File,
                ),
                ("xl/sharedStrings.xml", shared.as_bytes(), EntryKind::File),
            ],
        )
    }

    pub fn adjacent_shared_strings_xlsx() -> Result<Self, Box<dyn Error>> {
        let shared = b"<?xml version=\"1.0\"?><sst><si><t>foo</t></si><si><t>bar</t></si></sst>";
        Self::package(
            XLSX_MEDIA_TYPE,
            "foo\nbar\n",
            &[
                (
                    "xl/workbook.xml",
                    b"<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet r:id=\"rId1\"/></sheets></workbook>".as_slice(),
                    EntryKind::File,
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    b"<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/><Relationship Id=\"rIdShared\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings\" Target=\"sharedStrings.xml\"/></Relationships>".as_slice(),
                    EntryKind::File,
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    br#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row><c t="s"><v>0</v></c><c t="s"><v>1</v></c></row></sheetData></worksheet>"#.as_slice(),
                    EntryKind::File,
                ),
                ("xl/sharedStrings.xml", shared.as_slice(), EntryKind::File),
            ],
        )
    }

    pub fn pptx() -> Result<Self, Box<dyn Error>> {
        let expected_text = "generated pptx text";
        let slide = format!(
            r#"<?xml version="1.0"?><p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p><a:r><a:t>{expected_text}</a:t></a:r></a:p></p:sld>"#
        );
        let presentation = br#"<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId r:id="rId1"/></p:sldIdLst></p:presentation>"#;
        let relationships = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/></Relationships>"#;
        Self::package(
            PPTX_MEDIA_TYPE,
            expected_text,
            &[
                (
                    "ppt/presentation.xml",
                    presentation.as_slice(),
                    EntryKind::File,
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    relationships.as_slice(),
                    EntryKind::File,
                ),
                ("ppt/slides/slide1.xml", slide.as_bytes(), EntryKind::File),
            ],
        )
    }

    pub fn reordered_pptx() -> Result<Self, Box<dyn Error>> {
        let presentation = br#"<?xml version="1.0"?><p:presentation xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId r:id="rId2"/><p:sldId r:id="rId1"/></p:sldIdLst></p:presentation>"#;
        let relationships = br#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide2.xml"/></Relationships>"#;
        let first = br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p><a:t>first</a:t></a:p></p:sld>"#;
        let second = br#"<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:p><a:t>second</a:t></a:p></p:sld>"#;
        Self::package(
            PPTX_MEDIA_TYPE,
            "second\nfirst\n",
            &[
                (
                    "ppt/presentation.xml",
                    presentation.as_slice(),
                    EntryKind::File,
                ),
                (
                    "ppt/_rels/presentation.xml.rels",
                    relationships.as_slice(),
                    EntryKind::File,
                ),
                ("ppt/slides/slide1.xml", first.as_slice(), EntryKind::File),
                ("ppt/slides/slide2.xml", second.as_slice(), EntryKind::File),
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

    pub fn macro_enabled_docx() -> Result<Self, Box<dyn Error>> {
        let document = br#"<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"/>"#;
        let content_types = content_types_xml(&[("word/document.xml", DOCM_MAIN_CONTENT_TYPE)]);
        let mut fixture = Self::package_with_content_types(
            DOCX_MEDIA_TYPE,
            "",
            content_types.as_bytes(),
            &[("word/document.xml", document.as_slice(), EntryKind::File)],
        )?;
        fixture.expected_reason = Some("malformed_office_container");
        Ok(fixture)
    }

    pub fn vba_part_in_macro_free_docx() -> Result<Self, Box<dyn Error>> {
        let document = br#"<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"/>"#;
        let mut fixture = Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[
                ("word/document.xml", document.as_slice(), EntryKind::File),
                ("word/vbaProject.bin", b"vba".as_slice(), EntryKind::File),
            ],
        )?;
        fixture.expected_reason = Some("malformed_office_container");
        Ok(fixture)
    }

    pub fn mixed_docx_xlsx() -> Result<Self, Box<dyn Error>> {
        let document = br#"<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"/>"#;
        let workbook = b"<?xml version=\"1.0\"?><workbook/>";
        let content_types = content_types_xml(&[
            ("word/document.xml", DOCX_MAIN_CONTENT_TYPE),
            ("xl/workbook.xml", XLSX_MAIN_CONTENT_TYPE),
        ]);
        Self::package_with_content_types(
            DOCX_MEDIA_TYPE,
            "",
            content_types.as_bytes(),
            &[
                ("word/document.xml", document.as_slice(), EntryKind::File),
                ("xl/workbook.xml", workbook.as_slice(), EntryKind::File),
            ],
        )
    }

    pub fn large_opaque_part_docx() -> Result<Self, Box<dyn Error>> {
        let document = br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:t>bounded text</w:t></w:document>"#;
        let image = vec![0_u8; 5 * 1024 * 1024];
        Self::package(
            DOCX_MEDIA_TYPE,
            "bounded text",
            &[
                ("word/document.xml", document.as_slice(), EntryKind::File),
                ("word/media/photo.jpg", &image, EntryKind::File),
            ],
        )
    }

    pub fn empty_xml_docx() -> Result<Self, Box<dyn Error>> {
        Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[("word/document.xml", b"".as_slice(), EntryKind::File)],
        )
    }

    pub fn multiple_roots_docx() -> Result<Self, Box<dyn Error>> {
        Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[(
                "word/document.xml",
                br#"<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"/><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"/>"#.as_slice(),
                EntryKind::File,
            )],
        )
    }

    pub fn duplicate_document_part_docx() -> Result<Self, Box<dyn Error>> {
        let document = br#"<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"/>"#;
        let mut fixture = Self::package(
            DOCX_MEDIA_TYPE,
            "",
            &[
                ("word/document.xml", document.as_slice(), EntryKind::File),
                ("word/document2.xm", document.as_slice(), EntryKind::File),
            ],
        )?;
        replace_equal_length_name(
            &mut fixture.bytes,
            b"word/document2.xm",
            b"word/document.xml",
        )?;
        fixture.expected_reason = Some("malformed_office_container");
        Ok(fixture)
    }

    pub fn zip_slip_docx() -> Result<Self, Box<dyn Error>> {
        let document = br#"<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"/>"#;
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
        let document = br#"<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"/>"#;
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
        let document = br#"<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"/>"#;
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
        let content_types = content_types_xml(&[("word/document.xml", DOCX_MAIN_CONTENT_TYPE)]);
        writer.write_all(content_types.as_bytes())?;
        writer.start_file("word/document.xml", options)?;
        writer.write_all(
            br#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"/>"#,
        )?;
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
                br#"<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:t>unterminated"#.as_slice(),
                EntryKind::File,
            )],
        )
    }

    pub fn output_bomb_docx() -> Result<Self, Box<dyn Error>> {
        let text = "x".repeat(768 * 1024 + 1);
        let document = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:p><w:t>{text}</w:t></w:p></w:document>"#
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
        let content_types = content_types_xml(&[(
            main_part_for(media_type)?,
            main_content_type_for(media_type)?,
        )]);
        Self::package_with_content_types(
            media_type,
            expected_text,
            content_types.as_bytes(),
            entries,
        )
    }

    fn package_with_content_types(
        media_type: &'static str,
        expected_text: &'static str,
        content_types: &[u8],
        entries: &[(&str, &[u8], EntryKind)],
    ) -> Result<Self, Box<dyn Error>> {
        let cursor = std::io::Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let file_options =
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        writer.start_file(CONTENT_TYPES_NAME, file_options)?;
        writer.write_all(content_types)?;
        writer.start_file("_rels/.rels", file_options)?;
        let main_part = main_part_for(media_type)?;
        writer.write_all(
            format!(
                r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="{main_part}"/></Relationships>"#
            )
            .as_bytes(),
        )?;
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
            expected_entries: entries.len() + 2,
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

    pub fn into_unknown_source(self) -> Result<MemorySource, Box<dyn Error>> {
        MemorySource::unknown(self.bytes)
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

fn main_part_for(media_type: &str) -> Result<&'static str, Box<dyn Error>> {
    match media_type {
        DOCX_MEDIA_TYPE => Ok("word/document.xml"),
        XLSX_MEDIA_TYPE => Ok("xl/workbook.xml"),
        PPTX_MEDIA_TYPE => Ok("ppt/presentation.xml"),
        _ => Err("unsupported Office fixture media type".into()),
    }
}

fn main_content_type_for(media_type: &str) -> Result<&'static str, Box<dyn Error>> {
    match media_type {
        DOCX_MEDIA_TYPE => Ok(DOCX_MAIN_CONTENT_TYPE),
        XLSX_MEDIA_TYPE => Ok(XLSX_MAIN_CONTENT_TYPE),
        PPTX_MEDIA_TYPE => Ok(PPTX_MAIN_CONTENT_TYPE),
        _ => Err("unsupported Office fixture media type".into()),
    }
}

fn content_types_xml(overrides: &[(&str, &str)]) -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">"#,
    );
    for (part_name, content_type) in overrides {
        xml.push_str(&format!(
            r#"<Override PartName="/{part_name}" ContentType="{content_type}"/>"#
        ));
    }
    xml.push_str("</Types>");
    xml
}

fn replace_equal_length_name(
    bytes: &mut [u8],
    original: &[u8],
    replacement: &[u8],
) -> Result<(), Box<dyn Error>> {
    if original.len() != replacement.len() {
        return Err("ZIP fixture replacement names differ in length".into());
    }
    let first = bytes
        .windows(original.len())
        .position(|window| window == original)
        .ok_or("ZIP fixture omitted duplicate local name")?;
    bytes[first..first + replacement.len()].copy_from_slice(replacement);
    let second_relative = bytes[first + replacement.len()..]
        .windows(original.len())
        .position(|window| window == original)
        .ok_or("ZIP fixture omitted duplicate central name")?;
    let second = first + replacement.len() + second_relative;
    bytes[second..second + replacement.len()].copy_from_slice(replacement);
    Ok(())
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
