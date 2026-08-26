//! S3-compatible adapter for the immutable blob-store contract.
//!
//! Request signing is delegated to `rusty-s3`; transport, credentials, bounds,
//! and publication verification remain explicit in this adapter.

use std::{
    error::Error,
    fmt,
    fs::File,
    io::{self, Read},
    num::NonZeroU64,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use http_body::{Body, Frame, SizeHint};
use instant_xml::FromXml;
use jiff::Timestamp;
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue},
    redirect::Policy,
};
use rustix::fs::{Mode, OFlags, openat};
use rusty_s3::{Bucket, Credentials, Method, S3Action, UrlStyle, signing::sign};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use signalbox_blob_store::{
    BlobObjectKey, BlobPutOutcome, BlobReader, BlobStore, BlobStoreError, BlobStoreFuture,
    BlobVerificationFailure, ExpectedBlob, MAX_BLOB_RANGE_BYTES, OpenedBlob,
};
use signalbox_domain::BlobDigest;
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex as AsyncMutex, mpsc, watch},
    task::JoinHandle,
};
use tokio_util::io::StreamReader;
use url::Url;
use zeroize::Zeroize;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const OPERATION_DEADLINE: Duration = Duration::from_secs(24 * 60 * 60);
const SIGNED_URL_LIFETIME: Duration = OPERATION_DEADLINE;
const MAX_CREDENTIAL_FILE_BYTES: u64 = 16_384;
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const STREAM_CHANNEL_CHUNKS: usize = 4;
const MIN_MULTIPART_PART_BYTES: u64 = 5 * 1024 * 1024;
const MAX_MULTIPART_PART_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_MULTIPART_PARTS: u64 = 10_000;
const MAX_S3_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024 * 1024;
const MAX_CREATE_RESPONSE_BYTES: usize = 65_536;
const MAX_COMPLETE_RESPONSE_BYTES: usize = 65_536;
const MAX_UPLOAD_ID_BYTES: usize = 1_024;
const MAX_ETAG_BYTES: usize = 256;
const MAX_COMPLETE_BODY_BYTES: usize = 3 * 1024 * 1024;
const PUBLICATION_LOCK_STRIPES: usize = 64;
const NAMESPACE_MARKER_KEY: &str = ".signalbox-blob-namespace-v1";
const MAX_NAMESPACE_MARKER_BYTES: usize = 128;
const MAX_LIFECYCLE_RESPONSE_BYTES: usize = 65_536;
const MAX_ERROR_RESPONSE_BYTES: usize = 65_536;
const S3_XML_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";
const BLOB_KEY_PREFIX: &str = "sha256/";
const OBJECT_ABSENCE_CODE: &str = "NoSuchKey";

/// One path-style S3-compatible immutable-object store.
pub struct S3BlobStore {
    bucket: Bucket,
    credentials_file: PathBuf,
    client: Client,
    publication_locks: Box<[AsyncMutex<()>]>,
    namespace_marker: Option<NamespaceMarker>,
}

#[derive(Default)]
struct PutReconciliation {
    credentials: Option<Credentials>,
    replacing: Option<bool>,
}

struct MultipartAbortGuard {
    client: Client,
    signed_abort: Option<Url>,
}

enum CompletionFailure {
    Definite(BlobStoreError),
    PossiblyAccepted,
}

impl MultipartAbortGuard {
    const fn new(client: Client, signed_abort: Url) -> Self {
        Self {
            client,
            signed_abort: Some(signed_abort),
        }
    }

    async fn abort(&mut self) {
        let Some(signed_abort) = self.signed_abort.as_ref().cloned() else {
            return;
        };
        let _ = self.client.delete(signed_abort).send().await;
        self.signed_abort = None;
    }

    fn disarm(&mut self) {
        self.signed_abort = None;
    }
}

impl Drop for MultipartAbortGuard {
    fn drop(&mut self) {
        let Some(signed_abort) = self.signed_abort.take() else {
            return;
        };
        let client = self.client.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = client.delete(signed_abort).send().await;
            });
        }
    }
}

impl fmt::Debug for S3BlobStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("S3BlobStore { endpoint: <redacted>, bucket: <redacted> }")
    }
}

impl S3BlobStore {
    /// Constructs one adapter without reading credentials or contacting S3.
    pub fn try_new(
        endpoint: Url,
        region: impl Into<String>,
        bucket: impl Into<String>,
        credentials_file: PathBuf,
    ) -> Result<Self, S3BlobStoreConstructionError> {
        if !credentials_file.is_absolute() {
            return Err(S3BlobStoreConstructionError::CredentialPath);
        }
        let bucket = Bucket::new(endpoint, UrlStyle::Path, bucket.into(), region.into())
            .map_err(|_| S3BlobStoreConstructionError::Endpoint)?;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = Client::builder()
            .tls_backend_rustls()
            .tls_version_min(reqwest::tls::Version::TLS_1_2)
            .tls_danger_accept_invalid_certs(false)
            .tls_danger_accept_invalid_hostnames(false)
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .pool_max_idle_per_host(0)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(IDLE_TIMEOUT)
            .timeout(OPERATION_DEADLINE)
            .build()
            .map_err(|_| S3BlobStoreConstructionError::Transport)?;
        Ok(Self {
            bucket,
            credentials_file,
            client,
            publication_locks: (0..PUBLICATION_LOCK_STRIPES)
                .map(|_| AsyncMutex::new(()))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            namespace_marker: None,
        })
    }

    /// Constructs an adapter that authenticates one durable bucket namespace.
    pub fn try_new_bound(
        endpoint: Url,
        region: impl Into<String>,
        bucket: impl Into<String>,
        credentials_file: PathBuf,
        namespace_marker_body: impl Into<String>,
    ) -> Result<Self, S3BlobStoreConstructionError> {
        let marker_body = namespace_marker_body.into();
        if marker_body.is_empty() || marker_body.len() > MAX_NAMESPACE_MARKER_BYTES {
            return Err(S3BlobStoreConstructionError::NamespaceMarker);
        }
        let mut store = Self::try_new(endpoint, region, bucket, credentials_file)?;
        store.namespace_marker = Some(NamespaceMarker {
            body: Arc::from(marker_body),
            verification: NamespaceVerification::default(),
        });
        Ok(store)
    }

    /// Authenticates or creates the reserved namespace marker for a routed store.
    pub async fn prepare_namespace(
        &self,
        state: S3NamespaceBindingState,
    ) -> Result<(), BlobStoreError> {
        let Some(marker) = &self.namespace_marker else {
            return Err(BlobStoreError::unavailable("configure S3 namespace marker"));
        };
        let result = match state {
            S3NamespaceBindingState::New => self.create_namespace_marker(marker).await,
            S3NamespaceBindingState::Recorded => self.verify_namespace_marker(marker).await,
        };
        if result.is_ok() {
            marker.verification.record(true);
        }
        result
    }

