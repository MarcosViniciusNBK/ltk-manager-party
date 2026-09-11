use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{
    HeaderMap, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
    ETAG,
};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use tracing::info;

use crate::audit::record_audit_event;
use crate::error::ErrorResponse;
use crate::routes::rooms::extract_token;
use crate::state::AppState;
use crate::storage::{StorageManager, MAX_BLOB_SIZE_BYTES};

#[derive(Debug, Deserialize)]
pub struct CheckBlobsRequest {
    pub hashes: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct CheckBlobsResponse {
    pub existing_hashes: Vec<String>,
    pub missing_hashes: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct RequestUploadUrlRequest {
    pub content_hash: String,
    pub size_bytes: u64,
    pub format: String,
}

#[derive(Debug, Serialize)]
pub struct RequestUploadUrlResponse {
    pub content_hash: String,
    pub upload_url: String,
    pub expires_at: u64,
}

#[derive(Debug, Serialize)]
pub struct RequestDownloadUrlResponse {
    pub content_hash: String,
    pub download_url: String,
    pub expires_at: u64,
}

#[derive(Debug, Deserialize)]
pub struct TransferGrantQuery {
    pub room_id: Option<String>,
    pub grant: Option<String>,
    pub expires: Option<u64>,
}

/// Check which hashes already exist in storage and which are missing.
pub async fn check_blobs(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<CheckBlobsRequest>,
) -> Result<Json<CheckBlobsResponse>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;
    let (_member_id, _role) = verify_room_member(&state, &room_id, &token).await?;

    let mut existing_hashes = Vec::new();
    let mut missing_hashes = Vec::new();

    for hash in payload.hashes {
        let clean = hash.trim().to_lowercase();
        if !StorageManager::is_safe_hash(&clean) {
            continue;
        }

        if state.storage.blob_exists(&clean) {
            existing_hashes.push(clean);
        } else {
            missing_hashes.push(clean);
        }
    }

    Ok(Json(CheckBlobsResponse {
        existing_hashes,
        missing_hashes,
    }))
}

/// Request a short-lived HMAC-signed upload URL for a specific immutable content hash.
pub async fn request_upload_url(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<RequestUploadUrlRequest>,
) -> Result<Json<RequestUploadUrlResponse>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;
    let (member_id, role) = verify_room_member(&state, &room_id, &token).await?;

    let clean_hash = payload.content_hash.trim().to_lowercase();
    if !StorageManager::is_safe_hash(&clean_hash) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid content_hash: must be 64-char lowercase hex SHA-256".to_string(),
                code: "INVALID_CONTENT_HASH".to_string(),
                details: None,
            }),
        ));
    }

    if payload.size_bytes == 0 || payload.size_bytes > MAX_BLOB_SIZE_BYTES {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!(
                    "Invalid size_bytes: must be between 1 and {MAX_BLOB_SIZE_BYTES} bytes"
                ),
                code: "INVALID_SIZE".to_string(),
                details: None,
            }),
        ));
    }

    let format = payload.format.trim().to_lowercase();
    if format != "modpkg" && format != "fantome" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid format: supported formats are 'modpkg' and 'fantome'".to_string(),
                code: "INVALID_FORMAT".to_string(),
                details: None,
            }),
        ));
    }

    let existing_metadata: Option<(i64, String)> =
        sqlx::query_as("SELECT size_bytes, format FROM room_blobs WHERE content_hash = $1")
            .bind(&clean_hash)
            .fetch_optional(&state.db)
            .await
            .map_err(blob_db_error)?;
    if let Some((existing_size, existing_format)) = &existing_metadata {
        if *existing_size != payload.size_bytes as i64 || existing_format != &format {
            return Err((
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: "Existing content hash has different immutable metadata".to_string(),
                    code: "BLOB_METADATA_MISMATCH".to_string(),
                    details: None,
                }),
            ));
        }
    }

    // Quota check per room
    let current_room_bytes: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(size_bytes), 0) FROM room_blobs WHERE uploaded_by_room_id = $1",
    )
    .bind(&room_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    let additional_bytes = if existing_metadata.is_some() {
        0
    } else {
        payload.size_bytes
    };
    if (current_room_bytes as u64) + additional_bytes > state.storage.room_quota_bytes() {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(ErrorResponse {
                error: "Room storage quota exceeded (limit 5 GB)".to_string(),
                code: "QUOTA_EXCEEDED".to_string(),
                details: None,
            }),
        ));
    }

    let (upload_url, expires_at) = state.storage.build_upload_url(&room_id, &clean_hash);

    // Persist the declared immutable-object metadata before issuing the grant. The upload URL
    // deliberately contains no user-controlled format field.
    sqlx::query(
        "INSERT INTO room_blobs (content_hash, size_bytes, format, storage_path, uploaded_by_room_id, created_at, last_accessed_at) \
         VALUES ($1, $2, $3, NULL, $4, NOW(), NOW()) \
         ON CONFLICT (content_hash) DO UPDATE SET last_accessed_at = NOW()",
    )
    .bind(&clean_hash)
    .bind(payload.size_bytes as i64)
    .bind(&format)
    .bind(&room_id)
    .execute(&state.db)
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to reserve blob metadata".to_string(),
                code: "DATABASE_ERROR".to_string(),
                details: Some(error.to_string()),
            }),
        )
    })?;

    record_audit_event(
        &state.db,
        &room_id,
        &member_id,
        &role,
        "upload_url_requested",
        Some(serde_json::json!({
            "content_hash": clean_hash,
            "size_bytes": payload.size_bytes,
            "format": format
        })),
        None,
    )
    .await;

    Ok(Json(RequestUploadUrlResponse {
        content_hash: clean_hash,
        upload_url,
        expires_at,
    }))
}

