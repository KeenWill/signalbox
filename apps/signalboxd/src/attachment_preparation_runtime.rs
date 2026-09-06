//! Pre-provider verification of rendered attachment authority.

use std::{
    collections::{BTreeSet, HashSet},
    future::Future,
    sync::Arc,
};

use sha2::{Digest as _, Sha256};
use signalbox_application::{
    AttachmentPreparationFailure, ModelCallCapabilityPreparation, ModelCallInputTokenCount,
    ModelCallInputTokenCounter, ModelCallProvider, PreparedModelOperation,
};
use signalbox_blob_store::{BlobStoreFailureKind, ExpectedBlob};
use signalbox_domain::{BlobDigest, PreparedModelCallRequest, ResolvedProviderTarget};
use signalbox_persistence::blob::{
    BlobCatalogEntry, BlobCatalogRepository, BlobCatalogRepositoryError,
};
use sqlx::PgPool;
use tokio::io::AsyncReadExt as _;

use crate::BlobStoreRegistry;

const VERIFICATION_BUFFER_BYTES: usize = 64 * 1024;

/// Provider wrapper that verifies every rendered attachment before capability
/// preparation or send authorization can begin.
#[derive(Clone, Debug)]
pub struct AttachmentPreparingModelCallProvider<Provider> {
    inner: Provider,
    catalog: BlobCatalogRepository,
    registry: Option<Arc<BlobStoreRegistry>>,
    count_targets: Option<Arc<HashSet<ResolvedProviderTarget>>>,
}

impl<Provider> AttachmentPreparingModelCallProvider<Provider> {
    /// Composes attachment preparation over one provider adapter.
    pub fn new(inner: Provider, pool: PgPool, registry: Option<Arc<BlobStoreRegistry>>) -> Self {
        Self {
            inner,
            catalog: BlobCatalogRepository::new(pool),
            registry,
            count_targets: None,
        }
    }

    /// Restricts attachment verification before prospective counting to the
    /// targets whose adapters can perform that provider interaction.
    pub fn for_counting(
        inner: Provider,
        pool: PgPool,
        registry: Option<Arc<BlobStoreRegistry>>,
        count_targets: HashSet<ResolvedProviderTarget>,
    ) -> Self {
        Self {
            inner,
            catalog: BlobCatalogRepository::new(pool),
            registry,
            count_targets: Some(Arc::new(count_targets)),
        }
    }
}

impl<Provider> ModelCallProvider for AttachmentPreparingModelCallProvider<Provider>
where
    Provider: ModelCallProvider + Send,
    Provider::Capability: Send,
{
    type Capability = Provider::Capability;
    type Error = Provider::Error;

    async fn prepare_capability<Cancellation>(
        &mut self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> Result<ModelCallCapabilityPreparation<Self::Capability>, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        let digests = operation.attachment_digests().collect::<BTreeSet<_>>();
        if digests.is_empty() {
            return self.inner.prepare_capability(operation, cancellation).await;
        }

        let mut cancellation = Box::pin(cancellation);
        let prepared = {
            let preparation = prepare_attachments(
                &self.catalog,
                self.registry.as_deref(),
                operation.request(),
                digests,
            );
            tokio::pin!(preparation);
            tokio::select! {
                biased;
                () = &mut cancellation => {
                    return Ok(ModelCallCapabilityPreparation::Cancelled);
                }
                prepared = &mut preparation => prepared,
            }
        };
        if let Err(failure) = prepared {
            return Ok(ModelCallCapabilityPreparation::AttachmentFailure(failure));
        }
        self.inner.prepare_capability(operation, cancellation).await
    }

    async fn invoke<AcceptancePossible, Cancellation>(
        &mut self,
        authorized: signalbox_domain::AuthorizedModelCall,
        capability: Self::Capability,
        acceptance_possible: AcceptancePossible,
        cancellation: Cancellation,
    ) -> Result<signalbox_domain::CorrelatedModelCallTerminalObservation, Self::Error>
    where
        AcceptancePossible: FnOnce() + Send,
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        self.inner
            .invoke(authorized, capability, acceptance_possible, cancellation)
            .await
    }
}

