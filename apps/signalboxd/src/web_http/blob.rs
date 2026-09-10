use super::utf8_percent_encode;
use headers::HeaderMapExt as _;
use std::str::FromStr as _;
use tokio::io::AsyncReadExt as _;

use super::{
    ACCEPT_RANGES, ALLOW, Arc, AsciiSet, BLOB_RESPONSE_TIMEOUT_SECONDS, BLOB_STREAM_CHUNK_BYTES,
    BlobDerivation, BlobDerivationProducer, BlobDigest, Body, Bytes, CACHE_CONTROL,
    CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, CONTROLS, Deserialize,
    Duration, ETAG, HeaderMap, HeaderValue, IF_RANGE, IMMUTABLE_CACHE_CONTROL, Instant,
    IntoResponse, Json, MAX_BLOB_RANGE_BYTES, MAX_DISPLAY_FILENAME_BYTES, Method, NonZeroU64,
    OwnedSemaphorePermit, Path, Query, QueryRejection, RANGE, Request, Response, Semaphore, State,
    StatusCode, TypedEtag, TypedIfNoneMatch, TypedIfRange, TypedRange, WebBlobAvailableView,
    WebBlobDerivation, WebBlobDerivationProducer, WebBlobDescriptor, WebBlobRuntime,
    WebBlobRuntimeError, WebBlobViewKind, WebHttpState, WebImageDerivativeKind,
    X_CONTENT_TYPE_OPTIONS, api_not_found, application_error, io, mpsc, open_recorded_blob_range,
    open_recorded_blob_verified, stream, timeout_at, transport_error,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BlobUseQuery {
    media_type: String,
    display_filename: Option<String>,
}

pub(super) async fn blob_descriptor(
    State(state): State<WebHttpState>,
    Path(digest): Path<String>,
    use_metadata: Result<Query<BlobUseQuery>, QueryRejection>,
) -> Response {
    let use_metadata = match use_metadata {
        Ok(Query(use_metadata)) => use_metadata,
        Err(_) => return invalid_blob_use_response(),
    };
    let Some(runtime) = state.blobs else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_storage_unavailable",
            "blob storage is not configured",
        );
    };
    let digest = match BlobDigest::from_str(&digest) {
        Ok(digest) => digest,
        Err(_) => {
            return transport_error(
                StatusCode::BAD_REQUEST,
                "invalid_blob_digest",
                "blob digest is not canonical",
            );
        }
    };
    if !valid_blob_use(&use_metadata) {
        return transport_error(
            StatusCode::BAD_REQUEST,
            "invalid_blob_use",
            "blob media type or display filename is invalid",
        );
    }
    let entry = match runtime.entry(digest).await {
        Ok(entry) => entry,
        Err(error) => return runtime_error_response(error),
    };
    let query = blob_use_query(&use_metadata);
    let download_url = format!("/api/blobs/{digest}/download?{query}");
    let byte_length = entry.expected().byte_length().to_string();
    let mut available_views = vec![WebBlobAvailableView {
        kind: WebBlobViewKind::Download,
        media_type: use_metadata.media_type.clone(),
        byte_length: byte_length.clone(),
        content_url: download_url,
        derivations: Vec::new(),
    }];
    if let Some(representation) = image_representation(&use_metadata.media_type) {
        let Some(representation_media_type) = representation_media_type(representation) else {
            return runtime_error_response(WebBlobRuntimeError::Integrity);
        };
        available_views.push(WebBlobAvailableView {
            kind: WebBlobViewKind::BrowserNative,
            media_type: representation_media_type.to_owned(),
            byte_length: byte_length.clone(),
            content_url: format!("/api/blobs/{digest}/content/{representation}"),
            derivations: Vec::new(),
        });
        if runtime.supports_image_derivatives() {
            append_image_derivative_view(
                &runtime,
                Arc::clone(&state.blob_read_budget),
                digest,
                WebImageDerivativeKind::Thumbnail,
                WebBlobViewKind::Thumbnail,
                &mut available_views,
            )
            .await;
            append_image_derivative_view(
                &runtime,
                Arc::clone(&state.blob_read_budget),
                digest,
                WebImageDerivativeKind::Preview,
                WebBlobViewKind::Preview,
                &mut available_views,
            )
            .await;
        }
    }
    Json(WebBlobDescriptor {
        digest: digest.to_string(),
        byte_length,
        declared_media_type: use_metadata.media_type,
        display_filename: use_metadata.display_filename.into_iter().collect(),
        available_views,
    })
    .into_response()
}