    /// Proves the routed bucket aborts incomplete multipart uploads after one day.
    pub async fn verify_multipart_lifecycle(&self) -> Result<(), BlobStoreError> {
        let credentials = self.credentials().await?;
        let url = sign(
            &Timestamp::now(),
            Method::Get,
            self.bucket.base_url().clone(),
            credentials.key(),
            credentials.secret(),
            credentials.token(),
            self.bucket.region(),
            SIGNED_URL_LIFETIME.as_secs(),
            std::iter::once(("lifecycle", "")),
            std::iter::empty(),
        );
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|_| BlobStoreError::io("read S3 lifecycle", SanitizedS3Failure))?;
        let response = require_success(response, "read S3 lifecycle").await?;
        let body =
            bounded_response(response, MAX_LIFECYCLE_RESPONSE_BYTES, "read S3 lifecycle").await?;
        let body = std::str::from_utf8(&body)
            .map_err(|_| BlobStoreError::unavailable("parse S3 lifecycle"))?;
        let lifecycle: LifecycleConfiguration = instant_xml::from_str(body)
            .map_err(|_| BlobStoreError::unavailable("parse S3 lifecycle"))?;
        if lifecycle.rules.iter().any(LifecycleRule::covers_blobs) {
            Ok(())
        } else {
            Err(BlobStoreError::unavailable("verify S3 multipart lifecycle"))
        }
    }

    async fn ensure_namespace_ready(
        &self,
        credentials: &Credentials,
    ) -> Result<(), BlobStoreError> {
        let Some(marker) = &self.namespace_marker else {
            return Ok(());
        };
        if marker.verification.proved.load(Ordering::Acquire) {
            return Ok(());
        }
        let arrived_at = marker.verification.arrive();
        let _gate = marker.verification.gate.lock().await;
        match marker.verification.probe(arrived_at) {
            NamespaceProbe::Proved => return Ok(()),
            NamespaceProbe::Adopt => {
                return Err(BlobStoreError::unavailable("verify S3 namespace marker"));
            }
            NamespaceProbe::Run => {}
        }
        let result = self.verify_namespace_marker_with(credentials, marker).await;
        marker.verification.record(result.is_ok());
        result
    }

    async fn create_namespace_marker(
        &self,
        marker: &NamespaceMarker,
    ) -> Result<(), BlobStoreError> {
        let credentials = self.credentials().await?;
        let mut action = self
            .bucket
            .put_object(Some(&credentials), NAMESPACE_MARKER_KEY);
        action.headers_mut().insert("if-none-match", "*");
        let response = self
            .client
            .put(action.sign(SIGNED_URL_LIFETIME))
            .header(reqwest::header::IF_NONE_MATCH, "*")
            .header(reqwest::header::CONTENT_LENGTH, marker.body.len())
            .body(String::from(marker.body.as_ref()))
            .send()
            .await
            .map_err(|_| BlobStoreError::io("create S3 namespace marker", SanitizedS3Failure))?;
        if !response.status().is_success() && response.status() != StatusCode::PRECONDITION_FAILED {
            return Err(BlobStoreError::unavailable("create S3 namespace marker"));
        }
        self.verify_namespace_marker_with(&credentials, marker)
            .await
    }

    async fn verify_namespace_marker(
        &self,
        marker: &NamespaceMarker,
    ) -> Result<(), BlobStoreError> {
        let credentials = self.credentials().await?;
        self.verify_namespace_marker_with(&credentials, marker)
            .await
    }

    async fn verify_namespace_marker_with(
        &self,
        credentials: &Credentials,
        marker: &NamespaceMarker,
    ) -> Result<(), BlobStoreError> {
        let action = self
            .bucket
            .get_object(Some(credentials), NAMESPACE_MARKER_KEY);
        let response = self
            .client
            .get(action.sign(SIGNED_URL_LIFETIME))
            .send()
            .await
            .map_err(|_| BlobStoreError::io("read S3 namespace marker", SanitizedS3Failure))?;
        let response = require_success(response, "read S3 namespace marker").await?;
        let body = bounded_response(
            response,
            MAX_NAMESPACE_MARKER_BYTES,
            "read S3 namespace marker",
        )
        .await?;
        if body.as_slice() == marker.body.as_bytes() {
            Ok(())
        } else {
            Err(BlobStoreError::unavailable("verify S3 namespace marker"))
        }
    }

    async fn credentials(&self) -> Result<Credentials, BlobStoreError> {
        let path = self.credentials_file.clone();
        let document = tokio::task::spawn_blocking(move || read_credentials(&path))
            .await
            .map_err(|_| BlobStoreError::io("join S3 credential read", SanitizedS3Failure))?
            .map_err(|_| BlobStoreError::io("read S3 credentials", SanitizedS3Failure))?;
        Ok(document.into_credentials())
    }

    async fn put_inner(
        &self,
        expected: ExpectedBlob,
        source: BlobReader,
        reconciliation: &StdMutex<PutReconciliation>,
    ) -> Result<BlobPutOutcome, BlobStoreError> {
        let credentials = self.credentials().await?;
        self.ensure_namespace_ready(&credentials).await?;
        let key = BlobObjectKey::for_digest(expected.digest());
        let stripe = usize::from(expected.digest().as_bytes()[0]) % PUBLICATION_LOCK_STRIPES;
        let _publication = self.publication_locks[stripe].lock().await;
        reconciliation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .credentials = Some(credentials.clone());
        let replacing = match self.verify_object(&credentials, &key, expected).await {
            Ok(()) => return Ok(BlobPutOutcome::AlreadyPresent { key }),
            Err(error)
                if matches!(
                    error.kind(),
                    signalbox_blob_store::BlobStoreFailureKind::NotFound
                        | signalbox_blob_store::BlobStoreFailureKind::VerificationFailed
                ) =>
            {
                error.kind() == signalbox_blob_store::BlobStoreFailureKind::VerificationFailed
            }
            Err(error) => return Err(error),
        };
        reconciliation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replacing = Some(replacing);

        self.multipart_publish(&credentials, &key, expected, source)
            .await?;
        if let Err(error) = self.verify_object(&credentials, &key, expected).await {
            if error.kind() == signalbox_blob_store::BlobStoreFailureKind::Unavailable {
                return Err(BlobStoreError::publication_ambiguous(
                    "reconcile S3 post-publication verification",
                ));
            }
            return Err(error);
        }
        if replacing {
            Ok(BlobPutOutcome::Repaired { key })
        } else {
            Ok(BlobPutOutcome::Published { key })
        }
    }

    async fn multipart_publish(
        &self,
        credentials: &Credentials,
        key: &BlobObjectKey,
        expected: ExpectedBlob,
        source: BlobReader,
    ) -> Result<(), BlobStoreError> {
        if multipart_part_bytes(expected.byte_length()).is_none() {
            return Err(BlobStoreError::unavailable("bound S3 multipart object"));
        }
        let create = self
            .bucket
            .create_multipart_upload(Some(credentials), key.as_str());
        let response = self
            .client
            .post(create.sign(SIGNED_URL_LIFETIME))
            .body(Bytes::new())
            .send()
            .await
            .map_err(|_| BlobStoreError::io("create S3 multipart upload", SanitizedS3Failure))?;
        let response = require_success(response, "create S3 multipart upload").await?;
        let body =
            bounded_response(response, MAX_CREATE_RESPONSE_BYTES, "read S3 upload id").await?;
        let body = std::str::from_utf8(&body)
            .map_err(|_| BlobStoreError::unavailable("parse S3 upload id"))?;
        let parsed = rusty_s3::actions::CreateMultipartUpload::parse_response(body)
            .map_err(|_| BlobStoreError::unavailable("parse S3 upload id"))?;
        let upload_id = parsed.upload_id();
        if upload_id.is_empty() || upload_id.len() > MAX_UPLOAD_ID_BYTES {
            return Err(BlobStoreError::unavailable("bound S3 upload id"));
        }
        let abort_action =
            self.bucket
                .abort_multipart_upload(Some(credentials), key.as_str(), upload_id);
        let mut abort_guard =
            MultipartAbortGuard::new(self.client.clone(), abort_action.sign(SIGNED_URL_LIFETIME));

        let result = self
            .upload_parts(credentials, key, upload_id, expected, source)
            .await;
        if result.is_err() {
            abort_guard.abort().await;
        } else {
            abort_guard.disarm();
        }
        result
    }

    async fn upload_parts(
        &self,
        credentials: &Credentials,
        key: &BlobObjectKey,
        upload_id: &str,
        expected: ExpectedBlob,
        source: BlobReader,
    ) -> Result<(), BlobStoreError> {
        let part_bytes = multipart_part_bytes(expected.byte_length())
            .ok_or_else(|| BlobStoreError::unavailable("bound S3 multipart object"))?;
        let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CHUNKS);
        let stream_state = Arc::new(StdMutex::new(UploadStreamState {
            receiver,
            pending: None,
        }));
        let mut producer = Some(tokio::spawn(produce_source(source, expected, sender)));
        let mut etags = Vec::new();
        let mut offset = 0_u64;
        let mut part_number = 1_u16;

        while offset < expected.byte_length() {
            let length = part_bytes.min(expected.byte_length() - offset);
            let (progress, observed_progress) = watch::channel(0_u64);
            let body = ExactUploadBody::new(Arc::clone(&stream_state), length, progress);
            let action =
                self.bucket
                    .upload_part(Some(credentials), key.as_str(), part_number, upload_id);
            let request = self
                .client
                .put(action.sign(SIGNED_URL_LIFETIME))
                .header(reqwest::header::CONTENT_LENGTH, length)
                .body(reqwest::Body::wrap(body));
            let response =
                match send_with_upload_idle_timeout(request, observed_progress, length).await {
                    Ok(response) => response,
                    Err(_) => {
                        return source_or_transport_failure(
                            producer.take(),
                            "upload S3 multipart part",
                        )
                        .await;
                    }
                };
            let response = require_success(response, "upload S3 multipart part").await?;
            let etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty() && value.len() <= MAX_ETAG_BYTES)
                .ok_or_else(|| BlobStoreError::unavailable("read S3 multipart ETag"))?;
            etags.push(String::from(etag));
            offset += length;
            part_number = part_number
                .checked_add(1)
                .ok_or_else(|| BlobStoreError::unavailable("bound S3 multipart part count"))?;
        }

        let observed = producer
            .take()
            .ok_or_else(|| BlobStoreError::unavailable("join S3 source stream"))?
            .await
            .map_err(|_| BlobStoreError::io("join S3 source stream", SanitizedS3Failure))??;
        if observed != expected.digest() {
            return Err(BlobStoreError::verification(
                "verify S3 publication source",
                BlobVerificationFailure::new(expected, Some(observed), expected.byte_length()),
            ));
        }

        let etag_refs = etags.iter().map(String::as_str);
        let complete = self.bucket.complete_multipart_upload(
            Some(credentials),
            key.as_str(),
            upload_id,
            etag_refs,
        );
        let signed = complete.sign(SIGNED_URL_LIFETIME);
        let completion_body = complete.body();
        if completion_body.len() > MAX_COMPLETE_BODY_BYTES {
            return Err(BlobStoreError::unavailable("bound S3 completion body"));
        }
        let completion_length = completion_body.len() as u64;
        let (progress, observed_progress) = watch::channel(0_u64);
        let request = self
            .client
            .post(signed)
            .header(reqwest::header::CONTENT_TYPE, "application/xml")
            .header(reqwest::header::CONTENT_LENGTH, completion_length)
            .body(reqwest::Body::wrap(ChunkedProgressBody::new(
                Bytes::from(completion_body),
                progress,
            )));
        let completion = async {
            let response =
                send_with_upload_idle_timeout(request, observed_progress, completion_length)
                    .await
                    .map_err(|()| CompletionFailure::PossiblyAccepted)?;
            if !response.status().is_success() {
                return Err(completion_status_failure(response.status()));
            }
            let body = bounded_response(
                response,
                MAX_COMPLETE_RESPONSE_BYTES,
                "read S3 completion response",
            )
            .await
            .map_err(|_| CompletionFailure::PossiblyAccepted)?;
            validate_completion_response(&body).map_err(|_| CompletionFailure::PossiblyAccepted)?;
            Ok(())
        }
        .await;
        match completion {
            Ok(()) => Ok(()),
            Err(CompletionFailure::Definite(error)) => Err(error),
            Err(CompletionFailure::PossiblyAccepted) => {
                match self.verify_object(credentials, key, expected).await {
                    Ok(()) => Ok(()),
                    Err(_) => Err(BlobStoreError::publication_ambiguous(
                        "reconcile S3 multipart completion",
                    )),
                }
            }
        }
    }

    async fn open_response(
        &self,
        credentials: &Credentials,
        key: &BlobObjectKey,
    ) -> Result<Response, BlobStoreError> {
        let action = self.bucket.get_object(Some(credentials), key.as_str());
        let response = self
            .client
            .get(action.sign(SIGNED_URL_LIFETIME))
            .send()
            .await
            .map_err(|_| BlobStoreError::io("get S3 object", SanitizedS3Failure))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(classify_absence(response, "get S3 object").await);
        }
        require_success(response, "get S3 object").await
    }

    /// Re-reads one object generation under a proved conditional match.
    ///
    /// The signed request carries no `If-Match`, so the header is unsigned and
    /// the generation claim rests on the store honoring the precondition. A
    /// refused precondition proves only that the verified generation is gone,
    /// never that the recorded blob is corrupt, so it stays unavailability.
    async fn open_pinned_response(
        &self,
        credentials: &Credentials,
        key: &BlobObjectKey,
        generation: &HeaderValue,
    ) -> Result<Response, BlobStoreError> {
        let action = self.bucket.get_object(Some(credentials), key.as_str());
        let response = self
            .client
            .get(action.sign(SIGNED_URL_LIFETIME))
            .header(reqwest::header::IF_MATCH, generation.clone())
            .send()
            .await
            .map_err(|_| BlobStoreError::io("get pinned S3 object", SanitizedS3Failure))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(classify_absence(response, "get pinned S3 object").await);
        }
        if response.status() == StatusCode::PRECONDITION_FAILED {
            return Err(BlobStoreError::unavailable("pin verified S3 generation"));
        }
        require_success(response, "get pinned S3 object").await
    }

    async fn verify_object(
        &self,
        credentials: &Credentials,
        key: &BlobObjectKey,
        expected: ExpectedBlob,
    ) -> Result<(), BlobStoreError> {
        let response = self.open_response(credentials, key).await?;
        verify_stream(response_reader(response), expected, "verify S3 object").await
    }

    async fn open_inner(&self, key: &BlobObjectKey) -> Result<OpenedBlob, BlobStoreError> {
        let credentials = self.credentials().await?;
        self.ensure_namespace_ready(&credentials).await?;
        let response = self.open_response(&credentials, key).await?;
        let length = response
            .content_length()
            .ok_or_else(|| BlobStoreError::unavailable("read S3 object length"))?;
        Ok(OpenedBlob::new(length, Box::new(response_reader(response))))
    }

    /// Verifies one complete object generation, then re-opens that same one.
    ///
    /// A remote object cannot be rewound the way a local file handle can, and
    /// the delivered stream may exceed every adapter memory and range bound,
    /// so the verified bytes are not spooled. The generation is pinned by the
    /// entity tag observed on the verifying response instead: the delivering
    /// request repeats that tag as `If-Match`, so the store either serves the
    /// generation this call hashed or refuses to serve anything.
    async fn open_verified_inner(
        &self,
        expected: ExpectedBlob,
        key: &BlobObjectKey,
    ) -> Result<OpenedBlob, BlobStoreError> {
        let credentials = self.credentials().await?;
        self.ensure_namespace_ready(&credentials).await?;
        let response = self.open_response(&credentials, key).await?;
        let generation = object_generation(response.headers())
            .ok_or_else(|| BlobStoreError::unavailable("name S3 object generation"))?;
        verify_stream(
            response_reader(response),
            expected,
            "verify opened S3 object",
        )
        .await?;
        let pinned = self
            .open_pinned_response(&credentials, key, &generation)
            .await?;
        let length = pinned
            .content_length()
            .filter(|length| *length == expected.byte_length())
            .ok_or_else(|| BlobStoreError::unavailable("read pinned S3 object length"))?;
        Ok(OpenedBlob::new(length, Box::new(response_reader(pinned))))
    }

    async fn open_range_inner(
        &self,
        key: &BlobObjectKey,
        expected: ExpectedBlob,
        offset: u64,
        length: u64,
    ) -> Result<OpenedBlob, BlobStoreError> {
        if length == 0 || length > MAX_BLOB_RANGE_BYTES {
            return Err(BlobStoreError::unavailable("validate S3 object range"));
        }
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= expected.byte_length())
            .ok_or_else(|| BlobStoreError::unavailable("validate S3 object range"))?;
        let capacity = usize::try_from(length)
            .map_err(|_| BlobStoreError::unavailable("allocate S3 object range"))?;
        let credentials = self.credentials().await?;
        self.ensure_namespace_ready(&credentials).await?;
        let response = self.open_response(&credentials, key).await?;
        let mut reader = response_reader(response);
        let mut hasher = Sha256::new();
        let mut retained = Vec::with_capacity(capacity);
        let mut observed_length = 0_u64;
        let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
        loop {
            let count = reader.read(&mut buffer).await.map_err(|_| {
                BlobStoreError::io("read S3 range verification", SanitizedS3Failure)
            })?;
            if count == 0 {
                break;
            }
            let chunk_start = observed_length;
            observed_length = observed_length.saturating_add(count as u64);
            if observed_length > expected.byte_length() {
                return Err(BlobStoreError::verification(
                    "verify S3 object range",
                    BlobVerificationFailure::new(expected, None, observed_length),
                ));
            }
            hasher.update(&buffer[..count]);
            let retain_start = offset.max(chunk_start);
            let retain_end = end.min(observed_length);
            if retain_start < retain_end {
                let local_start = usize::try_from(retain_start - chunk_start)
                    .map_err(|_| BlobStoreError::unavailable("retain S3 object range"))?;
                let local_end = usize::try_from(retain_end - chunk_start)
                    .map_err(|_| BlobStoreError::unavailable("retain S3 object range"))?;
                retained.extend_from_slice(&buffer[local_start..local_end]);
            }
        }
        let observed_digest = BlobDigest::from_bytes(hasher.finalize().into());
        if observed_length != expected.byte_length() || observed_digest != expected.digest() {
            return Err(BlobStoreError::verification(
                "verify S3 object range",
                BlobVerificationFailure::new(expected, Some(observed_digest), observed_length),
            ));
        }
        if retained.len() != capacity {
            return Err(BlobStoreError::unavailable(
                "retain complete S3 object range",
            ));
        }
        Ok(OpenedBlob::new(
            length,
            Box::new(std::io::Cursor::new(retained)),
        ))
    }

    /// Removes the deterministic fixture key used by the opt-in live suite.
    #[cfg(feature = "test-support")]
    pub async fn delete_for_conformance(
        &self,
        expected: ExpectedBlob,
    ) -> Result<(), BlobStoreError> {
        let credentials = self.credentials().await?;
        let key = BlobObjectKey::for_digest(expected.digest());
        let action = self.bucket.delete_object(Some(&credentials), key.as_str());
        let response = self
            .client
            .delete(action.sign(SIGNED_URL_LIFETIME))
            .send()
            .await
            .map_err(|_| BlobStoreError::io("delete S3 conformance object", SanitizedS3Failure))?;
        if response.status().is_success() || response.status() == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(BlobStoreError::unavailable("delete S3 conformance object"))
        }
    }

    /// Injects exact corrupt fixture bytes behind a deterministic expected key.
    #[cfg(feature = "test-support")]
    pub async fn corrupt_for_conformance(
        &self,
        expected: ExpectedBlob,
        bytes: &'static [u8],
    ) -> Result<(), BlobStoreError> {
        let credentials = self.credentials().await?;
        let key = BlobObjectKey::for_digest(expected.digest());
        let action = self.bucket.put_object(Some(&credentials), key.as_str());
        let response = self
            .client
            .put(action.sign(SIGNED_URL_LIFETIME))
            .header(reqwest::header::CONTENT_LENGTH, bytes.len())
            .body(bytes)
            .send()
            .await
            .map_err(|_| BlobStoreError::io("inject S3 conformance object", SanitizedS3Failure))?;
        let _ = require_success(response, "inject S3 conformance object").await?;
        Ok(())
    }
}

