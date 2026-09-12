//! Pre-provider verification of rendered attachment authority.

use std::{
    collections::{BTreeSet, HashSet},
    future::Future,
    sync::Arc,
};

use futures_util::FutureExt;

use signalbox_application::{
    AttachmentPreparationFailure, ModelCallCapabilityPreparation, ModelCallInputTokenCount,
    ModelCallInputTokenCounter, ModelCallProvider, PreparedModelOperation,
};
use signalbox_blob_store::BlobStoreFailureKind;
use signalbox_domain::{BlobDigest, PreparedModelCallRequest, ResolvedProviderTarget};
use signalbox_persistence::blob::{
    BlobCatalogEntry, BlobCatalogRepository, BlobCatalogRepositoryError,
};
use sqlx::PgPool;

use crate::BlobStoreRegistry;

// The attachment-preparation admission bound is independent of direct reads.
static PREPARATION_BUDGET: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(8);

/// Provider wrapper that verifies every rendered attachment after capability
/// preparation and before send authorization can begin.
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

        let cancellation = cancellation.shared();
        let request = operation.request().clone();
        prepare_attachment_capability(
            self.inner
                .prepare_capability(operation, cancellation.clone()),
            prepare_attachments(&self.catalog, self.registry.as_deref(), &request, digests),
            cancellation,
        )
        .await
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

async fn prepare_attachment_capability<Capability, Error>(
    capability: impl Future<Output = Result<ModelCallCapabilityPreparation<Capability>, Error>>,
    attachments: impl Future<Output = Result<(), AttachmentPreparationFailure>>,
    cancellation: impl Future<Output = ()>,
) -> Result<ModelCallCapabilityPreparation<Capability>, Error> {
    tokio::pin!(capability, attachments, cancellation);
    let prepared = tokio::select! {
        biased;
        () = &mut cancellation => return Ok(ModelCallCapabilityPreparation::Cancelled),
        prepared = &mut capability => prepared?,
    };
    let ModelCallCapabilityPreparation::Ready(capability) = prepared else {
        return Ok(prepared);
    };
    let prepared = tokio::select! {
        biased;
        () = &mut cancellation => return Ok(ModelCallCapabilityPreparation::Cancelled),
        prepared = &mut attachments => prepared,
    };
    if let Err(failure) = prepared {
        return Ok(ModelCallCapabilityPreparation::AttachmentFailure(failure));
    }
    Ok(ModelCallCapabilityPreparation::Ready(capability))
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
    verify_attachments(catalog, registry, Some(request), digests).await
}

pub(crate) async fn verify_attachments(
    catalog: &BlobCatalogRepository,
    registry: Option<&BlobStoreRegistry>,
    request: Option<&PreparedModelCallRequest>,
    digests: BTreeSet<BlobDigest>,
) -> Result<(), AttachmentPreparationFailure> {
    if digests.is_empty() {
        return Ok(());
    }
    bounded_attachment_preparation(
        &PREPARATION_BUDGET,
        prepare_attachments_inner(catalog, registry, request, digests),
    )
    .await
}

async fn bounded_attachment_preparation<F>(
    budget: &tokio::sync::Semaphore,
    traversal: F,
) -> Result<(), AttachmentPreparationFailure>
where
    F: Future<Output = Result<(), AttachmentPreparationFailure>>,
{
    let _permit = budget
        .try_acquire()
        .map_err(|_| AttachmentPreparationFailure::Unavailable)?;
    signalbox_application::with_scheduler_slot_released(async move {
        let _permit = _permit;
        within_attachment_deadline(traversal).await
    })
    .await
}

async fn within_attachment_deadline(
    preparation: impl Future<Output = Result<(), AttachmentPreparationFailure>>,
) -> Result<(), AttachmentPreparationFailure> {
    tokio::time::timeout(crate::blob_read_runtime::BLOB_READ_TIMEOUT, preparation)
        .await
        .unwrap_or(Err(AttachmentPreparationFailure::Unavailable))
}

async fn prepare_attachments_inner(
    catalog: &BlobCatalogRepository,
    registry: Option<&BlobStoreRegistry>,
    request: Option<&PreparedModelCallRequest>,
    digests: BTreeSet<BlobDigest>,
) -> Result<(), AttachmentPreparationFailure> {
    let Some(registry) = registry else {
        return Err(AttachmentPreparationFailure::Corrupt);
    };
    let mut entries = Vec::with_capacity(digests.len());
    for digest in digests {
        let entry = catalog
            .find(digest)
            .await
            .map_err(map_catalog_failure)?
            .ok_or(AttachmentPreparationFailure::Missing)?;
        let expected = entry.expected();
        if request.is_some_and(|request| {
            request
                .attachment_byte_length(digest)
                .map(|length| length.get())
                != Some(expected.byte_length())
        }) {
            return Err(AttachmentPreparationFailure::Corrupt);
        }
        if expected.byte_length() > registry.max_blob_bytes() {
            return Err(AttachmentPreparationFailure::TooLarge {
                maximum_bytes: registry.max_blob_bytes(),
            });
        }
        entries.push(entry);
    }
    let verifications = entries
        .iter()
        .map(|entry| verify_entry(registry, entry))
        .collect::<Vec<_>>();
    verify_attachment_entries(verifications).await
}

