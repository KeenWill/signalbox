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
    sync::{Arc, Mutex as StdMutex},
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use http_body::{Body, Frame, SizeHint};
use instant_xml::FromXml;
use jiff::Timestamp;
use reqwest::{Client, Response, StatusCode, redirect::Policy};
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
    sync::{Mutex as AsyncMutex, mpsc},
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
const MAX_CREATE_RESPONSE_BYTES: usize = 65_536;
const MAX_COMPLETE_RESPONSE_BYTES: usize = 65_536;
const MAX_UPLOAD_ID_BYTES: usize = 1_024;
const MAX_ETAG_BYTES: usize = 256;
const MAX_COMPLETE_BODY_BYTES: usize = 3 * 1024 * 1024;
const PUBLICATION_LOCK_STRIPES: usize = 64;
const NAMESPACE_MARKER_KEY: &str = ".signalbox-blob-namespace-v1";
const MAX_NAMESPACE_MARKER_BYTES: usize = 128;
const MAX_LIFECYCLE_RESPONSE_BYTES: usize = 65_536;
const S3_XML_NAMESPACE: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

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
            verified: tokio::sync::OnceCell::new(),
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
            let _ = marker.verified.set(Ok(()));
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

    async fn ensure_namespace_ready(&self) -> Result<(), BlobStoreError> {
        let Some(marker) = &self.namespace_marker else {
            return Ok(());
        };
        let result = marker
            .verified
            .get_or_init(|| async { self.verify_namespace_marker(marker).await.map_err(|_| ()) })
            .await;
        match result {
            Ok(()) => Ok(()),
            Err(()) => Err(BlobStoreError::unavailable("verify S3 namespace marker")),
        }
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
        self.ensure_namespace_ready().await?;
        let key = BlobObjectKey::for_digest(expected.digest());
        let stripe = usize::from(expected.digest().as_bytes()[0]) % PUBLICATION_LOCK_STRIPES;
        let _publication = self.publication_locks[stripe].lock().await;
        let credentials = self.credentials().await?;
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
        self.verify_object(&credentials, &key, expected).await?;
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
            let body = ExactUploadBody::new(Arc::clone(&stream_state), length);
            let action =
                self.bucket
                    .upload_part(Some(credentials), key.as_str(), part_number, upload_id);
            let response = match self
                .client
                .put(action.sign(SIGNED_URL_LIFETIME))
                .header(reqwest::header::CONTENT_LENGTH, length)
                .body(reqwest::Body::wrap(body))
                .send()
                .await
            {
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
        let body = complete.body();
        if body.len() > MAX_COMPLETE_BODY_BYTES {
            return Err(BlobStoreError::unavailable("bound S3 completion body"));
        }
        let completion = async {
            let response = self
                .client
                .post(signed)
                .header(reqwest::header::CONTENT_TYPE, "application/xml")
                .body(body)
                .send()
                .await
                .map_err(|_| {
                    BlobStoreError::io("complete S3 multipart upload", SanitizedS3Failure)
                })?;
            let response = require_success(response, "complete S3 multipart upload").await?;
            let _ = bounded_response(
                response,
                MAX_COMPLETE_RESPONSE_BYTES,
                "read S3 completion response",
            )
            .await?;
            Ok(())
        }
        .await;
        match completion {
            Ok(()) => Ok(()),
            Err(completion_error) => match self.verify_object(credentials, key, expected).await {
                Ok(()) => Ok(()),
                Err(_) => Err(completion_error),
            },
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
            return Err(BlobStoreError::not_found("get S3 object"));
        }
        require_success(response, "get S3 object").await
    }

    async fn verify_object(
        &self,
        credentials: &Credentials,
        key: &BlobObjectKey,
        expected: ExpectedBlob,
    ) -> Result<(), BlobStoreError> {
        let response = self.open_response(credentials, key).await?;
        let mut reader = response_reader(response);
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
            hasher.update(&buffer[..count]);
        }
        let observed_digest = BlobDigest::from_bytes(hasher.finalize().into());
        if observed_length != expected.byte_length() || observed_digest != expected.digest() {
            return Err(BlobStoreError::verification(
                "verify S3 object",
                BlobVerificationFailure::new(expected, Some(observed_digest), observed_length),
            ));
        }
        Ok(())
    }

    async fn open_inner(&self, key: &BlobObjectKey) -> Result<OpenedBlob, BlobStoreError> {
        self.ensure_namespace_ready().await?;
        let credentials = self.credentials().await?;
        let response = self.open_response(&credentials, key).await?;
        let length = response
            .content_length()
            .ok_or_else(|| BlobStoreError::unavailable("read S3 object length"))?;
        Ok(OpenedBlob::new(length, Box::new(response_reader(response))))
    }

    async fn open_range_inner(
        &self,
        key: &BlobObjectKey,
        expected: ExpectedBlob,
        offset: u64,
        length: u64,
    ) -> Result<OpenedBlob, BlobStoreError> {
        self.ensure_namespace_ready().await?;
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

async fn require_success(
    response: Response,
    operation: &'static str,
) -> Result<Response, BlobStoreError> {
    if response.status().is_success() {
        Ok(response)
    } else if response.status() == StatusCode::NOT_FOUND {
        Err(BlobStoreError::not_found(operation))
    } else {
        Err(BlobStoreError::unavailable(operation))
    }
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
    verified: tokio::sync::OnceCell<Result<(), ()>>,
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
        let prefix_covers = match (&self.filter, self.prefix.as_deref()) {
            (Some(filter), _) => {
                filter.tag.is_none()
                    && filter.and.is_none()
                    && matches!(filter.prefix.as_deref(), None | Some("") | Some("sha256/"))
            }
            (None, Some(prefix)) => matches!(prefix, "" | "sha256/"),
            (None, None) => false,
        };
        self.status == "Enabled"
            && prefix_covers
            && self.abort.as_ref().is_some_and(|abort| abort.days == 1)
    }
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

struct ExactUploadBody {
    state: Arc<StdMutex<UploadStreamState>>,
    remaining: u64,
}

impl ExactUploadBody {
    fn new(state: Arc<StdMutex<UploadStreamState>>, remaining: u64) -> Self {
        Self { state, remaining }
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
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
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

    use super::{
        CredentialFileError, LifecycleConfiguration, LifecycleRule, MAX_MULTIPART_PART_BYTES,
        MAX_MULTIPART_PARTS, MIN_MULTIPART_PART_BYTES, S3BlobStore, multipart_part_bytes,
        read_credentials,
    };

    const ACCESS_KEY: &str = "fixture-access-key";
    const SECRET_KEY: &str = "fixture-secret-key";
    const ENDPOINT: &str = "https://objects.example.test";
    const BUCKET: &str = "fixture-bucket";

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

    fn parsed_lifecycle(xml: &str) -> Result<LifecycleConfiguration, Box<dyn Error>> {
        Ok(instant_xml::from_str(xml)?)
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
    fn credential_file_rejects_unknown_members_and_broad_permissions() -> Result<(), Box<dyn Error>>
    {
        let unknown_body = format!("{}unknown = true\n", credential_body());
        let (_unknown_directory, unknown_path) = credential_fixture(&unknown_body)?;
        let mode_body = credential_body();
        let (_mode_directory, mode_path) = credential_fixture(&mode_body)?;
        fs::set_permissions(&mode_path, fs::Permissions::from_mode(0o640))?;

        assert!(matches!(
            read_credentials(&unknown_path),
            Err(CredentialFileError)
        ));
        assert!(matches!(
            read_credentials(&mode_path),
            Err(CredentialFileError)
        ));
        Ok(())
    }

    #[test]
    fn lifecycle_admits_only_an_enabled_one_day_blob_prefix_rule() -> Result<(), Box<dyn Error>> {
        let lifecycle = parsed_lifecycle(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Filter><Prefix>sha256/</Prefix></Filter><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;
        let rule = lifecycle.rules.first().ok_or("fixture rule is absent")?;

        assert!(LifecycleRule::covers_blobs(rule));
        Ok(())
    }

    #[test]
    fn lifecycle_rejects_a_late_abort_and_a_narrow_prefix() -> Result<(), Box<dyn Error>> {
        let late = parsed_lifecycle(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Prefix>sha256/</Prefix><AbortIncompleteMultipartUpload><DaysAfterInitiation>2</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;
        let narrow = parsed_lifecycle(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Prefix>sha256/ab/</Prefix><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;
        let tagged = parsed_lifecycle(
            r#"<LifecycleConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Rule><Status>Enabled</Status><Filter><Tag><Key>kind</Key><Value>blob</Value></Tag></Filter><AbortIncompleteMultipartUpload><DaysAfterInitiation>1</DaysAfterInitiation></AbortIncompleteMultipartUpload></Rule></LifecycleConfiguration>"#,
        )?;
        let late_rule = late.rules.first().ok_or("late fixture rule is absent")?;
        let narrow_rule = narrow
            .rules
            .first()
            .ok_or("narrow fixture rule is absent")?;
        let tagged_rule = tagged
            .rules
            .first()
            .ok_or("tagged fixture rule is absent")?;

        assert!(!LifecycleRule::covers_blobs(late_rule));
        assert!(!LifecycleRule::covers_blobs(narrow_rule));
        assert!(!LifecycleRule::covers_blobs(tagged_rule));
        Ok(())
    }

    #[test]
    fn multipart_part_size_bounds_part_count_without_buffering_a_part() {
        assert_eq!(multipart_part_bytes(1), Some(MIN_MULTIPART_PART_BYTES));
        assert_eq!(
            multipart_part_bytes(MIN_MULTIPART_PART_BYTES * MAX_MULTIPART_PARTS),
            Some(MIN_MULTIPART_PART_BYTES)
        );
        assert_eq!(
            multipart_part_bytes(MAX_MULTIPART_PART_BYTES * MAX_MULTIPART_PARTS),
            Some(MAX_MULTIPART_PART_BYTES)
        );
        assert_eq!(
            multipart_part_bytes(MAX_MULTIPART_PART_BYTES * MAX_MULTIPART_PARTS + 1),
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
