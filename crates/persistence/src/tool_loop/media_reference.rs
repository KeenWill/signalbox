//! Storage records for content-silent image evidence retained on a terminal attempt.

use super::*;
use signalbox_domain::{MediaValidationIdentity, ToolMediaReference};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityRecord {
    digest: String,
    media_type: String,
    provider: String,
    reader: String,
    revision: String,
    evidence: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceRecord {
    kind: String,
    byte_length: u64,
    presented: IdentityRecord,
    source: IdentityRecord,
}

fn identity_record(identity: &MediaValidationIdentity) -> IdentityRecord {
    IdentityRecord {
        digest: identity.digest().to_string(),
        media_type: identity.media_type().to_owned(),
        provider: identity.provider().to_owned(),
        reader: identity.reader().to_owned(),
        revision: identity.revision().to_owned(),
        evidence: crate::mapping::media_validation_to_str(identity.evidence()).to_owned(),
    }
}

pub(super) fn encode(
    reference: &ToolMediaReference,
) -> Result<serde_json::Value, ToolLoopRepositoryError> {
    serde_json::to_value(ReferenceRecord {
        kind: crate::mapping::media_presentation_to_str(
            crate::mapping::MediaPresentationStorageKind::Image,
        )
        .to_owned(),
        byte_length: reference.byte_length().get(),
        presented: identity_record(reference.presented()),
        source: identity_record(reference.source()),
    })
    .map_err(|_| ToolLoopCorruption::Inconsistent("media reference encoding").into())
}

fn identity(record: IdentityRecord) -> Option<MediaValidationIdentity> {
    MediaValidationIdentity::try_new(
        record.digest.parse().ok()?,
        record.media_type,
        record.provider,
        record.reader,
        record.revision,
        crate::mapping::media_validation_from_str(&record.evidence)?,
    )
}

pub(super) fn decode(
    value: serde_json::Value,
) -> Result<ToolMediaReference, ToolLoopRepositoryError> {
    let invalid = || ToolLoopCorruption::Inconsistent("media reference evidence");
    let record: ReferenceRecord = serde_json::from_value(value).map_err(|_| invalid())?;
    let presented = identity(record.presented).ok_or_else(invalid)?;
    let source = identity(record.source).ok_or_else(invalid)?;
    if crate::mapping::media_presentation_from_str(&record.kind).is_none() {
        return Err(invalid().into());
    }
    ToolMediaReference::image(
        presented,
        source,
        std::num::NonZeroU64::new(record.byte_length).ok_or_else(invalid)?,
    )
    .ok_or_else(|| invalid().into())
}

impl PostgresToolLoopRepository {
    /// Authenticates rich evidence from a rendered tool result against its durable terminal attempt.
    pub async fn load_media_reference(
        &self,
        request: ToolRequestId,
    ) -> Result<Option<ToolMediaReference>, ToolLoopRepositoryError> {
        let stored: Option<Option<serde_json::Value>> = sqlx::query_scalar(
            "SELECT result_media_reference FROM tool_attempt
             WHERE request_id = $1 AND terminal_disposition_kind = 'completed'
               AND result_content_kind = 'media'",
        )
        .bind(request.into_uuid())
        .fetch_optional(&self.pool)
        .await?;
        match stored {
            None => Ok(None),
            Some(Some(value)) => decode(value).map(Some),
            Some(None) => {
                Err(ToolLoopCorruption::Inconsistent("missing media reference evidence").into())
            }
        }
    }
}

impl PostgresToolLoopRepository {
    /// Returns the durable serving target that issued this authorized tool request.
    pub async fn file_use_target(
        &self,
        request: &signalbox_domain::ToolRequest,
    ) -> Result<signalbox_domain::ResolvedProviderTarget, ToolLoopRepositoryError> {
        let target: Option<Option<Uuid>> = sqlx::query_scalar("SELECT effective_provider_model_identity_id FROM model_call WHERE model_call_id = $1 AND session_id = $2")
            .bind(request.producing_call().into_uuid()).bind(request.session().into_uuid()).fetch_optional(&self.pool).await?;
        let target = target
            .flatten()
            .ok_or(ToolLoopCorruption::Inconsistent("file use serving target"))?;
        Ok(signalbox_domain::ResolvedProviderTarget::naming(
            signalbox_domain::ProviderModelIdentity::from_uuid(target),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_domain::MediaValidationEvidence;
    fn identity(seed: u8, media: &str) -> MediaValidationIdentity {
        MediaValidationIdentity::try_new(
            signalbox_domain::BlobDigest::from_bytes([seed; 32]),
            media.into(),
            "image_fixture".into(),
            "png".into(),
            "v2".into(),
            MediaValidationEvidence::StrongSignature,
        )
        .unwrap()
    }
    #[test]
    fn derived_reference_roundtrip_preserves_distinct_source_and_presented_evidence() {
        let reference = ToolMediaReference::image(
            identity(1, "image/png"),
            identity(2, "image/jpeg"),
            std::num::NonZeroU64::new(64).unwrap(),
        )
        .unwrap();
        assert_eq!(decode(encode(&reference).unwrap()).unwrap(), reference);
    }
    #[test]
    fn corrupt_reference_records_cannot_reconstitute_authority() {
        let reference = ToolMediaReference::direct_image(
            identity(1, "image/png"),
            std::num::NonZeroU64::new(64).unwrap(),
        )
        .unwrap();
        for (field, value) in [
            ("byte_length", serde_json::json!(0)),
            ("kind", serde_json::json!("text")),
        ] {
            let mut record = encode(&reference).unwrap();
            record[field] = value;
            assert!(decode(record).is_err());
        }
        let mut record = encode(&reference).unwrap();
        record["presented"]["evidence"] = serde_json::json!("declared");
        assert!(decode(record).is_err());
    }
}
