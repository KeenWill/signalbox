use std::{error::Error, num::NonZeroU64};

use lopdf::{Document, Object, Stream, content::Content, content::Operation, dictionary};
use signalbox_file_media_runtime::{
    AttachmentKind, DeclaredMediaType, FileDigest, FileUse, SourceReadError, SourceReadFuture,
    VerifiedBlobSource,
};

const FIXTURE_TEXT: &str = "generated PDF text";
const FIXTURE_PAGE_COUNT: usize = 1;
const READ_SOURCE_LIMIT: u64 = 8 * 1024 * 1024;

pub struct PdfFixture {
    bytes: Vec<u8>,
}

impl PdfFixture {
    pub fn ordinary() -> Result<Self, Box<dyn Error>> {
        Self::with_text(FIXTURE_TEXT)
    }

    pub fn with_text(text: &str) -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: build_pdf(text.as_bytes(), ContentShape::Ordinary)?,
        })
    }

    pub fn compressed_bomb() -> Result<Self, Box<dyn Error>> {
        let content = vec![b' '; 2 * 1024 * 1024];
        Ok(Self {
            bytes: build_pdf(&content, ContentShape::CompressedRaw)?,
        })
    }

    pub fn recursive_content_reference() -> Result<Self, Box<dyn Error>> {
        Ok(Self {
            bytes: build_pdf(FIXTURE_TEXT.as_bytes(), ContentShape::RecursiveReference)?,
        })
    }

    pub fn locked() -> Result<Self, Box<dyn Error>> {
        let mut bytes = Self::ordinary()?.bytes;
        let insertion = bytes
            .windows(b"<<".len())
            .rposition(|window| window == b"<<")
            .ok_or("generated PDF omitted trailer dictionary")?
            + 2;
        bytes.splice(
            insertion..insertion,
            b" /Encrypt << /Filter /Standard /V 1 /R 2 /P -4 >>"
                .iter()
                .copied(),
        );
        Ok(Self { bytes })
    }

    pub fn trailer_encrypt_comment() -> Result<Self, Box<dyn Error>> {
        let mut bytes = Self::ordinary()?.bytes;
        let insertion = bytes
            .windows(b"<<".len())
            .rposition(|window| window == b"<<")
            .ok_or("generated PDF omitted trailer dictionary")?;
        bytes.splice(insertion..insertion, b"% /Encrypt\n".iter().copied());
        Ok(Self { bytes })
    }

    pub fn catalog_version_override() -> Result<Self, Box<dyn Error>> {
        let mut document = Document::load_mem(&Self::ordinary()?.bytes)?;
        document
            .catalog_mut()?
            .set("Version", Object::Name(b"1.7".to_vec()));
        let mut bytes = Vec::new();
        document.save_to(&mut bytes)?;
        Ok(Self { bytes })
    }

    pub fn malformed_large() -> Result<Self, Box<dyn Error>> {
        let mut bytes = b"%PDF-1.5\n".to_vec();
        bytes.resize(300 * 1024, b' ');
        bytes.extend_from_slice(
            b"xref\n0 1\n0000000000 65535 f\ntrailer\n<< /Size 1 /Root 1 0 R >>\nstartxref\n9\n%%EOF\n",
        );
        Ok(Self { bytes })
    }

    pub fn over_source_limit() -> Result<Self, Box<dyn Error>> {
        let mut bytes = Self::ordinary()?.bytes;
        let insertion = bytes
            .windows(b"startxref".len())
            .rposition(|window| window == b"startxref")
            .ok_or("generated PDF omitted startxref")?;
        let padding = READ_SOURCE_LIMIT as usize + 1 - bytes.len();
        bytes.splice(insertion..insertion, std::iter::repeat_n(b' ', padding));
        Ok(Self { bytes })
    }

    pub fn truncated() -> Result<Self, Box<dyn Error>> {
        let mut bytes = Self::ordinary()?.bytes;
        let retained = bytes.len().saturating_sub(16);
        bytes.truncate(retained);
        Ok(Self { bytes })
    }

    pub fn expected_text(&self) -> &'static str {
        FIXTURE_TEXT
    }

    pub const fn expected_page_count(&self) -> usize {
        FIXTURE_PAGE_COUNT
    }

    pub const fn expected_source_limit(&self) -> u64 {
        READ_SOURCE_LIMIT
    }

    pub const fn expected_version_override(&self) -> &'static str {
        "1.7"
    }

    pub fn into_source(self) -> Result<MemorySource, Box<dyn Error>> {
        MemorySource::new(self.bytes)
    }
}

enum ContentShape {
    Ordinary,
    CompressedRaw,
    RecursiveReference,
}

fn build_pdf(content_bytes: &[u8], shape: ContentShape) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut document = Document::with_version("1.5");
    let pages_id = document.new_object_id();
    let font_id = document.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Courier",
    });
    let resources_id = document.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });
    let content_id = match shape {
        ContentShape::Ordinary => {
            let content = Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec!["F1".into(), 12.into()]),
                    Operation::new("Tj", vec![Object::string_literal(content_bytes)]),
                    Operation::new("ET", vec![]),
                ],
            };
            document.add_object(Stream::new(dictionary! {}, content.encode()?))
        }
        ContentShape::CompressedRaw => {
            let mut stream = Stream::new(dictionary! {}, content_bytes.to_vec());
            stream.compress()?;
            document.add_object(stream)
        }
        ContentShape::RecursiveReference => {
            let recursive_id = document.new_object_id();
            document
                .objects
                .insert(recursive_id, Object::Reference(recursive_id));
            recursive_id
        }
    };
    let page_id = document.add_object(dictionary! {
        "Type" => "Page",
        "Parent" => pages_id,
        "Contents" => content_id,
    });
    document.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![page_id.into()],
            "Count" => 1,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        }),
    );
    let catalog_id = document.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    document.trailer.set("Root", catalog_id);
    let mut bytes = Vec::new();
    document.save_to(&mut bytes)?;
    Ok(bytes)
}

#[derive(Clone)]
pub struct MemorySource {
    bytes: Vec<u8>,
    byte_length: NonZeroU64,
}

impl MemorySource {
    pub fn new(bytes: Vec<u8>) -> Result<Self, Box<dyn Error>> {
        let byte_length = NonZeroU64::new(u64::try_from(bytes.len())?)
            .ok_or("fixture source must be nonempty")?;
        Ok(Self { bytes, byte_length })
    }

    pub fn file_use(&self) -> Result<FileUse, Box<dyn Error>> {
        self.file_use_as("application/pdf")
    }

    pub fn file_use_as(&self, media_type: &str) -> Result<FileUse, Box<dyn Error>> {
        Ok(FileUse::new(
            self.digest(),
            self.byte_length,
            AttachmentKind::Document,
            DeclaredMediaType::try_new(media_type)?,
            None,
        ))
    }
}

impl VerifiedBlobSource for MemorySource {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes([0x50; 32])
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