impl BlobStore for S3BlobStore {
    fn put<'a>(
        &'a self,
        expected: ExpectedBlob,
        source: BlobReader,
    ) -> BlobStoreFuture<'a, BlobPutOutcome> {
        Box::pin(async move {
            let reconciliation = StdMutex::new(PutReconciliation::default());
            match tokio::time::timeout(
                OPERATION_DEADLINE,
                self.put_inner(expected, source, &reconciliation),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    let state = reconciliation
                        .into_inner()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some((credentials, replacing)) = state.credentials.zip(state.replacing)
                    else {
                        return Err(BlobStoreError::unavailable("bound S3 put deadline"));
                    };
                    let key = BlobObjectKey::for_digest(expected.digest());
                    match tokio::time::timeout(
                        OPERATION_DEADLINE,
                        self.verify_object(&credentials, &key, expected),
                    )
                    .await
                    {
                        Ok(Ok(())) if replacing => Ok(BlobPutOutcome::Repaired { key }),
                        Ok(Ok(())) => Ok(BlobPutOutcome::Published { key }),
                        _ => Err(BlobStoreError::publication_ambiguous(
                            "reconcile S3 put deadline",
                        )),
                    }
                }
            }
        })
    }

    fn open<'a>(&'a self, key: &'a BlobObjectKey) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(async move {
            tokio::time::timeout(OPERATION_DEADLINE, self.open_inner(key))
                .await
                .map_err(|_| BlobStoreError::unavailable("bound S3 open deadline"))?
        })
    }

    fn open_verified<'a>(
        &'a self,
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(async move {
            tokio::time::timeout(OPERATION_DEADLINE, self.open_verified_inner(expected, key))
                .await
                .map_err(|_| BlobStoreError::unavailable("bound S3 verified open deadline"))?
        })
    }

    fn open_range<'a>(
        &'a self,
        expected: ExpectedBlob,
        key: &'a BlobObjectKey,
        offset: u64,
        length: NonZeroU64,
    ) -> BlobStoreFuture<'a, OpenedBlob> {
        Box::pin(async move {
            tokio::time::timeout(
                OPERATION_DEADLINE,
                self.open_range_inner(key, expected, offset, length.get()),
            )
            .await
            .map_err(|_| BlobStoreError::unavailable("bound S3 range deadline"))?
        })
    }
}