/// Request a short-lived HMAC-signed download URL for a specific content hash.
/// A member can access only blobs referenced by a room they have joined.
pub async fn request_download_url(
    State(state): State<AppState>,
    Path((room_id, content_hash)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<RequestDownloadUrlResponse>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;
    let (member_id, role) = verify_room_member(&state, &room_id, &token).await?;

    let clean_hash = content_hash.trim().to_lowercase();
    if !StorageManager::is_safe_hash(&clean_hash) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid content_hash format".to_string(),
                code: "INVALID_CONTENT_HASH".to_string(),
                details: None,
            }),
        ));
    }

    if !state.storage.blob_exists(&clean_hash) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Blob '{clean_hash}' not found in storage"),
                code: "BLOB_NOT_FOUND".to_string(),
                details: None,
            }),
        ));
    }

    // Zero-Trust Room Authorization Check: Is this blob referenced by any manifest in this room?
    let is_referenced: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS( \
             SELECT 1 FROM room_manifests rm, \
             jsonb_array_elements(rm.manifest_json->'mods') AS m \
             WHERE rm.room_id = $1 AND m->>'contentHash' = $2 \
         )",
    )
    .bind(&room_id)
    .bind(&clean_hash)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    if !is_referenced.unwrap_or(false) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "Access denied: this blob is not referenced by any manifest in this room"
                    .to_string(),
                code: "BLOB_NOT_IN_ROOM".to_string(),
                details: None,
            }),
        ));
    }

    let (download_url, expires_at) = state.storage.build_download_url(&room_id, &clean_hash);

    record_audit_event(
        &state.db,
        &room_id,
        &member_id,
        &role,
        "download_url_requested",
        Some(serde_json::json!({
            "content_hash": clean_hash
        })),
        None,
    )
    .await;

    Ok(Json(RequestDownloadUrlResponse {
        content_hash: clean_hash,
        download_url,
        expires_at,
    }))
}

/// Probe an upload endpoint using HEAD, returning Upload-Offset and X-Content-SHA256 if complete.
pub async fn probe_upload_blob(
    State(state): State<AppState>,
    Path(content_hash): Path<String>,
    Query(query): Query<TransferGrantQuery>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let clean_hash = content_hash.trim().to_lowercase();
    let room_id = query.room_id.unwrap_or_default();
    let grant = query.grant.unwrap_or_default();
    let expires = query.expires.unwrap_or(0);

    if !state
        .storage
        .verify_grant(&room_id, &clean_hash, "upload", expires, &grant)
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "Invalid or expired upload grant".to_string(),
                code: "INVALID_GRANT".to_string(),
                details: None,
            }),
        ));
    }

    let mut res = StatusCode::OK.into_response();
    let headers = res.headers_mut();

    if state.storage.blob_exists(&clean_hash) {
        let size = state.storage.blob_size(&clean_hash).unwrap_or(0);
        headers.insert(
            "Upload-Offset",
            HeaderValue::from_str(&size.to_string()).unwrap(),
        );
        headers.insert(
            "X-Content-SHA256",
            HeaderValue::from_str(&clean_hash).unwrap(),
        );
        headers.insert(
            ETAG,
            HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap(),
        );
    } else {
        let offset = state.storage.partial_offset(&clean_hash);
        headers.insert(
            "Upload-Offset",
            HeaderValue::from_str(&offset.to_string()).unwrap(),
        );
        headers.insert(
            ETAG,
            HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap(),
        );
    }

    Ok(res)
}

