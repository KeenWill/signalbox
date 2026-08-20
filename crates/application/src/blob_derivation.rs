//! Lazy deterministic blob derivation orchestration.

use std::{error::Error, fmt, future::Future};

use signalbox_domain::{
    BlobDerivation, BlobDerivationError, BlobDerivationId, BlobDerivationProducer, BlobDigest,
    BlobTransformation, DeterministicBlobDerivationKey,
};

/// Application effect supplying fresh derivation fact identities.
pub trait BlobDerivationIdGenerator {
    fn next_blob_derivation_id(&mut self) -> BlobDerivationId;
}

/// Production UUIDv7 identity generator.
#[derive(Clone, Copy, Debug, Default)]
pub struct UuidV7BlobDerivationIdGenerator;

impl BlobDerivationIdGenerator for UuidV7BlobDerivationIdGenerator {
    fn next_blob_derivation_id(&mut self) -> BlobDerivationId {
        BlobDerivationId::from_uuid(uuid::Uuid::now_v7())
    }
}

/// Durable read/append boundary for immutable derivation records.
pub trait BlobDerivationStore {
    type Error;

    fn find_deterministic(
        &self,
        key: DeterministicBlobDerivationKey,
    ) -> impl Future<Output = Result<Option<BlobDerivation>, Self::Error>> + Send;

    fn record_deterministic(
        &self,
        key: DeterministicBlobDerivationKey,
        derivation: BlobDerivation,
    ) -> impl Future<Output = Result<BlobDerivationRecordOutcome, Self::Error>> + Send;
}

/// Result of an append racing on one deterministic cache key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlobDerivationRecordOutcome {
    Recorded(BlobDerivation),
    Existing(BlobDerivation),
}

/// Effect that materializes and registers deterministic output blobs.
pub trait DeterministicBlobProducer {
    type Error;

    fn produce(
        &mut self,
        inputs: &[BlobDigest],
        transformation: &BlobTransformation,
    ) -> impl Future<Output = Result<Box<[BlobDigest]>, Self::Error>> + Send;
}

/// Checked request whose cache key is fixed before external work starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeterministicBlobDerivationRequest {
    inputs: Box<[BlobDigest]>,
    transformation: BlobTransformation,
    implementation: BlobDigest,
    key: DeterministicBlobDerivationKey,
}

impl DeterministicBlobDerivationRequest {
    pub fn try_new(
        inputs: impl Into<Box<[BlobDigest]>>,
        transformation: BlobTransformation,
        implementation: BlobDigest,
    ) -> Result<Self, BlobDerivationError> {
        let inputs = inputs.into();
        let key =
            DeterministicBlobDerivationKey::try_derive(&inputs, &transformation, implementation)?;
        Ok(Self {
            inputs,
            transformation,
            implementation,
            key,
        })
    }

    pub fn inputs(&self) -> &[BlobDigest] {
        &self.inputs
    }

    pub const fn transformation(&self) -> &BlobTransformation {
        &self.transformation
    }

    pub const fn implementation(&self) -> BlobDigest {
        self.implementation
    }

    pub const fn key(&self) -> DeterministicBlobDerivationKey {
        self.key
    }
}

/// Whether this invocation reused an existing fact or won its append.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BlobDerivationServiceOutcome {
    Reused(BlobDerivation),
    Produced(BlobDerivation),
}

/// Closed application-stage failure retaining adapter-specific causes.
#[derive(Debug)]
pub enum BlobDerivationServiceError<StoreError, ProducerError> {
    Store(StoreError),
    Producer(ProducerError),
    InvalidProducerOutput(BlobDerivationError),
}

impl<StoreError: fmt::Display, ProducerError: fmt::Display> fmt::Display
    for BlobDerivationServiceError<StoreError, ProducerError>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "blob derivation store failed: {error}"),
            Self::Producer(error) => write!(formatter, "blob derivative producer failed: {error}"),
            Self::InvalidProducerOutput(error) => error.fmt(formatter),
        }
    }
}

impl<StoreError: Error + 'static, ProducerError: Error + 'static> Error
    for BlobDerivationServiceError<StoreError, ProducerError>
{
}

/// Coordinates cache lookup, isolated production, and append-only provenance.
#[derive(Debug)]
pub struct DeterministicBlobDerivationService<Ids, Store, Producer> {
    ids: Ids,
    store: Store,
    producer: Producer,
}

impl<Ids, Store, Producer> DeterministicBlobDerivationService<Ids, Store, Producer> {
    pub const fn new(ids: Ids, store: Store, producer: Producer) -> Self {
        Self {
            ids,
            store,
            producer,
        }
    }
}