pub(super) async fn blob_descriptor_head() -> Response {
    let mut response = transport_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "descriptor_method_not_allowed",
        "blob descriptors are available through GET",
    );
    insert_header(response.headers_mut(), ALLOW, String::from("GET"));
    response
}

async fn append_image_derivative_view(
    runtime: &WebBlobRuntime,
    read_budget: Arc<Semaphore>,
    input: BlobDigest,
    kind: WebImageDerivativeKind,
    view_kind: WebBlobViewKind,
    views: &mut Vec<WebBlobAvailableView>,
) {
    let Ok(derivation) = runtime.derive_image(input, kind).await else {
        return;
    };
    let Some(output) = derivation.outputs().first().copied() else {
        return;
    };
    let Ok(entry) = runtime.entry(output).await else {
        return;
    };
    let Some(_permit) = try_acquire_web_blob_read_permit(read_budget) else {
        return;
    };
    if open_recorded_blob_verified(runtime.registry(), &entry)
        .await
        .is_err()
    {
        return;
    }
    let Some(provenance) = project_derivation(&derivation) else {
        return;
    };
    views.push(WebBlobAvailableView {
        kind: view_kind,
        media_type: String::from("image/png"),
        byte_length: entry.expected().byte_length().to_string(),
        content_url: format!("/api/blobs/{output}/content/image-png"),
        derivations: vec![provenance],
    });
}

fn project_derivation(derivation: &BlobDerivation) -> Option<WebBlobDerivation> {
    let producer = match derivation.producer() {
        BlobDerivationProducer::Deterministic { implementation } => {
            WebBlobDerivationProducer::Deterministic {
                implementation_digest: implementation.to_string(),
                cache_key: derivation.deterministic_key()?.digest().to_string(),
            }
        }
        BlobDerivationProducer::Executed {
            execution_id,
            implementation,
        } => WebBlobDerivationProducer::Executed {
            execution_id: execution_id.to_string(),
            implementation_digest: implementation.to_string(),
        },
        BlobDerivationProducer::ModelDerived { model_call } => {
            WebBlobDerivationProducer::ModelDerived {
                model_call_id: model_call.into_uuid().to_string(),
            }
        }
    };
    Some(WebBlobDerivation {
        derivation_id: derivation.id().into_uuid().to_string(),
        input_digests: derivation
            .inputs()
            .iter()
            .map(ToString::to_string)
            .collect(),
        transformation_name: derivation.transformation().name().as_str().to_owned(),
        transformation_version: derivation.transformation().version().get(),
        parameters_json: derivation.transformation().parameters_json().to_owned(),
        producer,
        output_digests: derivation
            .outputs()
            .iter()
            .map(ToString::to_string)
            .collect(),
    })
}

fn valid_blob_use(value: &BlobUseQuery) -> bool {
    !value.media_type.is_empty()
        && value.media_type.len() <= 255
        && value.media_type.parse::<mime::Mime>().is_ok()
        && value.display_filename.as_ref().is_none_or(|filename| {
            !filename.is_empty()
                && filename.len() <= MAX_DISPLAY_FILENAME_BYTES
                && !filename.chars().any(char::is_control)
        })
}

fn invalid_blob_use_response() -> Response {
    transport_error(
        StatusCode::BAD_REQUEST,
        "invalid_blob_use",
        "blob media type or display filename is invalid",
    )
}

fn blob_use_query(value: &BlobUseQuery) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("media_type", &value.media_type);
    if let Some(filename) = &value.display_filename {
        serializer.append_pair("display_filename", filename);
    }
    serializer.finish()
}

fn image_representation(media_type: &str) -> Option<&'static str> {
    let media_type = media_type.parse::<mime::Mime>().ok()?;
    match (media_type.type_().as_str(), media_type.subtype().as_str()) {
        ("image", "png") => Some("image-png"),
        ("image", "jpeg") => Some("image-jpeg"),
        ("image", "gif") => Some("image-gif"),
        ("image", "webp") => Some("image-webp"),
        _ => None,
    }
}

