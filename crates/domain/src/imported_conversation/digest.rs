//! Imported raw-record and conversation content digests for `docs/spec/conversation-import.md`.

use super::format::ImportedConversationFormat;
use super::record::ImportedRawSourceRecordReconstitutionInput;
use super::structured_value::ImportedStructuredValue;
use super::structured_value::ImportedText;
use sha2::{Digest, Sha256};
use std::hash::Hash;

const SOURCE_DIGEST_DOMAIN: &[u8] = b"signalbox.imported-conversation.source-digest.v1";
const RAW_RECORD_CONVERSION_DIGEST_DOMAIN: &[u8] =
    b"signalbox.imported-conversation.raw-record-conversion.v1";
/// SHA-256 of one exact raw source-record byte sequence.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ImportedRawRecordHash([u8; 32]);

impl ImportedRawRecordHash {
    /// Reconstitutes one stored digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the fixed digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hashes exact source-record bytes.
    pub fn digest(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
}

/// SHA-256 authentication of one exact raw hash and normalized source record.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ImportedRawRecordConversionDigest([u8; 32]);

impl ImportedRawRecordConversionDigest {
    /// Reconstitutes one stored conversion digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the fixed digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(super) fn derive(
        raw_hash: ImportedRawRecordHash,
        normalized: &ImportedStructuredValue,
    ) -> Self {
        let mut digest = Sha256::new();
        update_length_framed(&mut digest, RAW_RECORD_CONVERSION_DIGEST_DOMAIN);
        update_length_framed(&mut digest, raw_hash.as_bytes());
        update_structured_digest(&mut digest, normalized);
        Self(digest.finalize().into())
    }
}

/// Domain-separated SHA-256 of a format and ordered raw-record hashes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ImportedConversationSourceDigest([u8; 32]);

/// Incremental derivation of an ordered imported-source digest.
pub struct ImportedConversationSourceDigestBuilder {
    digest: Sha256,
    expected_records: u64,
    observed_records: u64,
}

impl ImportedConversationSourceDigestBuilder {
    /// Starts a digest for an exact format and accepted-record count.
    pub fn new(format: ImportedConversationFormat, expected_records: u64) -> Self {
        let mut digest = Sha256::new();
        update_length_framed(&mut digest, SOURCE_DIGEST_DOMAIN);
        update_length_framed(&mut digest, format.digest_tag());
        digest.update(expected_records.to_be_bytes());
        Self {
            digest,
            expected_records,
            observed_records: 0,
        }
    }

    /// Adds the next accepted raw-record hash in physical order.
    pub fn push(&mut self, hash: ImportedRawRecordHash) -> bool {
        let Some(observed) = self.observed_records.checked_add(1) else {
            return false;
        };
        if observed > self.expected_records {
            return false;
        }
        update_length_framed(&mut self.digest, hash.as_bytes());
        self.observed_records = observed;
        true
    }

    /// Finishes only after the declared number of hashes was supplied.
    pub fn finish(self) -> Option<ImportedConversationSourceDigest> {
        (self.observed_records == self.expected_records)
            .then(|| ImportedConversationSourceDigest(self.digest.finalize().into()))
    }
}

impl ImportedConversationSourceDigest {
    /// Reconstitutes one stored source digest.
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the fixed digest bytes.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub(super) fn derive(
        format: ImportedConversationFormat,
        records: &[ImportedRawSourceRecordReconstitutionInput],
    ) -> Self {
        let mut digest = Sha256::new();
        update_length_framed(&mut digest, SOURCE_DIGEST_DOMAIN);
        update_length_framed(&mut digest, format.digest_tag());
        digest.update(
            u64::try_from(records.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for record in records {
            update_length_framed(&mut digest, record.stored_hash.as_bytes());
        }
        Self(digest.finalize().into())
    }
}

fn update_length_framed(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}
fn update_structured_digest(digest: &mut Sha256, value: &ImportedStructuredValue) {
    enum Part<'a> {
        Value(&'a ImportedStructuredValue),
        ObjectMemberName(&'a ImportedText),
    }

    let mut pending = vec![Part::Value(value)];
    while let Some(part) = pending.pop() {
        match part {
            Part::Value(ImportedStructuredValue::Null) => digest.update([0]),
            Part::Value(ImportedStructuredValue::Boolean(false)) => digest.update([1]),
            Part::Value(ImportedStructuredValue::Boolean(true)) => digest.update([2]),
            Part::Value(ImportedStructuredValue::Number(value)) => {
                digest.update([3]);
                update_length_framed(digest, value.as_str().as_bytes());
            }
            Part::Value(ImportedStructuredValue::String(value)) => {
                digest.update([4]);
                update_length_framed(digest, value.as_str().as_bytes());
            }
            Part::Value(ImportedStructuredValue::Array(values)) => {
                digest.update([5]);
                digest.update(
                    u64::try_from(values.len())
                        .unwrap_or(u64::MAX)
                        .to_be_bytes(),
                );
                pending.extend(values.iter().rev().map(Part::Value));
            }
            Part::Value(ImportedStructuredValue::Object(members)) => {
                digest.update([6]);
                digest.update(
                    u64::try_from(members.len())
                        .unwrap_or(u64::MAX)
                        .to_be_bytes(),
                );
                for member in members.iter().rev() {
                    pending.push(Part::Value(member.value()));
                    pending.push(Part::ObjectMemberName(member.name()));
                }
            }
            Part::ObjectMemberName(name) => {
                update_length_framed(digest, name.as_str().as_bytes());
            }
        }
    }
}