impl<Ids, Store, Producer> DeterministicBlobDerivationService<Ids, Store, Producer>
where
    Ids: BlobDerivationIdGenerator,
    Store: BlobDerivationStore,
    Producer: DeterministicBlobProducer,
{
    pub async fn execute(
        &mut self,
        request: DeterministicBlobDerivationRequest,
    ) -> Result<
        BlobDerivationServiceOutcome,
        BlobDerivationServiceError<Store::Error, Producer::Error>,
    > {
        if let Some(existing) = self
            .store
            .find_deterministic(request.key())
            .await
            .map_err(BlobDerivationServiceError::Store)?
        {
            return Ok(BlobDerivationServiceOutcome::Reused(existing));
        }
        let outputs = self
            .producer
            .produce(request.inputs(), request.transformation())
            .await
            .map_err(BlobDerivationServiceError::Producer)?;
        let derivation = BlobDerivation::try_new(
            self.ids.next_blob_derivation_id(),
            request.inputs,
            request.transformation,
            BlobDerivationProducer::Deterministic {
                implementation: request.implementation,
            },
            outputs,
        )
        .map_err(BlobDerivationServiceError::InvalidProducerOutput)?;
        match self
            .store
            .record_deterministic(request.key, derivation)
            .await
            .map_err(BlobDerivationServiceError::Store)?
        {
            BlobDerivationRecordOutcome::Recorded(recorded) => {
                Ok(BlobDerivationServiceOutcome::Produced(recorded))
            }
            BlobDerivationRecordOutcome::Existing(existing) => {
                Ok(BlobDerivationServiceOutcome::Reused(existing))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "application fixtures use explicit expectations"
    )]

    use std::{
        convert::Infallible,
        future::ready,
        sync::{Arc, Mutex},
    };

    use signalbox_domain::BlobTransformationName;

    use super::*;

    #[derive(Clone, Default)]
    struct FakeStore(Arc<Mutex<Option<(DeterministicBlobDerivationKey, BlobDerivation)>>>);

    impl BlobDerivationStore for FakeStore {
        type Error = Infallible;

        async fn find_deterministic(
            &self,
            key: DeterministicBlobDerivationKey,
        ) -> Result<Option<BlobDerivation>, Self::Error> {
            Ok(self
                .0
                .lock()
                .expect("fixture store lock is healthy")
                .as_ref()
                .filter(|(stored, _)| *stored == key)
                .map(|(_, value)| value.clone()))
        }

        async fn record_deterministic(
            &self,
            key: DeterministicBlobDerivationKey,
            derivation: BlobDerivation,
        ) -> Result<BlobDerivationRecordOutcome, Self::Error> {
            let mut slot = self.0.lock().expect("fixture store lock is healthy");
            if let Some((_, existing)) = slot.as_ref() {
                return Ok(BlobDerivationRecordOutcome::Existing(existing.clone()));
            }
            *slot = Some((key, derivation.clone()));
            Ok(BlobDerivationRecordOutcome::Recorded(derivation))
        }
    }

    #[derive(Clone)]
    struct FakeProducer {
        output: BlobDigest,
        calls: Arc<Mutex<u64>>,
    }

    impl DeterministicBlobProducer for FakeProducer {
        type Error = Infallible;

        fn produce(
            &mut self,
            _inputs: &[BlobDigest],
            _transformation: &BlobTransformation,
        ) -> impl Future<Output = Result<Box<[BlobDigest]>, Self::Error>> + Send {
            *self.calls.lock().expect("fixture counter lock is healthy") += 1;
            ready(Ok(Vec::from([self.output]).into_boxed_slice()))
        }
    }

    #[derive(Clone, Copy)]
    struct FixedIds;

    impl BlobDerivationIdGenerator for FixedIds {
        fn next_blob_derivation_id(&mut self) -> BlobDerivationId {
            BlobDerivationId::from_uuid(uuid::Uuid::from_u128(7))
        }
    }

    fn request() -> DeterministicBlobDerivationRequest {
        DeterministicBlobDerivationRequest::try_new(
            [BlobDigest::digest(b"input")],
            BlobTransformation::try_new(
                BlobTransformationName::try_new("image.thumbnail").expect("fixture name is valid"),
                1,
                &serde_json::json!({"edge_px": 256}),
            )
            .expect("fixture transformation is valid"),
            BlobDigest::digest(b"implementation"),
        )
        .expect("fixture request is valid")
    }

    #[tokio::test]
    async fn repeated_request_reuses_the_recorded_output_without_reproduction() {
        let calls = Arc::new(Mutex::new(0));
        let producer = FakeProducer {
            output: BlobDigest::digest(b"output"),
            calls: calls.clone(),
        };
        let mut service =
            DeterministicBlobDerivationService::new(FixedIds, FakeStore::default(), producer);

        let first = service
            .execute(request())
            .await
            .expect("first production succeeds");
        let replay = service
            .execute(request())
            .await
            .expect("cache replay succeeds");

        let BlobDerivationServiceOutcome::Produced(_) = first else {
            panic!("first invocation did not produce the derivative");
        };
        let BlobDerivationServiceOutcome::Reused(_) = replay else {
            panic!("second invocation did not reuse the derivative");
        };
        assert_eq!(*calls.lock().expect("fixture counter lock is healthy"), 1);
    }
}