/// Handle PUT request for uploading (and resuming) immutable blob data.
pub async fn put_upload_blob(
    State(state): State<AppState>,
    Path(content_hash): Path<String>,
    Query(query): Query<TransferGrantQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let clean_hash = content_hash.trim().to_lowercase();
    let room_id = query.room_id.unwrap_or_default();
    let grant = query.grant.unwrap_or_default();
    let expires = query.expires.unwrap_or(0);

    if !state
        .storage
        .verify_grant(&room_id, &clean_hash, "upload", expires, &grant)
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "Invalid or expired upload grant".to_string(),
                code: "INVALID_GRANT".to_string(),
                details: None,
            }),
        ));
    }

    let _upload_guard = state.lock_upload(&clean_hash).await;

    // Idempotent: if already complete and verified, do not rewrite
    if state.storage.blob_exists(&clean_hash) {
        let blob_path = state.storage.blob_path(&clean_hash);
        let size = state.storage.blob_size(&clean_hash).unwrap_or_default();
        sqlx::query(
            "UPDATE room_blobs SET size_bytes = $1, storage_path = $2, \
             uploaded_by_room_id = COALESCE(uploaded_by_room_id, $3), last_accessed_at = NOW() \
             WHERE content_hash = $4",
        )
        .bind(size as i64)
        .bind(blob_path.to_string_lossy().to_string())
        .bind(&room_id)
        .bind(&clean_hash)
        .execute(&state.db)
        .await
        .map_err(blob_db_error)?;
        let mut res = StatusCode::OK.into_response();
        let h = res.headers_mut();
        h.insert(
            "X-Content-SHA256",
            HeaderValue::from_str(&clean_hash).unwrap(),
        );
        h.insert(
            ETAG,
            HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap(),
        );
        return Ok(res);
    }

    let content_length = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| bad_request("CONTENT_LENGTH_REQUIRED", "Content-Length is required"))?;
    let (offset, total_size) = parse_content_range(&headers, content_length)?;
    let end_exclusive = offset.checked_add(content_length).ok_or_else(|| {
        bad_request(
            "INVALID_UPLOAD_RANGE",
            "Upload range exceeds integer limits",
        )
    })?;
    if content_length == 0
        || total_size == 0
        || total_size > MAX_BLOB_SIZE_BYTES
        || end_exclusive > total_size
    {
        return Err(bad_request(
            "INVALID_UPLOAD_RANGE",
            "Upload range exceeds the allowed blob size",
        ));
    }

    let expected_offset = state.storage.partial_offset(&clean_hash);
    if offset != expected_offset {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: format!("Upload must resume at byte {expected_offset}"),
                code: "UPLOAD_OFFSET_MISMATCH".to_string(),
                details: Some(expected_offset.to_string()),
            }),
        ));
    }

    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(state.storage.partial_path(&clean_hash))
        .await
        .map_err(storage_io_error)?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(storage_io_error)?;

    let mut stream = body.into_data_stream();
    let mut received = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            bad_request(
                "INVALID_UPLOAD_BODY",
                &format!("Could not read upload body: {error}"),
            )
        })?;
        received = received.saturating_add(chunk.len() as u64);
        if received > content_length {
            return Err(bad_request(
                "UPLOAD_TOO_LARGE",
                "Upload body exceeded Content-Length",
            ));
        }
        file.write_all(&chunk).await.map_err(storage_io_error)?;
    }
    if received != content_length {
        return Err(bad_request(
            "UPLOAD_SIZE_MISMATCH",
            "Upload body did not match Content-Length",
        ));
    }
    file.sync_data().await.map_err(storage_io_error)?;
    drop(file);

    let current_partial_len = state.storage.partial_offset(&clean_hash);

    if current_partial_len == total_size {
        let blob_path = state
            .storage
            .finalize_upload(&clean_hash, total_size)
            .map_err(|err| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("Upload integrity check failed: {err}"),
                        code: "INTEGRITY_MISMATCH".to_string(),
                        details: None,
                    }),
                )
            })?;

        sqlx::query(
            "INSERT INTO room_blobs (content_hash, size_bytes, format, storage_path, uploaded_by_room_id, created_at, last_accessed_at) \
             VALUES ($1, $2, COALESCE((SELECT format FROM room_blobs WHERE content_hash = $1), 'modpkg'), $3, $4, NOW(), NOW()) \
             ON CONFLICT (content_hash) DO UPDATE SET size_bytes = EXCLUDED.size_bytes, storage_path = EXCLUDED.storage_path, \
             uploaded_by_room_id = COALESCE(room_blobs.uploaded_by_room_id, EXCLUDED.uploaded_by_room_id), last_accessed_at = NOW()",
        )
        .bind(&clean_hash)
        .bind(total_size as i64)
        .bind(blob_path.to_string_lossy().to_string())
        .bind(&room_id)
        .execute(&state.db)
        .await
        .map_err(blob_db_error)?;

        record_audit_event(
            &state.db,
            &room_id,
            "uploader",
            "member",
            "blob_uploaded",
            Some(serde_json::json!({
                "content_hash": clean_hash,
                "size_bytes": total_size
            })),
            None,
        )
        .await;

        info!(content_hash = %clean_hash, size = total_size, "Upload completed and verified successfully");

        let mut res = StatusCode::OK.into_response();
        let h = res.headers_mut();
        h.insert(
            "X-Content-SHA256",
            HeaderValue::from_str(&clean_hash).unwrap(),
        );
        h.insert(
            ETAG,
            HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap(),
        );
        Ok(res)
    } else {
        let mut res = StatusCode::PARTIAL_CONTENT.into_response();
        let h = res.headers_mut();
        h.insert(
            "Upload-Offset",
            HeaderValue::from_str(&current_partial_len.to_string()).unwrap(),
        );
        h.insert(
            ETAG,
            HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap(),
        );
        Ok(res)
    }
}