fn response_reader(response: Response) -> impl tokio::io::AsyncRead + Send + Unpin {
    StreamReader::new(
        response
            .bytes_stream()
            .map_err(|_| io::Error::other("S3 response stream failed")),
    )
}

/// Hashes one complete stream against its expected immutable identity.
///
/// A stream that runs past the expected length fails as soon as the overrun is
/// observed, because no suffix can restore the declared identity and the
/// remainder is unbounded. Every other outcome is decided on the full stream.
async fn verify_stream(
    mut reader: impl tokio::io::AsyncRead + Send + Unpin,
    expected: ExpectedBlob,
    operation: &'static str,
) -> Result<(), BlobStoreError> {
    let mut hasher = Sha256::new();
    let mut observed_length = 0_u64;
    let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| BlobStoreError::io("read S3 object", SanitizedS3Failure))?;
        if count == 0 {
            break;
        }
        observed_length = observed_length.saturating_add(count as u64);
        if observed_length > expected.byte_length() {
            return Err(BlobStoreError::verification(
                operation,
                BlobVerificationFailure::new(expected, None, observed_length),
            ));
        }
        hasher.update(&buffer[..count]);
    }
    let observed_digest = BlobDigest::from_bytes(hasher.finalize().into());
    if observed_length != expected.byte_length() || observed_digest != expected.digest() {
        return Err(BlobStoreError::verification(
            operation,
            BlobVerificationFailure::new(expected, Some(observed_digest), observed_length),
        ));
    }
    Ok(())
}

/// Names the exact object generation a response was served from.
///
/// An absent, oversized, or non-singleton entity tag leaves the generation
/// unnamed, so no later request can prove it read the same bytes this call
/// verified.
fn object_generation(headers: &HeaderMap) -> Option<HeaderValue> {
    headers
        .get(reqwest::header::ETAG)
        .filter(|generation| {
            generation.len() <= MAX_ETAG_BYTES && names_one_strong_generation(generation)
        })
        .cloned()
}

