use signalbox_domain::{
    ImportedJsonNumber, ImportedMediaSource, ImportedMessageContentAbsence,
    ImportedSourceAttestation, ImportedSourceMetadata, ImportedStructuredObjectMember,
    ImportedStructuredValue, ImportedText, ImportedToolResultBlock, ImportedToolResultValue,
    ImportedTranscriptContent,
};

const ENCODING_VERSION: u8 = 1;
const STRUCTURED_PAYLOAD: u8 = 0;
const CONTENT_PAYLOAD: u8 = 1;
const SOURCE_METADATA_PAYLOAD: u8 = 2;
const MAX_CONTAINER_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImportedConversationEncodingFailure {
    LengthOutOfRange,
    UnexpectedEnd,
    TrailingBytes,
    UnsupportedVersion(u8),
    UnexpectedPayloadKind { expected: u8, actual: u8 },
    UnsupportedTag { kind: &'static str, value: u8 },
    InvalidUtf8(&'static str),
    InvalidJsonNumber,
    ContainerDepthExceeded,
}

pub(crate) fn encode_structured(
    value: &ImportedStructuredValue,
) -> Result<Vec<u8>, ImportedConversationEncodingFailure> {
    let mut bytes = encoding_header(STRUCTURED_PAYLOAD);
    encode_structured_value(&mut bytes, value, 0)?;
    Ok(bytes)
}

pub(crate) fn decode_structured(
    bytes: &[u8],
) -> Result<ImportedStructuredValue, ImportedConversationEncodingFailure> {
    let mut decoder = Decoder::new(bytes, STRUCTURED_PAYLOAD)?;
    let value = decoder.structured(0)?;
    decoder.finish()?;
    Ok(value)
}

pub(crate) fn encode_content(
    content: &ImportedTranscriptContent,
) -> Result<Vec<u8>, ImportedConversationEncodingFailure> {
    let mut bytes = encoding_header(CONTENT_PAYLOAD);
    match content {
        ImportedTranscriptContent::SourceEvent { source_type } => {
            push(&mut bytes, 0);
            encode_attestation(&mut bytes, source_type, encode_text)?;
        }
        ImportedTranscriptContent::SourceMessageBlock { source_type } => {
            push(&mut bytes, 8);
            encode_attestation(&mut bytes, source_type, encode_text)?;
        }
        ImportedTranscriptContent::Text(value) => {
            push(&mut bytes, 1);
            encode_attestation(&mut bytes, value, encode_text)?;
        }
        ImportedTranscriptContent::ToolCall {
            source_call_id,
            name,
            input,
            caller,
        } => {
            push(&mut bytes, 2);
            encode_attestation(&mut bytes, source_call_id, encode_text)?;
            encode_attestation(&mut bytes, name, encode_text)?;
            encode_attestation(&mut bytes, input, |bytes, value| {
                encode_structured_value(bytes, value, 0)
            })?;
            encode_attestation(&mut bytes, caller, |bytes, value| {
                encode_structured_value(bytes, value, 0)
            })?;
        }
        ImportedTranscriptContent::ToolResult {
            source_call_id,
            content,
            is_error,
        } => {
            push(&mut bytes, 3);
            encode_attestation(&mut bytes, source_call_id, encode_text)?;
            encode_attestation(&mut bytes, content, encode_tool_result_value)?;
            encode_attestation(&mut bytes, is_error, encode_bool)?;
        }
        ImportedTranscriptContent::Thinking {
            thinking,
            signature,
        } => {
            push(&mut bytes, 4);
            encode_attestation(&mut bytes, thinking, encode_text)?;
            encode_attestation(&mut bytes, signature, encode_text)?;
        }
        ImportedTranscriptContent::RedactedThinking { data } => {
            push(&mut bytes, 5);
            encode_attestation(&mut bytes, data, encode_text)?;
        }
        ImportedTranscriptContent::Document { source } => {
            push(&mut bytes, 6);
            encode_attestation(&mut bytes, source, encode_media_source)?;
        }
        ImportedTranscriptContent::MessageContentAbsent(absence) => {
            push(&mut bytes, 7);
            push(
                &mut bytes,
                match absence {
                    ImportedMessageContentAbsence::MessageNotAttested => 0,
                    ImportedMessageContentAbsence::MessageAttestedAbsent => 1,
                    ImportedMessageContentAbsence::ContentNotAttested => 2,
                    ImportedMessageContentAbsence::ContentAttestedAbsent => 3,
                    ImportedMessageContentAbsence::EmptyBlockArray => 4,
                },
            );
        }
    }
    Ok(bytes)
}

pub(crate) fn decode_content(
    bytes: &[u8],
) -> Result<ImportedTranscriptContent, ImportedConversationEncodingFailure> {
    let mut decoder = Decoder::new(bytes, CONTENT_PAYLOAD)?;
    let content = match decoder.byte()? {
        0 => ImportedTranscriptContent::SourceEvent {
            source_type: decoder.attestation(Decoder::text)?,
        },
        1 => ImportedTranscriptContent::Text(decoder.attestation(Decoder::text)?),
        2 => ImportedTranscriptContent::ToolCall {
            source_call_id: decoder.attestation(Decoder::text)?,
            name: decoder.attestation(Decoder::text)?,
            input: decoder.attestation(|decoder| decoder.structured(0))?,
            caller: decoder.attestation(|decoder| decoder.structured(0))?,
        },
        3 => ImportedTranscriptContent::ToolResult {
            source_call_id: decoder.attestation(Decoder::text)?,
            content: decoder.attestation(Decoder::tool_result_value)?,
            is_error: decoder.attestation(Decoder::boolean)?,
        },
        4 => ImportedTranscriptContent::Thinking {
            thinking: decoder.attestation(Decoder::text)?,
            signature: decoder.attestation(Decoder::text)?,
        },
        5 => ImportedTranscriptContent::RedactedThinking {
            data: decoder.attestation(Decoder::text)?,
        },
        6 => ImportedTranscriptContent::Document {
            source: decoder.attestation(Decoder::media_source)?,
        },
        7 => ImportedTranscriptContent::MessageContentAbsent(match decoder.byte()? {
            0 => ImportedMessageContentAbsence::MessageNotAttested,
            1 => ImportedMessageContentAbsence::MessageAttestedAbsent,
            2 => ImportedMessageContentAbsence::ContentNotAttested,
            3 => ImportedMessageContentAbsence::ContentAttestedAbsent,
            4 => ImportedMessageContentAbsence::EmptyBlockArray,
            value => {
                return Err(ImportedConversationEncodingFailure::UnsupportedTag {
                    kind: "message content absence",
                    value,
                });
            }
        }),
        8 => ImportedTranscriptContent::SourceMessageBlock {
            source_type: decoder.attestation(Decoder::text)?,
        },
        value => {
            return Err(ImportedConversationEncodingFailure::UnsupportedTag {
                kind: "imported transcript content",
                value,
            });
        }
    };
    decoder.finish()?;
    Ok(content)
}

pub(crate) fn encode_source_metadata(
    source: &ImportedSourceMetadata,
) -> Result<Vec<u8>, ImportedConversationEncodingFailure> {
    let mut bytes = encoding_header(SOURCE_METADATA_PAYLOAD);
    encode_attestation(&mut bytes, source.record_id(), encode_text)?;
    encode_attestation(&mut bytes, source.parent_record_id(), encode_text)?;
    encode_attestation(&mut bytes, source.source_session_id(), encode_text)?;
    encode_attestation(&mut bytes, source.timestamp(), encode_text)?;
    encode_attestation(&mut bytes, source.sidechain(), encode_bool)?;
    encode_attestation(&mut bytes, source.metadata(), encode_bool)?;
    encode_attestation(&mut bytes, source.message_role(), encode_speaker)?;
    Ok(bytes)
}

pub(crate) fn decode_source_metadata(
    bytes: &[u8],
) -> Result<ImportedSourceMetadata, ImportedConversationEncodingFailure> {
    let mut decoder = Decoder::new(bytes, SOURCE_METADATA_PAYLOAD)?;
    let source = ImportedSourceMetadata::new(
        decoder.attestation(Decoder::text)?,
        decoder.attestation(Decoder::text)?,
        decoder.attestation(Decoder::text)?,
        decoder.attestation(Decoder::text)?,
        decoder.attestation(Decoder::boolean)?,
        decoder.attestation(Decoder::boolean)?,
        decoder.attestation(Decoder::speaker)?,
    );
    decoder.finish()?;
    Ok(source)
}

fn encode_attestation<Value>(
    bytes: &mut Vec<u8>,
    attestation: &ImportedSourceAttestation<Value>,
    encode_value: impl FnOnce(&mut Vec<u8>, &Value) -> Result<(), ImportedConversationEncodingFailure>,
) -> Result<(), ImportedConversationEncodingFailure> {
    match attestation {
        ImportedSourceAttestation::NotAttested => push(bytes, 0),
        ImportedSourceAttestation::AttestedAbsent => push(bytes, 1),
        ImportedSourceAttestation::Attested(value) => {
            push(bytes, 2);
            encode_value(bytes, value)?;
        }
    }
    Ok(())
}

fn encode_structured_value(
    bytes: &mut Vec<u8>,
    value: &ImportedStructuredValue,
    depth: usize,
) -> Result<(), ImportedConversationEncodingFailure> {
    match value {
        ImportedStructuredValue::Null => push(bytes, 0),
        ImportedStructuredValue::Boolean(value) => {
            push(bytes, 1);
            encode_bool(bytes, value)?;
        }
        ImportedStructuredValue::Number(value) => {
            push(bytes, 2);
            encode_bytes(bytes, value.as_str().as_bytes())?;
        }
        ImportedStructuredValue::String(value) => {
            push(bytes, 3);
            encode_text(bytes, value)?;
        }
        ImportedStructuredValue::Array(values) => {
            enter_container(depth)?;
            push(bytes, 4);
            encode_length(bytes, values.len())?;
            for value in values {
                encode_structured_value(bytes, value, depth + 1)?;
            }
        }
        ImportedStructuredValue::Object(members) => {
            enter_container(depth)?;
            push(bytes, 5);
            encode_length(bytes, members.len())?;
            for member in members {
                encode_text(bytes, member.name())?;
                encode_structured_value(bytes, member.value(), depth + 1)?;
            }
        }
    }
    Ok(())
}

fn enter_container(depth: usize) -> Result<(), ImportedConversationEncodingFailure> {
    if depth >= MAX_CONTAINER_DEPTH {
        Err(ImportedConversationEncodingFailure::ContainerDepthExceeded)
    } else {
        Ok(())
    }
}

fn encode_tool_result_value(
    bytes: &mut Vec<u8>,
    value: &ImportedToolResultValue,
) -> Result<(), ImportedConversationEncodingFailure> {
    match value {
        ImportedToolResultValue::Text(value) => {
            push(bytes, 0);
            encode_text(bytes, value)?;
        }
        ImportedToolResultValue::Blocks(blocks) => {
            push(bytes, 1);
            encode_length(bytes, blocks.len())?;
            for block in blocks {
                encode_tool_result_block(bytes, block)?;
            }
        }
    }
    Ok(())
}

fn encode_tool_result_block(
    bytes: &mut Vec<u8>,
    block: &ImportedToolResultBlock,
) -> Result<(), ImportedConversationEncodingFailure> {
    match block {
        ImportedToolResultBlock::Text(value) => {
            push(bytes, 0);
            encode_attestation(bytes, value, encode_text)?;
        }
        ImportedToolResultBlock::Image(source) => {
            push(bytes, 1);
            encode_attestation(bytes, source, encode_media_source)?;
        }
        ImportedToolResultBlock::ToolReference { tool_name } => {
            push(bytes, 2);
            encode_attestation(bytes, tool_name, encode_text)?;
        }
    }
    Ok(())
}

fn encode_media_source(
    bytes: &mut Vec<u8>,
    source: &ImportedMediaSource,
) -> Result<(), ImportedConversationEncodingFailure> {
    encode_attestation(bytes, source.kind(), encode_text)?;
    encode_attestation(bytes, source.media_type(), encode_text)?;
    encode_attestation(bytes, source.data(), encode_text)
}

fn encode_speaker(
    bytes: &mut Vec<u8>,
    speaker: &signalbox_domain::ImportedSpeaker,
) -> Result<(), ImportedConversationEncodingFailure> {
    push(
        bytes,
        match speaker {
            signalbox_domain::ImportedSpeaker::User => 0,
            signalbox_domain::ImportedSpeaker::Assistant => 1,
        },
    );
    Ok(())
}

fn encode_bool(
    bytes: &mut Vec<u8>,
    value: &bool,
) -> Result<(), ImportedConversationEncodingFailure> {
    push(bytes, u8::from(*value));
    Ok(())
}

fn encode_text(
    bytes: &mut Vec<u8>,
    value: &ImportedText,
) -> Result<(), ImportedConversationEncodingFailure> {
    encode_bytes(bytes, value.as_str().as_bytes())
}

fn encode_bytes(
    bytes: &mut Vec<u8>,
    value: &[u8],
) -> Result<(), ImportedConversationEncodingFailure> {
    encode_length(bytes, value.len())?;
    bytes.extend_from_slice(value);
    Ok(())
}

fn encode_length(
    bytes: &mut Vec<u8>,
    length: usize,
) -> Result<(), ImportedConversationEncodingFailure> {
    let length =
        u64::try_from(length).map_err(|_| ImportedConversationEncodingFailure::LengthOutOfRange)?;
    bytes.extend_from_slice(&length.to_be_bytes());
    Ok(())
}

fn push(bytes: &mut Vec<u8>, value: u8) {
    bytes.push(value);
}

fn encoding_header(payload_kind: u8) -> Vec<u8> {
    vec![ENCODING_VERSION, payload_kind]
}

struct Decoder<'bytes> {
    bytes: &'bytes [u8],
    index: usize,
}

impl<'bytes> Decoder<'bytes> {
    fn new(
        bytes: &'bytes [u8],
        expected_payload_kind: u8,
    ) -> Result<Self, ImportedConversationEncodingFailure> {
        let Some((&version, remainder)) = bytes.split_first() else {
            return Err(ImportedConversationEncodingFailure::UnexpectedEnd);
        };
        if version != ENCODING_VERSION {
            return Err(ImportedConversationEncodingFailure::UnsupportedVersion(
                version,
            ));
        }
        let Some(&actual_payload_kind) = remainder.first() else {
            return Err(ImportedConversationEncodingFailure::UnexpectedEnd);
        };
        if actual_payload_kind != expected_payload_kind {
            return Err(ImportedConversationEncodingFailure::UnexpectedPayloadKind {
                expected: expected_payload_kind,
                actual: actual_payload_kind,
            });
        }
        Ok(Self { bytes, index: 2 })
    }

    fn finish(self) -> Result<(), ImportedConversationEncodingFailure> {
        if self.index == self.bytes.len() {
            Ok(())
        } else {
            Err(ImportedConversationEncodingFailure::TrailingBytes)
        }
    }

    fn byte(&mut self) -> Result<u8, ImportedConversationEncodingFailure> {
        let value = self
            .bytes
            .get(self.index)
            .copied()
            .ok_or(ImportedConversationEncodingFailure::UnexpectedEnd)?;
        self.index += 1;
        Ok(value)
    }

    fn length(&mut self) -> Result<usize, ImportedConversationEncodingFailure> {
        let bytes = self.take(8)?;
        let encoded = <[u8; 8]>::try_from(bytes)
            .map_err(|_| ImportedConversationEncodingFailure::UnexpectedEnd)?;
        usize::try_from(u64::from_be_bytes(encoded))
            .map_err(|_| ImportedConversationEncodingFailure::LengthOutOfRange)
    }

    fn collection_length(&mut self) -> Result<usize, ImportedConversationEncodingFailure> {
        let length = self.length()?;
        let remaining = self.bytes.len().saturating_sub(self.index);
        if length > remaining {
            Err(ImportedConversationEncodingFailure::UnexpectedEnd)
        } else {
            Ok(length)
        }
    }

    fn take(&mut self, length: usize) -> Result<&'bytes [u8], ImportedConversationEncodingFailure> {
        let end = self
            .index
            .checked_add(length)
            .ok_or(ImportedConversationEncodingFailure::LengthOutOfRange)?;
        let value = self
            .bytes
            .get(self.index..end)
            .ok_or(ImportedConversationEncodingFailure::UnexpectedEnd)?;
        self.index = end;
        Ok(value)
    }

    fn text(&mut self) -> Result<ImportedText, ImportedConversationEncodingFailure> {
        let length = self.length()?;
        let bytes = self.take(length)?;
        let value = std::str::from_utf8(bytes)
            .map_err(|_| ImportedConversationEncodingFailure::InvalidUtf8("imported text"))?;
        Ok(ImportedText::new(String::from(value)))
    }

    fn boolean(&mut self) -> Result<bool, ImportedConversationEncodingFailure> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(ImportedConversationEncodingFailure::UnsupportedTag {
                kind: "boolean",
                value,
            }),
        }
    }

    fn speaker(
        &mut self,
    ) -> Result<signalbox_domain::ImportedSpeaker, ImportedConversationEncodingFailure> {
        match self.byte()? {
            0 => Ok(signalbox_domain::ImportedSpeaker::User),
            1 => Ok(signalbox_domain::ImportedSpeaker::Assistant),
            value => Err(ImportedConversationEncodingFailure::UnsupportedTag {
                kind: "speaker",
                value,
            }),
        }
    }

    fn attestation<Value>(
        &mut self,
        decode_value: impl FnOnce(&mut Self) -> Result<Value, ImportedConversationEncodingFailure>,
    ) -> Result<ImportedSourceAttestation<Value>, ImportedConversationEncodingFailure> {
        match self.byte()? {
            0 => Ok(ImportedSourceAttestation::NotAttested),
            1 => Ok(ImportedSourceAttestation::AttestedAbsent),
            2 => decode_value(self).map(ImportedSourceAttestation::Attested),
            value => Err(ImportedConversationEncodingFailure::UnsupportedTag {
                kind: "source attestation",
                value,
            }),
        }
    }

    fn structured(
        &mut self,
        depth: usize,
    ) -> Result<ImportedStructuredValue, ImportedConversationEncodingFailure> {
        match self.byte()? {
            0 => Ok(ImportedStructuredValue::Null),
            1 => self.boolean().map(ImportedStructuredValue::Boolean),
            2 => {
                let value = self.text()?.into_string();
                ImportedJsonNumber::try_new(value)
                    .map(ImportedStructuredValue::Number)
                    .map_err(|_| ImportedConversationEncodingFailure::InvalidJsonNumber)
            }
            3 => self.text().map(ImportedStructuredValue::String),
            4 => {
                enter_container(depth)?;
                let length = self.collection_length()?;
                let mut values = Vec::new();
                for _ in 0..length {
                    values
                        .try_reserve(1)
                        .map_err(|_| ImportedConversationEncodingFailure::LengthOutOfRange)?;
                    values.push(self.structured(depth + 1)?);
                }
                Ok(ImportedStructuredValue::Array(values.into_boxed_slice()))
            }
            5 => {
                enter_container(depth)?;
                let length = self.collection_length()?;
                let mut members = Vec::new();
                for _ in 0..length {
                    members
                        .try_reserve(1)
                        .map_err(|_| ImportedConversationEncodingFailure::LengthOutOfRange)?;
                    members.push(ImportedStructuredObjectMember::new(
                        self.text()?,
                        self.structured(depth + 1)?,
                    ));
                }
                Ok(ImportedStructuredValue::Object(members.into_boxed_slice()))
            }
            value => Err(ImportedConversationEncodingFailure::UnsupportedTag {
                kind: "structured value",
                value,
            }),
        }
    }

    fn tool_result_value(
        &mut self,
    ) -> Result<ImportedToolResultValue, ImportedConversationEncodingFailure> {
        match self.byte()? {
            0 => self.text().map(ImportedToolResultValue::Text),
            1 => {
                let length = self.collection_length()?;
                let mut blocks = Vec::new();
                for _ in 0..length {
                    blocks
                        .try_reserve(1)
                        .map_err(|_| ImportedConversationEncodingFailure::LengthOutOfRange)?;
                    blocks.push(self.tool_result_block()?);
                }
                Ok(ImportedToolResultValue::Blocks(blocks.into_boxed_slice()))
            }
            value => Err(ImportedConversationEncodingFailure::UnsupportedTag {
                kind: "tool-result value",
                value,
            }),
        }
    }

    fn tool_result_block(
        &mut self,
    ) -> Result<ImportedToolResultBlock, ImportedConversationEncodingFailure> {
        match self.byte()? {
            0 => self
                .attestation(Self::text)
                .map(ImportedToolResultBlock::Text),
            1 => self
                .attestation(Self::media_source)
                .map(ImportedToolResultBlock::Image),
            2 => Ok(ImportedToolResultBlock::ToolReference {
                tool_name: self.attestation(Self::text)?,
            }),
            value => Err(ImportedConversationEncodingFailure::UnsupportedTag {
                kind: "tool-result block",
                value,
            }),
        }
    }

    fn media_source(&mut self) -> Result<ImportedMediaSource, ImportedConversationEncodingFailure> {
        Ok(ImportedMediaSource::new(
            self.attestation(Self::text)?,
            self.attestation(Self::text)?,
            self.attestation(Self::text)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use signalbox_domain::{
        ImportedJsonNumber, ImportedMediaSource, ImportedMessageContentAbsence,
        ImportedSourceAttestation, ImportedSourceMetadata, ImportedSpeaker,
        ImportedStructuredObjectMember, ImportedStructuredValue, ImportedText,
        ImportedToolResultBlock, ImportedToolResultValue, ImportedTranscriptContent,
    };

    use super::{
        ImportedConversationEncodingFailure, decode_content, decode_source_metadata,
        decode_structured, encode_content, encode_source_metadata, encode_structured,
    };

    fn text(value: &str) -> ImportedText {
        ImportedText::new(String::from(value))
    }

    fn attested_text(value: &str) -> ImportedSourceAttestation<ImportedText> {
        ImportedSourceAttestation::Attested(text(value))
    }

    fn media() -> ImportedMediaSource {
        ImportedMediaSource::new(
            attested_text("base64"),
            attested_text("image/png"),
            attested_text("AA=="),
        )
    }

    #[track_caller]
    fn assert_content_round_trips(content: ImportedTranscriptContent) {
        let encoded = encode_content(&content).expect("fixture content must encode");
        assert_eq!(
            decode_content(&encoded).expect("fixture content must decode"),
            content
        );
    }

    #[track_caller]
    fn assert_version_one_structured(value: ImportedStructuredValue, expected: &[u8]) {
        assert_eq!(
            encode_structured(&value).expect("fixture value must encode"),
            expected
        );
        assert_eq!(
            decode_structured(expected).expect("literal version-one bytes must decode"),
            value
        );
    }

    #[track_caller]
    fn assert_version_one_content(content: ImportedTranscriptContent, expected: &[u8]) {
        assert_eq!(
            encode_content(&content).expect("fixture content must encode"),
            expected
        );
        assert_eq!(
            decode_content(expected).expect("literal version-one bytes must decode"),
            content
        );
    }

    #[track_caller]
    fn assert_version_one_source_metadata(source: ImportedSourceMetadata, expected: &[u8]) {
        assert_eq!(
            encode_source_metadata(&source).expect("fixture source must encode"),
            expected
        );
        assert_eq!(
            decode_source_metadata(expected).expect("literal version-one source bytes must decode"),
            source
        );
    }

    #[test]
    fn s28_inv002_inv038_structured_encoding_preserves_complete_domain_algebra() {
        let value = ImportedStructuredValue::Object(
            vec![
                ImportedStructuredObjectMember::new(
                    text("same"),
                    ImportedStructuredValue::Number(
                        ImportedJsonNumber::try_new(String::from("1e+09"))
                            .expect("fixture number is valid"),
                    ),
                ),
                ImportedStructuredObjectMember::new(
                    text("same"),
                    ImportedStructuredValue::Array(
                        vec![
                            ImportedStructuredValue::Null,
                            ImportedStructuredValue::Boolean(false),
                            ImportedStructuredValue::String(text("\0")),
                        ]
                        .into_boxed_slice(),
                    ),
                ),
            ]
            .into_boxed_slice(),
        );
        let encoded = encode_structured(&value).expect("fixture value must encode");
        assert_eq!(
            decode_structured(&encoded).expect("fixture encoding must decode"),
            value
        );
    }

    #[test]
    fn s28_inv002_inv038_content_encoding_round_trips_every_variant() {
        let structured = ImportedStructuredValue::Object(
            vec![ImportedStructuredObjectMember::new(
                text("key"),
                ImportedStructuredValue::String(text("value")),
            )]
            .into_boxed_slice(),
        );
        assert_content_round_trips(ImportedTranscriptContent::SourceEvent {
            source_type: attested_text("summary"),
        });
        assert_content_round_trips(ImportedTranscriptContent::SourceMessageBlock {
            source_type: attested_text("fallback"),
        });
        assert_content_round_trips(ImportedTranscriptContent::Text(
            ImportedSourceAttestation::AttestedAbsent,
        ));
        assert_content_round_trips(ImportedTranscriptContent::ToolCall {
            source_call_id: attested_text("call"),
            name: ImportedSourceAttestation::NotAttested,
            input: ImportedSourceAttestation::Attested(structured),
            caller: ImportedSourceAttestation::AttestedAbsent,
        });
        assert_content_round_trips(ImportedTranscriptContent::ToolResult {
            source_call_id: attested_text("call"),
            content: ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                vec![
                    ImportedToolResultBlock::Text(attested_text("")),
                    ImportedToolResultBlock::Image(ImportedSourceAttestation::Attested(media())),
                    ImportedToolResultBlock::ToolReference {
                        tool_name: ImportedSourceAttestation::AttestedAbsent,
                    },
                ]
                .into_boxed_slice(),
            )),
            is_error: ImportedSourceAttestation::Attested(false),
        });
        assert_content_round_trips(ImportedTranscriptContent::Thinking {
            thinking: attested_text("thought"),
            signature: attested_text("signature"),
        });
        assert_content_round_trips(ImportedTranscriptContent::RedactedThinking {
            data: attested_text("sealed"),
        });
        assert_content_round_trips(ImportedTranscriptContent::Document {
            source: ImportedSourceAttestation::Attested(media()),
        });
        assert_content_round_trips(ImportedTranscriptContent::MessageContentAbsent(
            ImportedMessageContentAbsence::EmptyBlockArray,
        ));
    }

    #[test]
    fn s28_inv002_inv038_source_metadata_encoding_retains_independent_attestations() {
        let source = ImportedSourceMetadata::new(
            attested_text("record"),
            ImportedSourceAttestation::AttestedAbsent,
            ImportedSourceAttestation::NotAttested,
            attested_text("timestamp"),
            ImportedSourceAttestation::Attested(true),
            ImportedSourceAttestation::Attested(false),
            ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
        );
        let encoded = encode_source_metadata(&source).expect("fixture source must encode");
        assert_eq!(
            decode_source_metadata(&encoded).expect("fixture source must decode"),
            source
        );
    }

    #[test]
    fn s28_inv002_inv038_version_one_structured_bytes_pin_every_tag_and_ordering() {
        let value = ImportedStructuredValue::Object(
            vec![
                ImportedStructuredObjectMember::new(text("n"), ImportedStructuredValue::Null),
                ImportedStructuredObjectMember::new(
                    text("b"),
                    ImportedStructuredValue::Boolean(true),
                ),
                ImportedStructuredObjectMember::new(
                    text("d"),
                    ImportedStructuredValue::Number(
                        ImportedJsonNumber::try_new(String::from("-1"))
                            .expect("fixture number is valid"),
                    ),
                ),
                ImportedStructuredObjectMember::new(
                    text("s"),
                    ImportedStructuredValue::String(text("x")),
                ),
                ImportedStructuredObjectMember::new(
                    text("a"),
                    ImportedStructuredValue::Array(
                        vec![ImportedStructuredValue::Boolean(false)].into_boxed_slice(),
                    ),
                ),
            ]
            .into_boxed_slice(),
        );
        let version_one = [
            1, 0, 5, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 0, 0, 0, 0, 1, 110, 0, 0, 0, 0, 0, 0, 0, 0,
            1, 98, 1, 1, 0, 0, 0, 0, 0, 0, 0, 1, 100, 2, 0, 0, 0, 0, 0, 0, 0, 2, 45, 49, 0, 0, 0,
            0, 0, 0, 0, 1, 115, 3, 0, 0, 0, 0, 0, 0, 0, 1, 120, 0, 0, 0, 0, 0, 0, 0, 1, 97, 4, 0,
            0, 0, 0, 0, 0, 0, 1, 1, 0,
        ];

        assert_version_one_structured(value, &version_one);
    }

    #[test]
    fn s28_inv002_inv038_version_one_content_bytes_pin_every_tag_and_field_order() {
        assert_version_one_content(
            ImportedTranscriptContent::SourceEvent {
                source_type: attested_text("e"),
            },
            &[1, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 101],
        );
        assert_version_one_content(
            ImportedTranscriptContent::Text(ImportedSourceAttestation::NotAttested),
            &[1, 1, 1, 0],
        );
        assert_version_one_content(
            ImportedTranscriptContent::ToolCall {
                source_call_id: attested_text("i"),
                name: ImportedSourceAttestation::AttestedAbsent,
                input: ImportedSourceAttestation::NotAttested,
                caller: ImportedSourceAttestation::Attested(ImportedStructuredValue::Null),
            },
            &[1, 1, 2, 2, 0, 0, 0, 0, 0, 0, 0, 1, 105, 1, 0, 2, 0],
        );
        assert_version_one_content(
            ImportedTranscriptContent::ToolResult {
                source_call_id: attested_text("i"),
                content: ImportedSourceAttestation::Attested(ImportedToolResultValue::Blocks(
                    vec![
                        ImportedToolResultBlock::Text(attested_text("t")),
                        ImportedToolResultBlock::Image(ImportedSourceAttestation::Attested(
                            ImportedMediaSource::new(
                                attested_text("k"),
                                ImportedSourceAttestation::AttestedAbsent,
                                ImportedSourceAttestation::NotAttested,
                            ),
                        )),
                        ImportedToolResultBlock::ToolReference {
                            tool_name: attested_text("r"),
                        },
                    ]
                    .into_boxed_slice(),
                )),
                is_error: ImportedSourceAttestation::Attested(true),
            },
            &[
                1, 1, 3, 2, 0, 0, 0, 0, 0, 0, 0, 1, 105, 2, 1, 0, 0, 0, 0, 0, 0, 0, 3, 0, 2, 0, 0,
                0, 0, 0, 0, 0, 1, 116, 1, 2, 2, 0, 0, 0, 0, 0, 0, 0, 1, 107, 1, 0, 2, 2, 0, 0, 0,
                0, 0, 0, 0, 1, 114, 2, 1,
            ],
        );
        assert_version_one_content(
            ImportedTranscriptContent::ToolResult {
                source_call_id: ImportedSourceAttestation::NotAttested,
                content: ImportedSourceAttestation::Attested(ImportedToolResultValue::Text(text(
                    "v",
                ))),
                is_error: ImportedSourceAttestation::AttestedAbsent,
            },
            &[1, 1, 3, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 1, 118, 1],
        );
        assert_version_one_content(
            ImportedTranscriptContent::Thinking {
                thinking: attested_text("t"),
                signature: ImportedSourceAttestation::AttestedAbsent,
            },
            &[1, 1, 4, 2, 0, 0, 0, 0, 0, 0, 0, 1, 116, 1],
        );
        assert_version_one_content(
            ImportedTranscriptContent::RedactedThinking {
                data: ImportedSourceAttestation::NotAttested,
            },
            &[1, 1, 5, 0],
        );
        assert_version_one_content(
            ImportedTranscriptContent::Document {
                source: ImportedSourceAttestation::Attested(ImportedMediaSource::new(
                    ImportedSourceAttestation::NotAttested,
                    attested_text("m"),
                    ImportedSourceAttestation::AttestedAbsent,
                )),
            },
            &[1, 1, 6, 2, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 109, 1],
        );
        assert_version_one_content(
            ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::MessageNotAttested,
            ),
            &[1, 1, 7, 0],
        );
        assert_version_one_content(
            ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::MessageAttestedAbsent,
            ),
            &[1, 1, 7, 1],
        );
        assert_version_one_content(
            ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::ContentNotAttested,
            ),
            &[1, 1, 7, 2],
        );
        assert_version_one_content(
            ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::ContentAttestedAbsent,
            ),
            &[1, 1, 7, 3],
        );
        assert_version_one_content(
            ImportedTranscriptContent::MessageContentAbsent(
                ImportedMessageContentAbsence::EmptyBlockArray,
            ),
            &[1, 1, 7, 4],
        );
        assert_version_one_content(
            ImportedTranscriptContent::SourceMessageBlock {
                source_type: attested_text("f"),
            },
            &[1, 1, 8, 2, 0, 0, 0, 0, 0, 0, 0, 1, 102],
        );
    }

    #[test]
    fn s28_inv002_inv038_version_one_source_metadata_bytes_pin_field_and_scalar_order() {
        let source = ImportedSourceMetadata::new(
            attested_text("r"),
            ImportedSourceAttestation::AttestedAbsent,
            ImportedSourceAttestation::NotAttested,
            attested_text("t"),
            ImportedSourceAttestation::Attested(true),
            ImportedSourceAttestation::Attested(false),
            ImportedSourceAttestation::Attested(ImportedSpeaker::Assistant),
        );
        let version_one = [
            1, 2, 2, 0, 0, 0, 0, 0, 0, 0, 1, 114, 1, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 116, 2, 1, 2, 0,
            2, 1,
        ];
        assert_version_one_source_metadata(source, &version_one);

        assert_version_one_source_metadata(
            ImportedSourceMetadata::new(
                ImportedSourceAttestation::NotAttested,
                ImportedSourceAttestation::NotAttested,
                ImportedSourceAttestation::NotAttested,
                ImportedSourceAttestation::NotAttested,
                ImportedSourceAttestation::NotAttested,
                ImportedSourceAttestation::NotAttested,
                ImportedSourceAttestation::Attested(ImportedSpeaker::User),
            ),
            &[1, 2, 0, 0, 0, 0, 0, 0, 2, 0],
        );
    }

    #[test]
    fn inv002_top_level_payload_kinds_are_domain_separated() {
        let content = encode_content(&ImportedTranscriptContent::Text(
            ImportedSourceAttestation::NotAttested,
        ))
        .expect("fixture content must encode");
        assert_eq!(
            decode_structured(&content),
            Err(ImportedConversationEncodingFailure::UnexpectedPayloadKind {
                expected: 0,
                actual: 1,
            })
        );

        let structured =
            encode_structured(&ImportedStructuredValue::Null).expect("fixture value must encode");
        assert_eq!(
            decode_content(&structured),
            Err(ImportedConversationEncodingFailure::UnexpectedPayloadKind {
                expected: 1,
                actual: 0,
            })
        );
    }

    #[test]
    fn inv002_encoding_rejects_trailing_bytes() {
        let mut encoded =
            encode_structured(&ImportedStructuredValue::Null).expect("fixture must encode");
        encoded.push(0);
        assert_eq!(
            decode_structured(&encoded),
            Err(ImportedConversationEncodingFailure::TrailingBytes)
        );
    }

    #[test]
    fn inv002_corrupt_collection_count_fails_after_incremental_decoding() {
        let value = ImportedStructuredValue::Array(
            vec![ImportedStructuredValue::String(ImportedText::new(
                "x".repeat(4_096),
            ))]
            .into_boxed_slice(),
        );
        let mut encoded = encode_structured(&value).expect("fixture must encode");
        let claimed_count =
            u64::try_from(encoded.len() - 11).expect("fixture encoded length fits u64");
        encoded[3..11].copy_from_slice(&claimed_count.to_be_bytes());

        assert_eq!(
            decode_structured(&encoded),
            Err(ImportedConversationEncodingFailure::UnexpectedEnd)
        );
    }
}