/// Probe a download endpoint using HEAD.
pub async fn probe_download_blob(
    State(state): State<AppState>,
    Path(content_hash): Path<String>,
    Query(query): Query<TransferGrantQuery>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let clean_hash = content_hash.trim().to_lowercase();
    let room_id = query.room_id.unwrap_or_default();
    let grant = query.grant.unwrap_or_default();
    let expires = query.expires.unwrap_or(0);

    if !state
        .storage
        .verify_grant(&room_id, &clean_hash, "download", expires, &grant)
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "Invalid or expired download grant".to_string(),
                code: "INVALID_GRANT".to_string(),
                details: None,
            }),
        ));
    }

    let size = state.storage.blob_size(&clean_hash).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Blob file not found".to_string(),
                code: "BLOB_NOT_FOUND".to_string(),
                details: None,
            }),
        )
    })?;

    let mut res = StatusCode::OK.into_response();
    let h = res.headers_mut();
    h.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&size.to_string()).unwrap(),
    );
    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        ETAG,
        HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap(),
    );
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );

    Ok(res)
}

/// Download immutable blob with support for HTTP Range (206 Partial Content).
pub async fn get_download_blob(
    State(state): State<AppState>,
    Path(content_hash): Path<String>,
    Query(query): Query<TransferGrantQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let clean_hash = content_hash.trim().to_lowercase();
    let room_id = query.room_id.unwrap_or_default();
    let grant = query.grant.unwrap_or_default();
    let expires = query.expires.unwrap_or(0);

    if !state
        .storage
        .verify_grant(&room_id, &clean_hash, "download", expires, &grant)
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "Invalid or expired download grant".to_string(),
                code: "INVALID_GRANT".to_string(),
                details: None,
            }),
        ));
    }

    let file_path = state.storage.blob_path(&clean_hash);
    let mut file = tokio::fs::File::open(&file_path).await.map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Blob file not found on disk".to_string(),
                code: "BLOB_NOT_FOUND".to_string(),
                details: None,
            }),
        )
    })?;

    let total_size = file
        .metadata()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to read blob metadata: {e}"),
                    code: "IO_ERROR".to_string(),
                    details: None,
                }),
            )
        })?
        .len();

    let _ = sqlx::query("UPDATE room_blobs SET last_accessed_at = NOW() WHERE content_hash = $1")
        .bind(&clean_hash)
        .execute(&state.db)
        .await;

    if let Some(range_header) = headers.get("range").and_then(|h| h.to_str().ok()) {
        if let Some(range_spec) = range_header.strip_prefix("bytes=") {
            let parts: Vec<&str> = range_spec.split('-').collect();
            if let Ok(start) = parts[0].parse::<u64>() {
                let end = if parts.len() > 1 && !parts[1].is_empty() {
                    parts[1].parse::<u64>().unwrap_or(total_size - 1)
                } else {
                    total_size - 1
                };

                let end = end.min(total_size - 1);
                if start <= end {
                    let length = end - start + 1;
                    file.seek(std::io::SeekFrom::Start(start))
                        .await
                        .map_err(|e| {
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(ErrorResponse {
                                    error: format!("Seek failed: {e}"),
                                    code: "IO_ERROR".to_string(),
                                    details: None,
                                }),
                            )
                        })?;

                    let stream = ReaderStream::new(file.take(length));
                    let mut res = Response::new(Body::from_stream(stream));
                    *res.status_mut() = StatusCode::PARTIAL_CONTENT;
                    let h = res.headers_mut();
                    h.insert(
                        CONTENT_RANGE,
                        HeaderValue::from_str(&format!("bytes {start}-{end}/{total_size}"))
                            .unwrap(),
                    );
                    h.insert(
                        CONTENT_LENGTH,
                        HeaderValue::from_str(&length.to_string()).unwrap(),
                    );
                    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                    h.insert(
                        ETAG,
                        HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap(),
                    );
                    h.insert(
                        CONTENT_TYPE,
                        HeaderValue::from_static("application/octet-stream"),
                    );
                    return Ok(res);
                }
            }
        }
    }

    let stream = ReaderStream::new(file);
    let mut res = Response::new(Body::from_stream(stream));
    *res.status_mut() = StatusCode::OK;
    let h = res.headers_mut();
    h.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&total_size.to_string()).unwrap(),
    );
    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        ETAG,
        HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap(),
    );
    h.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{clean_hash}.bin\"")).unwrap(),
    );

    Ok(res)
}