fn representation_media_type(representation: &str) -> Option<&'static str> {
    match representation {
        "image-png" => Some("image/png"),
        "image-jpeg" => Some("image/jpeg"),
        "image-gif" => Some("image/gif"),
        "image-webp" => Some("image/webp"),
        _ => None,
    }
}

pub(super) async fn blob_content(
    State(state): State<WebHttpState>,
    Path((digest, representation)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Some(media_type) = representation_media_type(&representation) else {
        return api_not_found().await;
    };
    serve_blob(state, digest, media_type, None, request).await
}

pub(super) async fn blob_download(
    State(state): State<WebHttpState>,
    Path(digest): Path<String>,
    use_metadata: Result<Query<BlobUseQuery>, QueryRejection>,
    request: Request,
) -> Response {
    let use_metadata = match use_metadata {
        Ok(Query(use_metadata)) => use_metadata,
        Err(_) => return invalid_blob_use_response(),
    };
    if !valid_blob_use(&use_metadata) {
        return transport_error(
            StatusCode::BAD_REQUEST,
            "invalid_blob_use",
            "blob media type or display filename is invalid",
        );
    }
    let filename = use_metadata
        .display_filename
        .as_deref()
        .unwrap_or("download");
    serve_blob(
        state,
        digest,
        &use_metadata.media_type,
        Some(content_disposition(filename)),
        request,
    )
    .await
}

async fn serve_blob(
    state: WebHttpState,
    digest: String,
    media_type: &str,
    disposition: Option<String>,
    request: Request,
) -> Response {
    let Some(runtime) = state.blobs else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_storage_unavailable",
            "blob storage is not configured",
        );
    };
    let digest = match BlobDigest::from_str(&digest) {
        Ok(digest) => digest,
        Err(_) => {
            return transport_error(
                StatusCode::BAD_REQUEST,
                "invalid_blob_digest",
                "blob digest is not canonical",
            );
        }
    };
    let entry = match runtime.entry(digest).await {
        Ok(entry) => entry,
        Err(error) => return runtime_error_response(error),
    };
    let etag = format!("\"{digest}\"");
    if if_none_match(request.headers(), &etag) {
        return not_modified_response(&etag);
    }
    let total = entry.expected().byte_length();
    let requested_range = match applicable_range_header(request.headers(), &etag) {
        Ok(range) => range,
        Err(()) => return range_not_satisfiable(total, &etag),
    };
    let (offset, length, partial) = match requested_range {
        Some(range) => match parse_byte_range(range, total) {
            Ok(range) => range,
            Err(()) => return range_not_satisfiable(total, &etag),
        },
        None => (0, total, false),
    };
    let content_type = match HeaderValue::from_str(media_type) {
        Ok(value) => value,
        Err(_) => {
            return transport_error(
                StatusCode::BAD_REQUEST,
                "invalid_blob_media_type",
                "blob media type is not an HTTP field value",
            );
        }
    };
    let method = request.method().clone();
    // A head response owes the same status as the equivalent `GET`, so read
    // admission covers both methods. The head response then releases its permit
    // at once, because it never opens a replica or streams blob bytes.
    let Some(streamed_length) = NonZeroU64::new(length) else {
        return range_not_satisfiable(total, &etag);
    };
    let Some(permit) = try_acquire_web_blob_read_permit(Arc::clone(&state.blob_read_budget)) else {
        return application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_read_busy",
            "blob read capacity is busy",
        );
    };
    let body = if method == Method::HEAD {
        drop(permit);
        Body::empty()
    } else {
        let deadline = Instant::now() + Duration::from_secs(BLOB_RESPONSE_TIMEOUT_SECONDS);
        let opened = timeout_at(deadline, async {
            if streamed_length.get() <= MAX_BLOB_RANGE_BYTES {
                open_recorded_blob_range(runtime.registry(), &entry, offset, streamed_length).await
            } else {
                let mut reader = open_recorded_blob_verified(runtime.registry(), &entry).await?;
                let skipped =
                    tokio::io::copy(&mut (&mut reader).take(offset), &mut tokio::io::sink())
                        .await
                        .map_err(|_| crate::blob_read_runtime::BlobReadError::Unavailable)?;
                if skipped != offset {
                    return Err(crate::blob_read_runtime::BlobReadError::Integrity);
                }
                Ok(reader)
            }
        })
        .await;
        let reader = match opened {
            Ok(Ok(reader)) => reader,
            Ok(Err(error)) => return blob_read_error_response(error),
            Err(_) => {
                return blob_read_error_response(
                    crate::blob_read_runtime::BlobReadError::Unavailable,
                );
            }
        };
        reader_body_until(reader, streamed_length.get(), permit, deadline)
    };
    let mut response = Response::new(body);
    *response.status_mut() = if partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    response.headers_mut().insert(CONTENT_TYPE, content_type);
    insert_static_blob_headers(response.headers_mut(), &etag, length);
    if partial {
        let end = offset + length - 1;
        insert_header(
            response.headers_mut(),
            CONTENT_RANGE,
            format!("bytes {offset}-{end}/{total}"),
        );
    }
    if let Some(disposition) = disposition {
        insert_header(response.headers_mut(), CONTENT_DISPOSITION, disposition);
    }
    response
}