/// Reports whether a served entity tag names exactly one strong generation.
///
/// The delivering request replays this value as `If-Match`, where `*` matches
/// whatever representation is current and a comma-separated list matches any
/// member, so either spelling would let the store serve a generation this call
/// never hashed. `If-Match` also compares strongly, so a weak `W/` tag can
/// never match and is not a usable generation token either. Only one
/// double-quoted tag of at least one entity-tag character — no embedded quote,
/// no comma, no space — names the exact generation the verifying response was
/// served from.
fn names_one_strong_generation(generation: &HeaderValue) -> bool {
    let Some(interior) = generation
        .as_bytes()
        .strip_prefix(b"\"")
        .and_then(|rest| rest.strip_suffix(b"\""))
    else {
        return false;
    };
    !interior.is_empty()
        && interior
            .iter()
            .all(|byte| matches!(byte, 0x21 | 0x23..=0x7e | 0x80..=0xff))
}

async fn require_success(
    response: Response,
    operation: &'static str,
) -> Result<Response, BlobStoreError> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(BlobStoreError::unavailable(operation))
    }
}

async fn send_with_upload_idle_timeout(
    request: reqwest::RequestBuilder,
    mut progress: watch::Receiver<u64>,
    length: u64,
) -> Result<Response, ()> {
    let send = request.send();
    tokio::pin!(send);
    loop {
        if *progress.borrow() >= length {
            return send.await.map_err(|_| ());
        }
        tokio::select! {
            response = &mut send => return response.map_err(|_| ()),
            changed = tokio::time::timeout(IDLE_TIMEOUT, progress.changed()) =>
                changed.map_err(|_| ())?.map_err(|_| ())?,
        }
    }
}

/// Classifies a non-success completion status that may still have published.
///
/// A server or gateway error can be returned after the request reached the
/// backend, so it does not prove rejection and must reach the read-back.
fn completion_status_failure(status: StatusCode) -> CompletionFailure {
    if status.is_server_error() {
        CompletionFailure::PossiblyAccepted
    } else {
        CompletionFailure::Definite(BlobStoreError::unavailable("complete S3 multipart upload"))
    }
}

/// Separates a proved missing object from a backend that did not answer.
///
/// Reporting absence is a durable claim about the blob, so only a parsed
/// object-level absence earns `NotFound`. A bucket-level code, an oversized,
/// truncated, malformed, or non-UTF-8 body, and any unrecognized code all
/// leave absence unproved and stay ordinary unavailability.
async fn classify_absence(response: Response, operation: &'static str) -> BlobStoreError {
    let Ok(body) = bounded_response(response, MAX_ERROR_RESPONSE_BYTES, operation).await else {
        return BlobStoreError::unavailable(operation);
    };
    let Ok(body) = std::str::from_utf8(&body) else {
        return BlobStoreError::unavailable(operation);
    };
    if names_absent_object(body) {
        BlobStoreError::not_found(operation)
    } else {
        BlobStoreError::unavailable(operation)
    }
}

/// Reports whether a bounded S3 error document proves object-level absence.
fn names_absent_object(body: &str) -> bool {
    instant_xml::from_str::<S3ErrorDocument>(body)
        .is_ok_and(|document| document.code.as_deref() == Some(OBJECT_ABSENCE_CODE))
}

fn validate_completion_response(body: &[u8]) -> Result<(), ()> {
    let body = std::str::from_utf8(body).map_err(|_| ())?;
    instant_xml::from_str::<CompleteMultipartUploadResult>(body)
        .map(|_| ())
        .map_err(|_| ())
}

async fn bounded_response(
    response: Response,
    maximum: usize,
    operation: &'static str,
) -> Result<Vec<u8>, BlobStoreError> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BlobStoreError::io(operation, SanitizedS3Failure))?;
        let next = bytes
            .len()
            .checked_add(chunk.len())
            .filter(|next| *next <= maximum)
            .ok_or_else(|| BlobStoreError::unavailable(operation))?;
        bytes.reserve(next - bytes.len());
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn multipart_part_bytes(length: u64) -> Option<u64> {
    if length > MAX_S3_OBJECT_BYTES {
        return None;
    }
    let ceiling = length.div_ceil(MAX_MULTIPART_PARTS);
    let part_bytes = ceiling.max(MIN_MULTIPART_PART_BYTES);
    if part_bytes > MAX_MULTIPART_PART_BYTES {
        None
    } else {
        Some(part_bytes)
    }
}

async fn source_or_transport_failure(
    producer: Option<JoinHandle<Result<BlobDigest, BlobStoreError>>>,
    operation: &'static str,
) -> Result<(), BlobStoreError> {
    let Some(producer) = producer else {
        return Err(BlobStoreError::unavailable(operation));
    };
    if !producer.is_finished() {
        producer.abort();
        return Err(BlobStoreError::io(operation, SanitizedS3Failure));
    }
    match producer.await {
        Ok(Err(error)) => Err(error),
        Ok(Ok(_)) | Err(_) => Err(BlobStoreError::io(operation, SanitizedS3Failure)),
    }
}

async fn produce_source(
    mut source: BlobReader,
    expected: ExpectedBlob,
    sender: mpsc::Sender<Bytes>,
) -> Result<BlobDigest, BlobStoreError> {
    let mut remaining = expected.byte_length();
    let mut observed = 0_u64;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; STREAM_CHUNK_BYTES];
    while remaining > 0 {
        let limit = usize::try_from(remaining.min(STREAM_CHUNK_BYTES as u64))
            .map_err(|_| BlobStoreError::unavailable("bound S3 source read"))?;
        let count = source
            .read(&mut buffer[..limit])
            .await
            .map_err(|_| BlobStoreError::io("read S3 publication source", SanitizedS3Failure))?;
        if count == 0 {
            return Err(BlobStoreError::verification(
                "verify S3 publication source",
                BlobVerificationFailure::new(expected, None, observed),
            ));
        }
        observed += count as u64;
        remaining -= count as u64;
        hasher.update(&buffer[..count]);
        sender
            .send(Bytes::copy_from_slice(&buffer[..count]))
            .await
            .map_err(|_| BlobStoreError::io("stream S3 publication source", SanitizedS3Failure))?;
    }
    let mut extra = [0_u8; 1];
    let extra_count = source
        .read(&mut extra)
        .await
        .map_err(|_| BlobStoreError::io("read S3 publication source", SanitizedS3Failure))?;
    if extra_count != 0 {
        return Err(BlobStoreError::verification(
            "verify S3 publication source",
            BlobVerificationFailure::new(expected, None, observed.saturating_add(1)),
        ));
    }
    Ok(BlobDigest::from_bytes(hasher.finalize().into()))
}

struct UploadStreamState {
    receiver: mpsc::Receiver<Bytes>,
    pending: Option<Bytes>,
}

struct NamespaceMarker {
    body: Arc<str>,
    verification: NamespaceVerification,
}

/// Lazy namespace-marker verification shared across concurrent callers.
///
/// Only proved verification is final, so a rotated credential can bind the
/// namespace without a restart. Callers already waiting while one probe runs
/// adopt its outcome instead of each running another, which keeps an outage
/// from serializing one 60-second probe per concurrent traversal.
#[derive(Default)]
struct NamespaceVerification {
    proved: AtomicBool,
    completed_attempts: AtomicU64,
    gate: AsyncMutex<()>,
}

/// What one caller holding the gate must do about verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NamespaceProbe {
    /// Verification already succeeded and no probe is needed.
    Proved,
    /// An attempt completed while this caller waited; adopt its failure.
    Adopt,
    /// No attempt has completed since this caller arrived; probe now.
    Run,
}

impl NamespaceVerification {
    /// Records this caller's arrival against the completed-attempt counter.
    fn arrive(&self) -> u64 {
        self.completed_attempts.load(Ordering::Acquire)
    }

    /// Decides one gate holder's action given the count it arrived with.
    fn probe(&self, arrived_at: u64) -> NamespaceProbe {
        if self.proved.load(Ordering::Acquire) {
            NamespaceProbe::Proved
        } else if self.completed_attempts.load(Ordering::Acquire) == arrived_at {
            NamespaceProbe::Run
        } else {
            NamespaceProbe::Adopt
        }
    }