impl<Provider> ModelCallInputTokenCounter for AttachmentPreparingModelCallProvider<Provider>
where
    Provider: ModelCallInputTokenCounter + Sync,
{
    type Error = Provider::Error;

    async fn count_input_tokens<Cancellation>(
        &self,
        operation: PreparedModelOperation,
        cancellation: Cancellation,
    ) -> Result<ModelCallInputTokenCount, Self::Error>
    where
        Cancellation: Future<Output = ()> + Send + 'static,
    {
        if self
            .count_targets
            .as_ref()
            .is_some_and(|targets| !targets.contains(&operation.request().call().target()))
        {
            return self.inner.count_input_tokens(operation, cancellation).await;
        }
        let digests = operation.attachment_digests().collect::<BTreeSet<_>>();
        if digests.is_empty() {
            return self.inner.count_input_tokens(operation, cancellation).await;
        }

        let mut cancellation = Box::pin(cancellation);
        let prepared = {
            let preparation = prepare_attachments(
                &self.catalog,
                self.registry.as_deref(),
                operation.request(),
                digests,
            );
            tokio::pin!(preparation);
            tokio::select! {
                biased;
                () = &mut cancellation => {
                    return Ok(ModelCallInputTokenCount::Cancelled);
                }
                prepared = &mut preparation => prepared,
            }
        };
        if let Err(failure) = prepared {
            return Ok(attachment_count_failure(failure));
        }
        self.inner.count_input_tokens(operation, cancellation).await
    }
}

fn attachment_count_failure(failure: AttachmentPreparationFailure) -> ModelCallInputTokenCount {
    match failure {
        AttachmentPreparationFailure::Unavailable => {
            ModelCallInputTokenCount::AttachmentUnavailable
        }
        AttachmentPreparationFailure::TooLarge { .. }
        | AttachmentPreparationFailure::Missing
        | AttachmentPreparationFailure::Corrupt => {
            ModelCallInputTokenCount::AttachmentFailure(failure)
        }
    }
}

async fn prepare_attachments(
    catalog: &BlobCatalogRepository,
    registry: Option<&BlobStoreRegistry>,
    request: &PreparedModelCallRequest,
    digests: BTreeSet<BlobDigest>,
) -> Result<(), AttachmentPreparationFailure> {
    let Some(registry) = registry else {
        return Err(AttachmentPreparationFailure::Corrupt);
    };
    let mut entries = Vec::with_capacity(digests.len());
    let mut total = 0_u64;
    for digest in digests {
        let entry = catalog
            .find(digest)
            .await
            .map_err(map_catalog_failure)?
            .ok_or(AttachmentPreparationFailure::Missing)?;
        let expected = entry.expected();
        if request
            .attachment_byte_length(digest)
            .map(|length| length.get())
            != Some(expected.byte_length())
        {
            return Err(AttachmentPreparationFailure::Corrupt);
        }
        total = total.checked_add(expected.byte_length()).ok_or(
            AttachmentPreparationFailure::TooLarge {
                maximum_bytes: registry.max_blob_bytes(),
            },
        )?;
        entries.push(entry);
    }
    if total > registry.max_blob_bytes() {
        return Err(AttachmentPreparationFailure::TooLarge {
            maximum_bytes: registry.max_blob_bytes(),
        });
    }
    for entry in &entries {
        verify_entry(registry, entry).await?;
    }
    Ok(())
}

