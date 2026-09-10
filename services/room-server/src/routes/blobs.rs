use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::{HeaderMap, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
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
                error: format!("Invalid size_bytes: must be between 1 and {MAX_BLOB_SIZE_BYTES} bytes"),
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

    // Quota check per room
    let current_room_bytes: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(size_bytes), 0) FROM room_blobs WHERE uploaded_by_room_id = $1",
    )
    .bind(&room_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    if (current_room_bytes as u64) + payload.size_bytes > state.storage.room_quota_bytes() {
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
                error: "Access denied: this blob is not referenced by any manifest in this room".to_string(),
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

    if !state.storage.verify_grant(&room_id, &clean_hash, "upload", expires, &grant) {
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
        headers.insert("Upload-Offset", HeaderValue::from_str(&size.to_string()).unwrap());
        headers.insert("X-Content-SHA256", HeaderValue::from_str(&clean_hash).unwrap());
        headers.insert(ETAG, HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap());
    } else {
        let offset = state.storage.partial_offset(&clean_hash);
        headers.insert("Upload-Offset", HeaderValue::from_str(&offset.to_string()).unwrap());
        headers.insert(ETAG, HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap());
    }

    Ok(res)
}

/// Handle PUT request for uploading (and resuming) immutable blob data.
pub async fn put_upload_blob(
    State(state): State<AppState>,
    Path(content_hash): Path<String>,
    Query(query): Query<TransferGrantQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let clean_hash = content_hash.trim().to_lowercase();
    let room_id = query.room_id.unwrap_or_default();
    let grant = query.grant.unwrap_or_default();
    let expires = query.expires.unwrap_or(0);

    if !state.storage.verify_grant(&room_id, &clean_hash, "upload", expires, &grant) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "Invalid or expired upload grant".to_string(),
                code: "INVALID_GRANT".to_string(),
                details: None,
            }),
        ));
    }

    // Idempotent: if already complete and verified, do not rewrite
    if state.storage.blob_exists(&clean_hash) {
        let mut res = StatusCode::OK.into_response();
        let h = res.headers_mut();
        h.insert("X-Content-SHA256", HeaderValue::from_str(&clean_hash).unwrap());
        h.insert(ETAG, HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap());
        return Ok(res);
    }

    let (offset, total_size) = parse_content_range(&headers, body.len() as u64)?;

    state
        .storage
        .write_partial_chunk(&clean_hash, offset, &body)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to write chunk to storage: {e}"),
                    code: "STORAGE_IO_ERROR".to_string(),
                    details: None,
                }),
            )
        })?;

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

        let _ = sqlx::query(
            "INSERT INTO room_blobs (content_hash, size_bytes, format, storage_path, uploaded_by_room_id, created_at, last_accessed_at) \
             VALUES ($1, $2, 'modpkg', $3, $4, NOW(), NOW()) \
             ON CONFLICT (content_hash) DO UPDATE SET last_accessed_at = NOW()",
        )
        .bind(&clean_hash)
        .bind(total_size as i64)
        .bind(blob_path.to_string_lossy().to_string())
        .bind(&room_id)
        .execute(&state.db)
        .await;

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
        h.insert("X-Content-SHA256", HeaderValue::from_str(&clean_hash).unwrap());
        h.insert(ETAG, HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap());
        Ok(res)
    } else {
        let mut res = StatusCode::PARTIAL_CONTENT.into_response();
        let h = res.headers_mut();
        h.insert("Upload-Offset", HeaderValue::from_str(&current_partial_len.to_string()).unwrap());
        h.insert(ETAG, HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap());
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

    if !state.storage.verify_grant(&room_id, &clean_hash, "download", expires, &grant) {
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
    h.insert(CONTENT_LENGTH, HeaderValue::from_str(&size.to_string()).unwrap());
    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(ETAG, HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap());
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));

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

    if !state.storage.verify_grant(&room_id, &clean_hash, "download", expires, &grant) {
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
    let mut file = File::open(&file_path).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Blob file not found on disk".to_string(),
                code: "BLOB_NOT_FOUND".to_string(),
                details: None,
            }),
        )
    })?;

    let total_size = file.metadata().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Failed to read blob metadata: {e}"),
                code: "IO_ERROR".to_string(),
                details: None,
            }),
        )
    })?.len();

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
                    file.seek(SeekFrom::Start(start)).map_err(|e| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ErrorResponse {
                                error: format!("Seek failed: {e}"),
                                code: "IO_ERROR".to_string(),
                                details: None,
                            }),
                        )
                    })?;

                    let mut buffer = vec![0u8; length as usize];
                    file.read_exact(&mut buffer).map_err(|e| {
                        (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(ErrorResponse {
                                error: format!("Read range failed: {e}"),
                                code: "IO_ERROR".to_string(),
                                details: None,
                            }),
                        )
                    })?;

                    let mut res = (StatusCode::PARTIAL_CONTENT, Bytes::from(buffer)).into_response();
                    let h = res.headers_mut();
                    h.insert(
                        CONTENT_RANGE,
                        HeaderValue::from_str(&format!("bytes {start}-{end}/{total_size}")).unwrap(),
                    );
                    h.insert(CONTENT_LENGTH, HeaderValue::from_str(&length.to_string()).unwrap());
                    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                    h.insert(ETAG, HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap());
                    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
                    return Ok(res);
                }
            }
        }
    }

    let mut buffer = Vec::with_capacity(total_size as usize);
    file.read_to_end(&mut buffer).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Read error: {e}"),
                code: "IO_ERROR".to_string(),
                details: None,
            }),
        )
    })?;

    let mut res = (StatusCode::OK, Bytes::from(buffer)).into_response();
    let h = res.headers_mut();
    h.insert(CONTENT_LENGTH, HeaderValue::from_str(&total_size.to_string()).unwrap());
    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(ETAG, HeaderValue::from_str(&format!("\"{clean_hash}\"")).unwrap());
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{clean_hash}.bin\"")).unwrap(),
    );

    Ok(res)
}

fn parse_content_range(headers: &HeaderMap, body_len: u64) -> Result<(u64, u64), (StatusCode, Json<ErrorResponse>)> {
    if let Some(range_header) = headers.get(CONTENT_RANGE).and_then(|h| h.to_str().ok()) {
        if let Some(range_str) = range_header.strip_prefix("bytes ") {
            let parts: Vec<&str> = range_str.split('/').collect();
            if parts.len() == 2 {
                let range_parts: Vec<&str> = parts[0].split('-').collect();
                if range_parts.len() == 2 {
                    if let (Ok(start), Ok(total)) = (range_parts[0].parse::<u64>(), parts[1].parse::<u64>()) {
                        return Ok((start, total));
                    }
                }
            }
        }
    }

    Ok((0, body_len))
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