    /// Records one completed attempt and whether it proved the namespace.
    fn record(&self, proved: bool) {
        if proved {
            self.proved.store(true, Ordering::Release);
        }
        self.completed_attempts.fetch_add(1, Ordering::Release);
    }
}

#[derive(Debug, FromXml)]
#[xml(rename = "LifecycleConfiguration", ns(S3_XML_NAMESPACE))]
struct LifecycleConfiguration {
    #[xml(rename = "Rule")]
    rules: Vec<LifecycleRule>,
}

#[derive(Debug, FromXml)]
#[xml(rename = "Rule", ns(S3_XML_NAMESPACE))]
struct LifecycleRule {
    #[xml(rename = "Status")]
    status: String,
    #[xml(rename = "Prefix")]
    prefix: Option<String>,
    #[xml(rename = "Filter")]
    filter: Option<LifecycleFilter>,
    #[xml(rename = "AbortIncompleteMultipartUpload")]
    abort: Option<AbortIncompleteMultipartUpload>,
}

impl LifecycleRule {
    fn covers_blobs(&self) -> bool {
        self.status == "Enabled"
            && self.covers_every_blob_key()
            && self.abort.as_ref().is_some_and(|abort| abort.days == 1)
    }

    /// Reports whether this rule selects every deterministic blob object key.
    ///
    /// A rule that carries both the legacy `Prefix` and a `Filter` names two
    /// selections at once, so nothing it states is a proof: the response is not
    /// a valid lifecycle configuration, and honoring either field alone can
    /// admit a rule whose real selection is narrower than the blob key prefix.
    /// A rule that narrows by tag, size, or a compound `And` never proves blob
    /// coverage; otherwise the selected prefix must be an ancestor of the blob
    /// key prefix, and an absent filter and prefix is whole-bucket coverage.
    fn covers_every_blob_key(&self) -> bool {
        match (&self.filter, self.prefix.as_deref()) {
            (Some(_), Some(_)) => false,
            (Some(filter), None) => {
                filter.tag.is_none()
                    && filter.and.is_none()
                    && filter.object_size_greater_than.is_none()
                    && filter.object_size_less_than.is_none()
                    && covers_blob_keys(filter.prefix.as_deref())
            }
            (None, prefix) => covers_blob_keys(prefix),
        }
    }
}

/// Reports whether a lifecycle prefix selects every `sha256/` object key.
fn covers_blob_keys(prefix: Option<&str>) -> bool {
    prefix.is_none_or(|prefix| BLOB_KEY_PREFIX.starts_with(prefix))
}

#[derive(Debug, FromXml)]
#[xml(rename = "Filter", ns(S3_XML_NAMESPACE))]
struct LifecycleFilter {
    #[xml(rename = "Prefix")]
    prefix: Option<String>,
    #[xml(rename = "Tag")]
    tag: Option<LifecycleTag>,
    #[xml(rename = "And")]
    and: Option<LifecycleAnd>,
    #[xml(rename = "ObjectSizeGreaterThan")]
    object_size_greater_than: Option<u64>,
    #[xml(rename = "ObjectSizeLessThan")]
    object_size_less_than: Option<u64>,
}

#[derive(Debug, FromXml)]
#[xml(rename = "Tag", ns(S3_XML_NAMESPACE))]
struct LifecycleTag {
    #[xml(rename = "Key")]
    _key: Option<String>,
    #[xml(rename = "Value")]
    _value: Option<String>,
}

#[derive(Debug, FromXml)]
#[xml(rename = "And", ns(S3_XML_NAMESPACE))]
struct LifecycleAnd {
    #[xml(rename = "Prefix")]
    _prefix: Option<String>,
    #[xml(rename = "Tag")]
    _tags: Vec<LifecycleTag>,
}

#[derive(Debug, FromXml)]
#[xml(rename = "AbortIncompleteMultipartUpload", ns(S3_XML_NAMESPACE))]
struct AbortIncompleteMultipartUpload {
    #[xml(rename = "DaysAfterInitiation")]
    days: u16,
}

#[derive(Debug, FromXml)]
#[xml(rename = "Error")]
struct S3ErrorDocument {
    #[xml(rename = "Code")]
    code: Option<String>,
}

#[derive(Debug, FromXml)]
#[xml(rename = "CompleteMultipartUploadResult", ns(S3_XML_NAMESPACE))]
struct CompleteMultipartUploadResult {
    #[xml(rename = "Location")]
    _location: Option<String>,
    #[xml(rename = "Bucket")]
    _bucket: Option<String>,
    #[xml(rename = "Key")]
    _key: Option<String>,
    #[xml(rename = "ETag")]
    _etag: Option<String>,
}

/// One in-memory request body emitted in bounded frames that report progress.
///
/// Framing the body keeps the shared no-progress bound applicable to a request
/// whose bytes are already resident, so a peer that stops consuming the write
/// cannot hold the operation until the whole-operation deadline.
struct ChunkedProgressBody {
    bytes: Bytes,
    offset: usize,
    progress: watch::Sender<u64>,
}

impl ChunkedProgressBody {
    const fn new(bytes: Bytes, progress: watch::Sender<u64>) -> Self {
        Self {
            bytes,
            offset: 0,
            progress,
        }
    }

    fn remaining(&self) -> u64 {
        self.bytes.len().saturating_sub(self.offset) as u64
    }
}

impl Body for ChunkedProgressBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.offset >= self.bytes.len() {
            return Poll::Ready(None);
        }
        let end = self
            .offset
            .saturating_add(STREAM_CHUNK_BYTES)
            .min(self.bytes.len());
        let chunk = self.bytes.slice(self.offset..end);
        self.offset = end;
        self.progress.send_replace(self.offset as u64);
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }

    fn is_end_stream(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining())
    }
}

struct ExactUploadBody {
    state: Arc<StdMutex<UploadStreamState>>,
    remaining: u64,
    emitted: u64,
    progress: watch::Sender<u64>,
}

impl ExactUploadBody {
    fn new(
        state: Arc<StdMutex<UploadStreamState>>,
        remaining: u64,
        progress: watch::Sender<u64>,
    ) -> Self {
        Self {
            state,
            remaining,
            emitted: 0,
            progress,
        }
    }
}

impl Body for ExactUploadBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let state = Arc::clone(&self.state);
        let mut state = state
            .lock()
            .map_err(|_| io::Error::other("S3 upload stream lock failed"))?;
        let chunk = if let Some(chunk) = state.pending.take() {
            chunk
        } else {
            match Pin::new(&mut state.receiver).poll_recv(context) {
                Poll::Ready(Some(chunk)) => chunk,
                Poll::Ready(None) => {
                    return Poll::Ready(Some(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "S3 upload source ended early",
                    ))));
                }
                Poll::Pending => return Poll::Pending,
            }
        };
        let take = usize::try_from(self.remaining.min(chunk.len() as u64))
            .map_err(|_| io::Error::other("S3 upload chunk bound failed"))?;
        let emitted = chunk.slice(..take);
        if take < chunk.len() {
            state.pending = Some(chunk.slice(take..));
        }
        drop(state);
        self.remaining -= take as u64;
        self.emitted += take as u64;
        self.progress.send_replace(self.emitted);
        Poll::Ready(Some(Ok(Frame::data(emitted))))
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

#[derive(Deserialize, Zeroize)]
#[serde(deny_unknown_fields)]
#[zeroize(drop)]
struct CredentialDocument {
    version: u8,
    access_key_id: String,
    secret_access_key: String,
}

impl CredentialDocument {
    fn validate(&self) -> Result<(), CredentialFileError> {
        if self.version != 1
            || self.access_key_id.is_empty()
            || self.access_key_id.len() > 256
            || self.secret_access_key.is_empty()
            || self.secret_access_key.len() > 4_096
        {
            Err(CredentialFileError)
        } else {
            Ok(())
        }
    }