fn parse_content_range(
    headers: &HeaderMap,
    body_len: u64,
) -> Result<(u64, u64), (StatusCode, Json<ErrorResponse>)> {
    if let Some(range_header) = headers.get(CONTENT_RANGE).and_then(|h| h.to_str().ok()) {
        if let Some(range_str) = range_header.strip_prefix("bytes ") {
            let parts: Vec<&str> = range_str.split('/').collect();
            if parts.len() == 2 {
                let range_parts: Vec<&str> = parts[0].split('-').collect();
                if range_parts.len() == 2 {
                    if let (Ok(start), Ok(end), Ok(total)) = (
                        range_parts[0].parse::<u64>(),
                        range_parts[1].parse::<u64>(),
                        parts[1].parse::<u64>(),
                    ) {
                        let expected_end = start
                            .checked_add(body_len.saturating_sub(1))
                            .ok_or_else(|| {
                                bad_request(
                                    "INVALID_UPLOAD_RANGE",
                                    "Content-Range exceeds integer limits",
                                )
                            })?;
                        if body_len > 0 && end == expected_end {
                            return Ok((start, total));
                        }
                    }
                }
            }
        }
        return Err(bad_request(
            "INVALID_UPLOAD_RANGE",
            "Content-Range must match the request body exactly",
        ));
    }

    Ok((0, body_len))
}

fn bad_request(code: &str, error: &str) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ErrorResponse {
            error: error.to_string(),
            code: code.to_string(),
            details: None,
        }),
    )
}

fn storage_io_error(error: std::io::Error) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: format!("Failed to write upload to storage: {error}"),
            code: "STORAGE_IO_ERROR".to_string(),
            details: None,
        }),
    )
}

fn blob_db_error(error: sqlx::Error) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: "Blob metadata database operation failed".to_string(),
            code: "DATABASE_ERROR".to_string(),
            details: Some(error.to_string()),
        }),
    )
}

async fn verify_room_member(
    state: &AppState,
    room_id: &str,
    token: &str,
) -> Result<(String, String), (StatusCode, Json<ErrorResponse>)> {
    let member_info: Option<(String, String)> = sqlx::query_as(
        "SELECT member_id, role FROM room_members WHERE room_id = $1 AND member_token = $2 \
         UNION \
         SELECT rm.member_id, 'owner' as role FROM room_members rm JOIN rooms r ON rm.room_id = r.room_id \
         WHERE r.room_id = $1 AND r.owner_token = $2 LIMIT 1",
    )
    .bind(room_id)
    .bind(token)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    match member_info {
        Some(info) => Ok(info),
        None => Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid or missing token for this room".to_string(),
                code: "UNAUTHORIZED".to_string(),
                details: None,
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_must_match_the_body_exactly() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 10-19/30"));
        assert_eq!(parse_content_range(&headers, 10).unwrap(), (10, 30));

        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 10-20/30"));
        let error = parse_content_range(&headers, 10).unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(error.1.code, "INVALID_UPLOAD_RANGE");
    }

    #[test]
    fn upload_without_content_range_is_a_full_body() {
        assert_eq!(parse_content_range(&HeaderMap::new(), 42).unwrap(), (0, 42));
    }
}