pub(super) fn try_acquire_web_blob_read_permit(
    budget: Arc<Semaphore>,
) -> Option<OwnedSemaphorePermit> {
    budget.try_acquire_owned().ok()
}

pub(super) fn reader_body_until(
    mut reader: signalbox_blob_store::BlobReader,
    length: u64,
    permit: OwnedSemaphorePermit,
    deadline: Instant,
) -> Body {
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        let _permit = permit;
        let produce = async move {
            let mut remaining = length;
            while remaining > 0 {
                let capacity = usize::try_from(remaining.min(BLOB_STREAM_CHUNK_BYTES as u64))
                    .map_err(|_| io::Error::other("blob response length is invalid"))?;
                let mut buffer = vec![0_u8; capacity];
                let read = reader.read(&mut buffer).await?;
                if read == 0 {
                    return Err(io::Error::other(
                        "blob response ended before its declared length",
                    ));
                }
                buffer.truncate(read);
                remaining -=
                    u64::try_from(read).map_err(|_| io::Error::other("blob read is invalid"))?;
                sender
                    .send(Ok::<Bytes, io::Error>(Bytes::from(buffer)))
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "blob response closed")
                    })?;
            }
            Ok::<(), io::Error>(())
        };
        let _ = timeout_at(deadline, produce).await;
    });
    Body::from_stream(stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    }))
}

pub(super) fn parse_byte_range(value: &HeaderValue, total: u64) -> Result<(u64, u64, bool), ()> {
    if value.as_bytes().contains(&b',') {
        return Err(());
    }
    let mut headers = HeaderMap::new();
    headers.insert(RANGE, value.clone());
    let range = headers
        .typed_try_get::<TypedRange>()
        .map_err(|_| ())?
        .ok_or(())?;
    let mut ranges = range.satisfiable_ranges(total);
    let (start, end) = ranges.next().ok_or(())?;
    if ranges.next().is_some() {
        return Err(());
    }
    let std::ops::Bound::Included(start) = start else {
        return Err(());
    };
    let end = match end {
        std::ops::Bound::Included(end) => end.min(total.saturating_sub(1)),
        std::ops::Bound::Unbounded => total.checked_sub(1).ok_or(())?,
        std::ops::Bound::Excluded(_) => return Err(()),
    };
    if start > end || start >= total {
        return Err(());
    }
    Ok((start, end - start + 1, true))
}

/// Reports the `Range` field a blob response applies, once `If-Range` has decided.
///
/// A failed `If-Range` condition makes the whole `Range` field inapplicable, so
/// the condition is evaluated before the field is validated. A field this
/// endpoint would otherwise reject — repeated occurrences included — is then
/// ignored and the full representation is served, rather than answered with
/// `416`; `Err` is reserved for a rejectable field the condition admitted.
pub(super) fn applicable_range_header<'headers>(
    headers: &'headers HeaderMap,
    etag: &str,
) -> Result<Option<&'headers HeaderValue>, ()> {
    if !if_range_matches(headers, etag) {
        return Ok(None);
    }
    single_range_header(headers)
}