    fn into_credentials(mut self) -> Credentials {
        Credentials::new(
            std::mem::take(&mut self.access_key_id),
            std::mem::take(&mut self.secret_access_key),
        )
    }
}

fn read_credentials(path: &Path) -> Result<CredentialDocument, CredentialFileError> {
    let descriptor = openat(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|_| CredentialFileError)?;
    let mut file = File::from(descriptor);
    let metadata = file.metadata().map_err(|_| CredentialFileError)?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.len() > MAX_CREDENTIAL_FILE_BYTES
    {
        return Err(CredentialFileError);
    }
    let mut bytes =
        Vec::with_capacity(usize::try_from(metadata.len()).map_err(|_| CredentialFileError)?);
    file.by_ref()
        .take(MAX_CREDENTIAL_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| CredentialFileError)?;
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        bytes.zeroize();
        return Err(CredentialFileError);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| CredentialFileError)?;
    let document = toml::from_str::<CredentialDocument>(text).map_err(|_| CredentialFileError);
    bytes.zeroize();
    let document = document?;
    document.validate()?;
    Ok(document)
}

/// S3 adapter construction rejected a non-secret deployment fact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum S3BlobStoreConstructionError {
    /// Endpoint, bucket, or region could not form a path-style bucket URL.
    Endpoint,
    /// The credentials reference was not absolute.
    CredentialPath,
    /// The closed HTTP client could not be constructed.
    Transport,
    /// The canonical namespace marker body exceeded its fixed bound.
    NamespaceMarker,
}

impl fmt::Display for S3BlobStoreConstructionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("S3 blob store construction failed")
    }
}

impl Error for S3BlobStoreConstructionError {}

/// Whether startup may create a missing namespace marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum S3NamespaceBindingState {
    /// The database has no durable binding and conditional marker creation is allowed.
    New,
    /// The database already binds this name and a marker must already exist.
    Recorded,
}

#[derive(Clone, Copy, Debug)]
struct SanitizedS3Failure;

impl fmt::Display for SanitizedS3Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("S3 transport failed")
    }
}

impl Error for SanitizedS3Failure {}

#[derive(Clone, Copy, Debug)]
struct CredentialFileError;

impl fmt::Display for CredentialFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("S3 credential file is invalid")
    }
}