async fn verify_attachment_entries(
    entries: impl IntoIterator<Item = impl Future<Output = Result<(), AttachmentPreparationFailure>>>,
) -> Result<(), AttachmentPreparationFailure> {
    let mut outcome = Ok(());
    for entry in entries {
        match entry.await {
            Ok(()) => {}
            Err(AttachmentPreparationFailure::Unavailable) => {
                outcome = Err(AttachmentPreparationFailure::Unavailable);
            }
            Err(terminal) => return Err(terminal),
        }
    }
    outcome
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
                return Ok(());
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
    use signalbox_application::{AttachmentPreparationFailure, ModelCallInputTokenCount};

    use super::attachment_count_failure;

    #[tokio::test]
    async fn a_prepared_capability_survives_successful_attachment_verification() {
        let result = super::prepare_attachment_capability(
            std::future::ready(Ok::<_, std::convert::Infallible>(
                signalbox_application::ModelCallCapabilityPreparation::Ready("call capability"),
            )),
            std::future::ready(Ok(())),
            std::future::pending(),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            signalbox_application::ModelCallCapabilityPreparation::Ready("call capability")
        ));
    }

    #[tokio::test]
    async fn cancellation_after_capability_preparation_interrupts_attachment_verification() {
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let result = super::prepare_attachment_capability(
            async {
                cancel.send(()).unwrap();
                Ok::<_, std::convert::Infallible>(
                    signalbox_application::ModelCallCapabilityPreparation::Ready(()),
                )
            },
            std::future::pending(),
            async { cancelled.await.unwrap() },
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            signalbox_application::ModelCallCapabilityPreparation::Cancelled
        ));
    }

    #[tokio::test]
    async fn capability_failure_is_discovered_before_unavailable_attachments() {
        let result = super::prepare_attachment_capability(
            std::future::ready(Ok::<_, std::convert::Infallible>(
                signalbox_application::ModelCallCapabilityPreparation::<()>::KnownFailure,
            )),
            std::future::ready(Err(AttachmentPreparationFailure::Unavailable)),
            std::future::pending(),
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            signalbox_application::ModelCallCapabilityPreparation::KnownFailure
        ));
    }

    #[tokio::test]
    async fn terminal_attachment_failure_wins_over_temporary_loss_in_either_order() {
        use AttachmentPreparationFailure::{Corrupt, Missing, Unavailable};
        for terminal in [Missing, Corrupt] {
            for failures in [[Unavailable, terminal], [terminal, Unavailable]] {
                let outcome = super::verify_attachment_entries(
                    failures.map(|failure| std::future::ready(Err(failure))),
                )
                .await;
                assert_eq!(outcome, Err(terminal), "attachment failures: {failures:?}");
            }
        }
    }

    #[tokio::test]
    async fn healthy_later_attachment_does_not_erase_temporary_loss() {
        let outcome = super::verify_attachment_entries([
            std::future::ready(Err(AttachmentPreparationFailure::Unavailable)),
            std::future::ready(Ok(())),
        ])
        .await;
        assert_eq!(outcome, Err(AttachmentPreparationFailure::Unavailable));
    }

    #[tokio::test(start_paused = true)]
    async fn preparation_rejects_immediately_when_all_eight_traversals_are_active() {
        let budget = tokio::sync::Semaphore::new(8);
        let held = budget.try_acquire_many(8).expect("eight preparations fit");
        assert_eq!(
            super::bounded_attachment_preparation(&budget, async {
                panic!("a ninth traversal must not start")
            })
            .await,
            Err(AttachmentPreparationFailure::Unavailable)
        );
        drop(held);
    }

    #[tokio::test(start_paused = true)]
    async fn attachment_candidates_share_one_aggregate_deadline() {
        let deadline = crate::blob_read_runtime::BLOB_READ_TIMEOUT;
        let started = tokio::time::Instant::now();
        let outcome = super::within_attachment_deadline(async {
            tokio::time::sleep(deadline * 3 / 4).await;
            tokio::time::sleep(deadline * 3 / 4).await;
            Ok(())
        })
        .await;

        assert_eq!(outcome, Err(AttachmentPreparationFailure::Unavailable));
        assert_eq!(started.elapsed(), deadline);
    }

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
}