async fn verify_entry(
    registry: &BlobStoreRegistry,
    entry: &BlobCatalogEntry,
) -> Result<(), AttachmentPreparationFailure> {
    let expected = entry.expected();
    let mut saw_missing = false;
    let mut saw_corrupt = false;
    let mut saw_unavailable = false;
    for replica in entry.replicas() {
        let Some(store) = registry.recorded_store(replica.store()) else {
            return Err(AttachmentPreparationFailure::Corrupt);
        };
        match store.open(replica.object_key()).await {
            Ok(opened) => {
                if opened.byte_length() != expected.byte_length() {
                    saw_corrupt = true;
                    continue;
                }
                match verify_stream(opened.into_reader(), expected).await {
                    Ok(()) => return Ok(()),
                    Err(StreamVerificationFailure::Corrupt) => saw_corrupt = true,
                    Err(StreamVerificationFailure::Unavailable) => saw_unavailable = true,
                }
            }
            Err(error) => match error.kind() {
                BlobStoreFailureKind::NotFound => saw_missing = true,
                BlobStoreFailureKind::VerificationFailed => saw_corrupt = true,
                BlobStoreFailureKind::PublicationAmbiguous | BlobStoreFailureKind::Unavailable => {
                    saw_unavailable = true;
                }
            },
        }
    }
    if saw_unavailable {
        Err(AttachmentPreparationFailure::Unavailable)
    } else if saw_corrupt {
        Err(AttachmentPreparationFailure::Corrupt)
    } else if saw_missing || entry.replicas().is_empty() {
        Err(AttachmentPreparationFailure::Missing)
    } else {
        Err(AttachmentPreparationFailure::Corrupt)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamVerificationFailure {
    Corrupt,
    Unavailable,
}

async fn verify_stream(
    mut reader: signalbox_blob_store::BlobReader,
    expected: ExpectedBlob,
) -> Result<(), StreamVerificationFailure> {
    let mut digest = Sha256::new();
    let mut observed = 0_u64;
    let mut buffer = vec![0_u8; VERIFICATION_BUFFER_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|_| StreamVerificationFailure::Unavailable)?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(u64::try_from(read).map_err(|_| StreamVerificationFailure::Corrupt)?)
            .ok_or(StreamVerificationFailure::Corrupt)?;
        if observed > expected.byte_length() {
            return Err(StreamVerificationFailure::Corrupt);
        }
        digest.update(&buffer[..read]);
    }
    let observed_digest = BlobDigest::from_bytes(digest.finalize().into());
    if observed == expected.byte_length() && observed_digest == expected.digest() {
        Ok(())
    } else {
        Err(StreamVerificationFailure::Corrupt)
    }
}

fn map_catalog_failure(error: BlobCatalogRepositoryError) -> AttachmentPreparationFailure {
    match error {
        BlobCatalogRepositoryError::Database(_)
        | BlobCatalogRepositoryError::CommitAmbiguous(_) => {
            AttachmentPreparationFailure::Unavailable
        }
        BlobCatalogRepositoryError::Corruption(_) => AttachmentPreparationFailure::Corrupt,
    }
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, num::NonZeroU64};

    use signalbox_blob_store::ExpectedBlob;
    use signalbox_domain::BlobDigest;

    use signalbox_application::{AttachmentPreparationFailure, ModelCallInputTokenCount};

    use super::{StreamVerificationFailure, attachment_count_failure, verify_stream};

    #[test]
    fn attachment_count_failures_preserve_transient_and_definitive_classes() {
        assert_eq!(
            attachment_count_failure(AttachmentPreparationFailure::Unavailable),
            ModelCallInputTokenCount::AttachmentUnavailable
        );
        assert_eq!(
            attachment_count_failure(AttachmentPreparationFailure::Missing),
            ModelCallInputTokenCount::AttachmentFailure(AttachmentPreparationFailure::Missing)
        );
    }

    fn expected(bytes: &[u8]) -> ExpectedBlob {
        ExpectedBlob::new(
            BlobDigest::digest(bytes),
            NonZeroU64::new(u64::try_from(bytes.len()).expect("fixture length fits u64"))
                .expect("fixtures are nonempty"),
        )
    }

    #[tokio::test]
    async fn exact_stream_verifies_without_retaining_attachment_bytes() {
        let bytes = b"canonical attachment bytes";

        assert_eq!(
            verify_stream(Box::new(Cursor::new(bytes.to_vec())), expected(bytes)).await,
            Ok(())
        );
    }

    #[tokio::test]
    async fn short_stream_is_corrupt() {
        let bytes = b"canonical attachment bytes";

        assert_eq!(
            verify_stream(
                Box::new(Cursor::new(bytes[..bytes.len() - 1].to_vec())),
                expected(bytes),
            )
            .await,
            Err(StreamVerificationFailure::Corrupt)
        );
    }

    #[tokio::test]
    async fn long_stream_is_rejected_when_catalog_length_is_exceeded() {
        let bytes = b"canonical attachment bytes";
        let mut longer = bytes.to_vec();
        longer.push(b'!');

        assert_eq!(
            verify_stream(Box::new(Cursor::new(longer)), expected(bytes)).await,
            Err(StreamVerificationFailure::Corrupt)
        );
    }

    #[tokio::test]
    async fn equal_length_digest_mismatch_is_corrupt() {
        let bytes = b"canonical attachment bytes";
        let different = b"canonical attachment byte!";
        assert_eq!(bytes.len(), different.len());

        assert_eq!(
            verify_stream(Box::new(Cursor::new(different.to_vec())), expected(bytes)).await,
            Err(StreamVerificationFailure::Corrupt)
        );
    }
}