impl Error for CredentialFileError {}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;
    use url::Url;

    use signalbox_blob_store::{BlobStoreFailureKind, ExpectedBlob};
    use signalbox_domain::BlobDigest;

    use super::{
        CompletionFailure, CredentialFileError, HeaderMap, HeaderValue, LifecycleConfiguration,
        LifecycleRule, MAX_ETAG_BYTES, MAX_MULTIPART_PARTS, MAX_S3_OBJECT_BYTES,
        MIN_MULTIPART_PART_BYTES, NamespaceProbe, NamespaceVerification, S3BlobStore, StatusCode,
        completion_status_failure, multipart_part_bytes, names_absent_object, object_generation,
        read_credentials, validate_completion_response, verify_stream,
    };

    const ACCESS_KEY: &str = "fixture-access-key";
    const SECRET_KEY: &str = "fixture-secret-key";
    const ENDPOINT: &str = "https://objects.example.test";
    const BUCKET: &str = "fixture-bucket";
    const VERIFIED_CONTENT: &[u8] = b"S3 verified-open conformance fixture";
    const SAME_LENGTH_CONTENT: &[u8] = b"S3 verified-open conformance fixturf";

    fn verified_expectation() -> ExpectedBlob {
        ExpectedBlob::try_new(
            BlobDigest::digest(VERIFIED_CONTENT),
            VERIFIED_CONTENT.len() as u64,
        )
        .expect("the verified-open fixture is nonempty")
    }

    fn generation_headers(generation: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::ETAG,
            HeaderValue::from_str(generation).expect("the fixture entity tag is a header value"),
        );
        headers
    }

    fn credential_body() -> String {
        format!(
            "version = 1\naccess_key_id = \"{ACCESS_KEY}\"\nsecret_access_key = \"{SECRET_KEY}\"\n"
        )
    }

    fn credential_fixture(body: &str) -> Result<(TempDir, PathBuf), Box<dyn Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("credentials.toml");
        fs::write(&path, body)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok((directory, path))
    }

    fn first_rule(xml: &str) -> Result<LifecycleRule, Box<dyn Error>> {
        let lifecycle: LifecycleConfiguration = instant_xml::from_str(xml)?;
        lifecycle
            .rules
            .into_iter()
            .next()
            .ok_or_else(|| Box::<dyn Error>::from("fixture lifecycle has no rule"))
    }

    #[test]
    fn credential_file_accepts_only_the_closed_version_one_shape() -> Result<(), Box<dyn Error>> {
        let body = credential_body();
        let (_directory, path) = credential_fixture(&body)?;
        let document = read_credentials(&path)?;

        assert_eq!(document.version, 1);
        assert_eq!(document.access_key_id, ACCESS_KEY);
        assert_eq!(document.secret_access_key, SECRET_KEY);
        Ok(())
    }

    #[test]
    fn credential_file_rejects_an_unknown_member() -> Result<(), Box<dyn Error>> {
        let body = format!("{}unknown = true\n", credential_body());
        let (_directory, path) = credential_fixture(&body)?;

        assert!(matches!(read_credentials(&path), Err(CredentialFileError)));
        Ok(())
    }

    #[test]
    fn credential_file_rejects_group_readable_permissions() -> Result<(), Box<dyn Error>> {
        let (_directory, path) = credential_fixture(&credential_body())?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640))?;

        assert!(matches!(read_credentials(&path), Err(CredentialFileError)));
        Ok(())
    }

    #[test]
    fn lifecycle_admits_only_an_enabled_one_day_blob_prefix_rule() -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Filter><Prefix>sha256/</Prefix></Filter><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_admits_an_ancestor_prefix_of_the_blob_key_prefix() -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Prefix>sha256</Prefix><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_admits_a_whole_bucket_rule_without_a_filter_or_prefix()
    -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_admits_an_empty_whole_bucket_filter() -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Filter></Filter><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_rejects_an_abort_later_than_one_day() -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Prefix>sha256/</Prefix><AbortIncompleteMultipartUpload><DaysAfterInitiation>2</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(!LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_rejects_a_prefix_narrower_than_the_blob_key_prefix() -> Result<(), Box<dyn Error>>
    {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Prefix>sha256/ab/</Prefix><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(!LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_rejects_a_prefix_outside_the_blob_key_prefix() -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Prefix>staging/</Prefix><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(!LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_rejects_a_tag_filtered_rule() -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Filter><Tag><Key>kind</Key><Value>blob</Value></Tag></Filter><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(!LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_rejects_a_size_filtered_rule() -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Filter><ObjectSizeGreaterThan>1024</ObjectSizeGreaterThan></Filter><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(!LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_rejects_a_rule_carrying_both_a_legacy_prefix_and_a_filter()
    -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Prefix>staging/</Prefix><Filter><Prefix>sha256/</Prefix></Filter><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(!LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn lifecycle_rejects_a_whole_bucket_filter_beside_a_narrow_legacy_prefix()
    -> Result<(), Box<dyn Error>> {
        let rule = first_rule(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Prefix>staging/</Prefix><Filter></Filter><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;

        assert!(!LifecycleRule::covers_blobs(&rule));
        Ok(())
    }

    #[test]
    fn an_absent_object_code_proves_absence() {
        let key = r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>NoSuchKey</Code><Message>The specified key does not exist</Message><RequestId>fixture</RequestId></Error>"#;

        assert!(names_absent_object(key));
    }

    #[test]
    fn an_absent_bucket_code_does_not_prove_object_absence() {
        let bucket = r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>NoSuchBucket</Code><Message>The specified bucket does not exist</Message><BucketName>fixture-bucket</BucketName><RequestId>fixture</RequestId></Error>"#;

        assert!(!names_absent_object(bucket));
    }

    #[test]
    fn an_unrecognized_code_does_not_prove_object_absence() {
        let denied = r#"<?xml version="1.0" encoding="UTF-8"?><Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>"#;

        assert!(!names_absent_object(denied));
    }

    #[test]
    fn an_unparsable_body_does_not_prove_object_absence() {
        assert!(!names_absent_object(""));
    }

    #[test]
    fn a_first_caller_runs_its_own_namespace_probe() {
        let verification = NamespaceVerification::default();
        let arrived_at = verification.arrive();

        assert_eq!(verification.probe(arrived_at), NamespaceProbe::Run);
    }

    #[test]
    fn a_caller_waiting_through_a_failed_probe_adopts_it() {
        let verification = NamespaceVerification::default();
        let arrived_at = verification.arrive();
        verification.record(false);

        assert_eq!(verification.probe(arrived_at), NamespaceProbe::Adopt);
    }

    #[test]
    fn a_caller_arriving_after_a_failed_probe_retries() {
        let verification = NamespaceVerification::default();
        verification.record(false);
        let arrived_at = verification.arrive();

        assert_eq!(verification.probe(arrived_at), NamespaceProbe::Run);
    }

    #[test]
    fn a_proved_namespace_needs_no_further_probe() {
        let verification = NamespaceVerification::default();
        let arrived_at = verification.arrive();
        verification.record(true);

        assert_eq!(verification.probe(arrived_at), NamespaceProbe::Proved);
    }

    #[test]
    fn a_server_completion_status_stays_possibly_accepted() {
        let failure = completion_status_failure(StatusCode::BAD_GATEWAY);

        assert!(matches!(failure, CompletionFailure::PossiblyAccepted));
    }

    #[test]
    fn a_client_completion_status_proves_definite_failure() {
        let failure = completion_status_failure(StatusCode::NOT_FOUND);

        assert!(matches!(failure, CompletionFailure::Definite(_)));
    }

    #[test]
    fn completion_response_rejects_an_http_success_error_document() {
        let success = br#"<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Location>fixture</Location><Bucket>bucket</Bucket><Key>key</Key><ETag>etag</ETag></CompleteMultipartUploadResult>"#;
        let embedded_error =
            br#"<Error><Code>InternalError</Code><Message>retry</Message></Error>"#;

        assert!(validate_completion_response(success).is_ok());
        assert!(validate_completion_response(embedded_error).is_err());
    }

    #[test]
    fn multipart_part_size_bounds_part_count_without_buffering_a_part() {
        assert_eq!(multipart_part_bytes(1), Some(MIN_MULTIPART_PART_BYTES));
        assert_eq!(
            multipart_part_bytes(MIN_MULTIPART_PART_BYTES * MAX_MULTIPART_PARTS),
            Some(MIN_MULTIPART_PART_BYTES)
        );
        assert!(multipart_part_bytes(MAX_S3_OBJECT_BYTES).is_some());
        assert_eq!(multipart_part_bytes(MAX_S3_OBJECT_BYTES + 1), None);
    }

    #[tokio::test]
    async fn a_verified_stream_admits_the_exact_expected_bytes() {
        let expected = verified_expectation();

        verify_stream(
            std::io::Cursor::new(VERIFIED_CONTENT.to_vec()),
            expected,
            "verify opened S3 object",
        )
        .await
        .expect("the exact recorded bytes verify");
    }

    #[tokio::test]
    async fn a_verified_stream_fails_as_soon_as_it_runs_long() {
        let expected = verified_expectation();
        let mut overrun = VERIFIED_CONTENT.to_vec();
        overrun.extend_from_slice(b" and one trailing overrun");

        let error = verify_stream(
            std::io::Cursor::new(overrun),
            expected,
            "verify opened S3 object",
        )
        .await
        .expect_err("a stream longer than the declared length must fail verification");

        assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);
        let failure = error
            .verification_failure()
            .expect("an overrun retains its observed facts");
        assert_eq!(failure.observed_digest(), None);
        assert!(failure.observed_length() > expected.byte_length());
    }

    #[tokio::test]
    async fn a_verified_stream_rejects_a_same_length_digest_mismatch() {
        let expected = verified_expectation();
        assert_eq!(SAME_LENGTH_CONTENT.len(), VERIFIED_CONTENT.len());

        let error = verify_stream(
            std::io::Cursor::new(SAME_LENGTH_CONTENT.to_vec()),
            expected,
            "verify opened S3 object",
        )
        .await
        .expect_err("bytes that do not hash to the recorded digest must fail verification");

        assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);
        let failure = error
            .verification_failure()
            .expect("a completed mismatch retains its observed facts");
        assert_eq!(
            failure.observed_digest(),
            Some(BlobDigest::digest(SAME_LENGTH_CONTENT))
        );
        assert_eq!(failure.observed_length(), expected.byte_length());
    }

    #[tokio::test]
    async fn a_verified_stream_rejects_a_truncated_generation() {
        let expected = verified_expectation();
        let truncated = VERIFIED_CONTENT[..VERIFIED_CONTENT.len() - 1].to_vec();

        let error = verify_stream(
            std::io::Cursor::new(truncated.clone()),
            expected,
            "verify opened S3 object",
        )
        .await
        .expect_err("a stream shorter than the declared length must fail verification");

        assert_eq!(error.kind(), BlobStoreFailureKind::VerificationFailed);
        let failure = error
            .verification_failure()
            .expect("a completed shortfall retains its observed facts");
        assert_eq!(
            failure.observed_digest(),
            Some(BlobDigest::digest(&truncated))
        );
        assert_eq!(failure.observed_length(), expected.byte_length() - 1);
    }

    #[test]
    fn an_entity_tag_names_the_generation_a_pinned_read_replays() {
        let generation = object_generation(&generation_headers("\"fixture-generation\""))
            .expect("a served entity tag names the generation");

        assert_eq!(generation.as_bytes(), b"\"fixture-generation\"");
    }

    #[test]
    fn a_response_without_an_entity_tag_leaves_the_generation_unnamed() {
        assert_eq!(object_generation(&HeaderMap::new()), None);
    }

    #[test]
    fn an_empty_entity_tag_leaves_the_generation_unnamed() {
        assert_eq!(object_generation(&generation_headers("")), None);
    }

    #[test]
    fn an_oversized_entity_tag_leaves_the_generation_unnamed() {
        let oversized = format!("\"{}\"", "e".repeat(MAX_ETAG_BYTES));

        assert_eq!(object_generation(&generation_headers(&oversized)), None);
    }

    #[test]
    fn a_wildcard_entity_tag_leaves_the_generation_unnamed() {
        assert_eq!(object_generation(&generation_headers("*")), None);
    }

    #[test]
    fn an_entity_tag_list_leaves_the_generation_unnamed() {
        assert_eq!(
            object_generation(&generation_headers("\"first\", \"second\"")),
            None
        );
    }

    #[test]
    fn a_weak_entity_tag_leaves_the_generation_unnamed() {
        assert_eq!(
            object_generation(&generation_headers("W/\"fixture-generation\"")),
            None
        );
    }

    #[test]
    fn an_unquoted_entity_tag_leaves_the_generation_unnamed() {
        assert_eq!(
            object_generation(&generation_headers("fixture-generation")),
            None
        );
    }

    #[test]
    fn adapter_debug_omits_endpoint_bucket_and_credential_path() -> Result<(), Box<dyn Error>> {
        let (_directory, path) = credential_fixture(&credential_body())?;
        let store = S3BlobStore::try_new(
            Url::parse(ENDPOINT)?,
            "fixture-region",
            BUCKET,
            path.clone(),
        )?;
        let debug = format!("{store:?}");

        assert!(!debug.contains(ENDPOINT));
        assert!(!debug.contains(BUCKET));
        assert!(!debug.contains(Path::new(&path).to_string_lossy().as_ref()));
        assert!(!debug.contains(ACCESS_KEY));
        assert!(!debug.contains(SECRET_KEY));
        Ok(())
    }
}