pub(super) fn single_range_header(headers: &HeaderMap) -> Result<Option<&HeaderValue>, ()> {
    let mut values = headers.get_all(RANGE).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    Ok(first)
}

pub(super) fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    let Ok(etag) = etag.parse::<TypedEtag>() else {
        return false;
    };
    headers
        .typed_try_get::<TypedIfNoneMatch>()
        .ok()
        .flatten()
        .is_some_and(|condition| !condition.precondition_passes(&etag))
}

pub(super) fn if_range_matches(headers: &HeaderMap, etag: &str) -> bool {
    let mut values = headers.get_all(IF_RANGE).iter();
    if values.next().is_none() {
        return true;
    }
    if values.next().is_some() {
        return false;
    }
    let Ok(etag) = etag.parse::<TypedEtag>() else {
        return false;
    };
    headers
        .typed_try_get::<TypedIfRange>()
        .ok()
        .flatten()
        .is_some_and(|condition| !condition.is_modified(Some(&etag), None))
}

fn not_modified_response(etag: &str) -> Response {
    let mut response = StatusCode::NOT_MODIFIED.into_response();
    insert_header(response.headers_mut(), ETAG, etag.to_owned());
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
    );
    response
}

fn range_not_satisfiable(total: u64, etag: &str) -> Response {
    let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    insert_header(
        response.headers_mut(),
        CONTENT_RANGE,
        format!("bytes */{total}"),
    );
    insert_header(response.headers_mut(), ETAG, etag.to_owned());
    response
}

fn insert_static_blob_headers(headers: &mut HeaderMap, etag: &str, length: u64) {
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(IMMUTABLE_CACHE_CONTROL),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    insert_header(headers, ETAG, etag.to_owned());
    insert_header(headers, CONTENT_LENGTH, length.to_string());
}

fn insert_header(headers: &mut HeaderMap, name: axum::http::HeaderName, value: String) {
    if let Ok(value) = HeaderValue::from_str(&value) {
        headers.insert(name, value);
    }
}

pub(super) fn content_disposition(filename: &str) -> String {
    const RFC_5987_VALUE: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'%')
        .add(b'\'')
        .add(b'(')
        .add(b')')
        .add(b'*')
        .add(b',')
        .add(b'/')
        .add(b':')
        .add(b';')
        .add(b'<')
        .add(b'=')
        .add(b'>')
        .add(b'?')
        .add(b'@')
        .add(b'[')
        .add(b'\\')
        .add(b']')
        .add(b'{')
        .add(b'}');
    let encoded = utf8_percent_encode(filename, RFC_5987_VALUE);
    format!("attachment; filename=\"download\"; filename*=UTF-8''{encoded}")
}

fn runtime_error_response(error: WebBlobRuntimeError) -> Response {
    match error {
        WebBlobRuntimeError::NotFound => application_error(
            StatusCode::NOT_FOUND,
            "blob_not_found",
            "blob does not exist",
        ),
        WebBlobRuntimeError::Busy => application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_derivation_busy",
            "blob derivative capacity is busy",
        ),
        WebBlobRuntimeError::Corrupt
        | WebBlobRuntimeError::Unavailable
        | WebBlobRuntimeError::IsolationUnavailable
        | WebBlobRuntimeError::ProducerFailed
        | WebBlobRuntimeError::Integrity => application_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "blob_unavailable",
            "blob content is temporarily unavailable",
        ),
    }
}

fn blob_read_error_response(error: crate::blob_read_runtime::BlobReadError) -> Response {
    use crate::blob_read_runtime::BlobReadError;
    match error {
        BlobReadError::NotFound => runtime_error_response(WebBlobRuntimeError::NotFound),
        BlobReadError::RangeOutOfBounds => application_error(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "blob_range_not_satisfiable",
            "blob byte range is not satisfiable",
        ),
        BlobReadError::Missing
        | BlobReadError::Corrupt
        | BlobReadError::Unavailable
        | BlobReadError::Integrity => runtime_error_response(WebBlobRuntimeError::Unavailable),
    }
}
