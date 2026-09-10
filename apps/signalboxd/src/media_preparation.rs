//! Authentication and bounded materialization of rendered durable image results.

use crate::{
    BlobStoreRegistry,
    blob_read_runtime::{BlobReadError, read_blob_chunk},
};
use signalbox_application::{ClassifyOperatorFailure as _, OperatorFailureClass};
use signalbox_domain::{BlobDigest, ToolRequestId};
use signalbox_model_runtime::{
    ImageInput, ImagePresentationCapability, MessagePart, ModelOperation,
};
use signalbox_persistence::{
    blob::{BlobCatalogRepository, BlobCatalogRepositoryError},
    tool_loop::{PostgresToolLoopRepository, ToolLoopRepositoryError},
};
use sqlx::PgPool;
use std::{num::NonZeroU64, sync::Arc};

#[derive(Clone, Debug)]
pub(crate) struct MediaPreparation {
    evidence: PostgresToolLoopRepository,
    catalog: BlobCatalogRepository,
    stores: Option<Arc<BlobStoreRegistry>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum MediaPreparationFailure {
    Unsupported,
    Unavailable,
    Missing,
    BlobCorrupt,
    Corrupt,
}

impl MediaPreparation {
    pub(crate) fn new(pool: PgPool, stores: Option<Arc<BlobStoreRegistry>>) -> Self {
        Self {
            evidence: PostgresToolLoopRepository::new(pool.clone()),
            catalog: BlobCatalogRepository::new(pool),
            stores,
        }
    }
    pub(crate) async fn prepare<C>(
        &self,
        operation: &mut ModelOperation<C>,
        target: Option<&ImagePresentationCapability>,
    ) -> Result<(), MediaPreparationFailure> {
        let mut references = Vec::new();
        let mut aggregate_bytes = 0_u64;
        for (message_index, message) in operation.messages.iter().enumerate() {
            for (part_index, part) in message.parts.iter().enumerate() {
                let MessagePart::ImageReference(reference) = part else {
                    continue;
                };
                let target = target.ok_or(MediaPreparationFailure::Unsupported)?;
                aggregate_bytes = aggregate_bytes
                    .checked_add(reference.byte_length.get())
                    .ok_or(MediaPreparationFailure::Unsupported)?;
                if !target.admits(&reference.media_type, reference.byte_length.get())
                    || reference.byte_length.get()
                        > signalbox_file_media_runtime::MAX_PRESENTED_IMAGE_BYTES
                    || aggregate_bytes
                        > signalbox_file_media_runtime::MAX_AGGREGATE_MEDIA_BYTES_PER_CALL
                    || references.len()
                        >= usize::from(signalbox_file_media_runtime::MAX_MEDIA_REFERENCES_PER_CALL)
                {
                    return Err(MediaPreparationFailure::Unsupported);
                }
                references.push((message_index, part_index, reference.clone()));
            }
        }
        if references.is_empty() {
            return Ok(());
        }
        let stores = self
            .stores
            .as_ref()
            .ok_or(MediaPreparationFailure::Unavailable)?;
        let mut authenticated = Vec::with_capacity(references.len());
        for (message, part, reference) in references {
            let request = uuid::Uuid::parse_str(&reference.authority)
                .map(ToolRequestId::from_uuid)
                .map_err(|_| MediaPreparationFailure::Corrupt)?;
            let proof = self
                .evidence
                .load_media_reference(request)
                .await
                .map_err(evidence_failure)?
                .ok_or(MediaPreparationFailure::Corrupt)?;
            if proof.presented().digest().as_bytes() != &reference.digest
                || proof.presented().media_type() != reference.media_type
                || proof.byte_length() != reference.byte_length
            {
                return Err(MediaPreparationFailure::Corrupt);
            }
            let entry = self
                .catalog
                .find(BlobDigest::from_bytes(reference.digest))
                .await
                .map_err(catalog_failure)?
                .ok_or(MediaPreparationFailure::Missing)?;
            if entry.expected().byte_length() != reference.byte_length.get() {
                return Err(MediaPreparationFailure::Corrupt);
            }
            authenticated.push((message, part, reference, entry));
        }
        // Every authority query has completed before store I/O begins.
        let _permit = stores
            .read_budget()
            .acquire_owned()
            .await
            .map_err(|_| MediaPreparationFailure::Unavailable)?;
        for (message, part, reference, entry) in authenticated {
            let mut bytes = Vec::with_capacity(reference.byte_length.get() as usize);
            while (bytes.len() as u64) < reference.byte_length.get() {
                let length = NonZeroU64::new(
                    (reference.byte_length.get() - bytes.len() as u64)
                        .min(signalbox_blob_store::MAX_BLOB_RANGE_BYTES),
                )
                .ok_or(MediaPreparationFailure::Corrupt)?;
                let section = read_blob_chunk(stores, &entry, bytes.len() as u64, length)
                    .await
                    .map_err(source_failure)?;
                bytes.extend_from_slice(&section);
            }
            operation.messages[message].parts[part] = MessagePart::Image(ImageInput {
                media_type: reference.media_type,
                bytes: bytes.into(),
            });
        }
        Ok(())
    }
}

fn evidence_failure(error: ToolLoopRepositoryError) -> MediaPreparationFailure {
    match error.operator_failure_class() {
        OperatorFailureClass::Infrastructure { .. } => MediaPreparationFailure::Unavailable,
        _ => MediaPreparationFailure::Corrupt,
    }
}
fn catalog_failure(error: BlobCatalogRepositoryError) -> MediaPreparationFailure {
    match error {
        BlobCatalogRepositoryError::Database(_)
        | BlobCatalogRepositoryError::CommitAmbiguous(_) => MediaPreparationFailure::Unavailable,
        BlobCatalogRepositoryError::Corruption(_) => MediaPreparationFailure::Corrupt,
    }
}
fn source_failure(error: BlobReadError) -> MediaPreparationFailure {
    match error {
        BlobReadError::Unavailable => MediaPreparationFailure::Unavailable,
        BlobReadError::Missing | BlobReadError::NotFound => MediaPreparationFailure::Missing,
        BlobReadError::Corrupt => MediaPreparationFailure::BlobCorrupt,
        BlobReadError::Integrity | BlobReadError::RangeOutOfBounds => {
            MediaPreparationFailure::Corrupt
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use signalbox_model_runtime::*;
    #[tokio::test]
    async fn file_image_bounds_reject_before_authority_or_source_access() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://invalid/fixture")
            .unwrap();
        pool.close().await;
        let preparation = MediaPreparation::new(pool, None);
        let mut operation = ModelOperation::new(
            (),
            CredentialReference::new("fixture"),
            RequestedTarget::new("fixture"),
            ResolvedTarget::new("fixture"),
            vec![ConversationMessage {
                role: ConversationRole::User,
                parts: vec![MessagePart::ImageReference(ImageReference {
                    authority: "invalid".into(),
                    digest: [0; 32],
                    byte_length: NonZeroU64::new(9 * 1024 * 1024).unwrap(),
                    media_type: "image/png".into(),
                })],
            }],
            ModelSettings::new(64),
        );
        let target = signalbox_model_runtime_codex_cli::image_presentation_capability();
        assert!(matches!(
            preparation.prepare(&mut operation, Some(&target)).await,
            Err(MediaPreparationFailure::Unsupported)
        ));
        assert!(matches!(
            preparation.prepare(&mut operation, None).await,
            Err(MediaPreparationFailure::Unsupported)
        ));
    }
    #[test]
    fn file_image_failures_keep_infrastructure_absence_and_integrity_distinct() {
        assert!(matches!(
            source_failure(BlobReadError::Integrity),
            MediaPreparationFailure::Corrupt
        ));
        assert!(matches!(
            source_failure(BlobReadError::Corrupt),
            MediaPreparationFailure::BlobCorrupt
        ));
        assert!(matches!(
            source_failure(BlobReadError::Missing),
            MediaPreparationFailure::Missing
        ));
        assert!(matches!(
            catalog_failure(BlobCatalogRepositoryError::Database(
                sqlx::Error::PoolClosed
            )),
            MediaPreparationFailure::Unavailable
        ));
    }
}
