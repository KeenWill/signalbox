use std::{error::Error, num::NonZeroU64};

use signalbox_file_media_runtime::{
    AttachmentKind, DeclaredMediaType, FileDigest, FileUse, SourceReadError, SourceReadFuture,
    VerifiedBlobSource,
};

const FIXTURE_TEXT: &str = "generated SVG text";
const FIXTURE_WIDTH: f64 = 320.0;
const FIXTURE_HEIGHT: f64 = 200.0;
const FIXTURE_VIEW_BOX: [f64; 4] = [0.0, 0.0, 320.0, 200.0];

pub struct SvgFixture {
    bytes: Vec<u8>,
}

impl SvgFixture {
    pub fn raw(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.to_vec(),
        }
    }

    pub fn ordinary() -> Self {
        Self::from_body(&format!(
            r#"<text x="1" y="2">{FIXTURE_TEXT}</text><path d="M0 0 L1 1"/>"#,
        ))
    }

    pub fn truncated() -> Self {
        Self {
            bytes: br#"<svg xmlns="http://www.w3.org/2000/svg"><text>unfinished"#.to_vec(),
        }
    }

    pub fn invalid_utf8() -> Self {
        let mut bytes = br#"<svg xmlns="http://www.w3.org/2000/svg"><text>"#.to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(b"</text></svg>");
        Self { bytes }
    }

    pub fn entity_bomb() -> Self {
        Self {
            bytes: br#"<!DOCTYPE svg [<!ENTITY a "boom">]><svg xmlns="http://www.w3.org/2000/svg"><text>&a;</text></svg>"#.to_vec(),
        }
    }

    pub fn script() -> Self {
        Self::from_body("<script>host()</script>")
    }

    pub fn external_image() -> Self {
        Self::from_body(r#"<image href="https://example.invalid/host.png"/>"#)
    }

    pub fn nested_svg() -> Self {
        Self::from_body(r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#)
    }

    pub fn excessive_elements() -> Self {
        let mut body = String::new();
        for _ in 0..10_000 {
            body.push_str("<path/>");
        }
        Self::from_body(&body)
    }

    pub fn output_bomb() -> Self {
        Self::from_body(&format!("<text>{}</text>", "x".repeat(128 * 1024 + 1)))
    }

    pub fn oversized_source() -> Self {
        let mut bytes = br#"<svg xmlns="http://www.w3.org/2000/svg">"#.to_vec();
        bytes.resize(256 * 1024 + 1, b' ');
        bytes.extend_from_slice(b"</svg>");
        Self { bytes }
    }

    pub fn malformed_dimension() -> Self {
        Self {
            bytes: br#"<svg xmlns="http://www.w3.org/2000/svg" width="calc()"></svg>"#.to_vec(),
        }
    }

    pub const fn expected_text(&self) -> &'static str {
        FIXTURE_TEXT
    }

    pub const fn expected_elements(&self) -> usize {
        3
    }

    pub const fn expected_width(&self) -> f64 {
        FIXTURE_WIDTH
    }

    pub const fn expected_height(&self) -> f64 {
        FIXTURE_HEIGHT
    }

    pub const fn expected_view_box(&self) -> [f64; 4] {
        FIXTURE_VIEW_BOX
    }

    pub fn into_source(self) -> Result<MemorySource, Box<dyn Error>> {
        MemorySource::new(self.bytes)
    }

    fn from_body(body: &str) -> Self {
        Self {
            bytes: format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><svg xmlns="http://www.w3.org/2000/svg" width="320" height="200px" viewBox="0 0 320 200">{body}</svg>"#,
            )
            .into_bytes(),
        }
    }
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

    pub fn unknown(bytes: Vec<u8>) -> Result<Self, Box<dyn Error>> {
        Self::new(bytes)
    }

    pub fn file_use(&self) -> Result<FileUse, Box<dyn Error>> {
        self.file_use_as("image/svg+xml")
    }

    pub fn file_use_as(&self, media_type: &str) -> Result<FileUse, Box<dyn Error>> {
        Ok(FileUse::new(
            self.digest(),
            self.byte_length,
            AttachmentKind::Image,
            DeclaredMediaType::try_new(media_type)?,
            None,
        ))
    }
}

impl VerifiedBlobSource for MemorySource {
    fn digest(&self) -> FileDigest {
        FileDigest::from_bytes([0x53; 32])
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
