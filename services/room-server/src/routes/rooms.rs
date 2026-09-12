use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::audit::{record_audit_event, AuditLogEntry};
use crate::auth::{generate_high_entropy_token, hash_password, verify_password};
use crate::error::ErrorResponse;
use crate::manifest::{RoomManifest, RoomModFormat};
use crate::state::{AppState, RoomEvent};

#[derive(Debug, Deserialize)]
pub struct CreateRoomRequest {
    pub room_id: String,
    pub password: String,
    pub game_build: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateRoomResponse {
    pub room_id: String,
    pub member_id: String,
    pub owner_token: String,
    pub member_token: String,
    pub role: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct JoinRoomRequest {
    pub password: String,
    pub member_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct JoinRoomResponse {
    pub room_id: String,
    pub member_id: String,
    pub member_token: String,
    pub role: &'static str,
    pub revision: i64,
}

#[derive(Debug, Serialize)]
pub struct RoomInfoResponse {
    pub room_id: String,
    pub revision: i64,
    pub game_build: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub active_members: i64,
}

#[derive(Debug, Deserialize)]
pub struct PublishManifestRequest {
    pub previous_revision: i64,
    pub manifest: RoomManifest,
}

#[derive(Debug, Serialize)]
pub struct PublishManifestResponse {
    pub room_id: String,
    pub revision: i64,
    pub mod_count: usize,
    pub total_size_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct AckRevisionRequest {
    pub revision: i64,
    pub status: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AckRevisionResponse {
    pub success: bool,
    pub member_id: String,
    pub revision: i64,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct MemberInfo {
    pub member_id: String,
    pub display_name: String,
    pub role: String,
    pub last_acknowledged_revision: i64,
    pub ack_status: String,
    pub joined_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
    pub is_online: bool,
    pub is_stale: bool,
}

#[derive(Debug, Deserialize)]
pub struct TransferOwnerRequest {
    pub new_owner_member_id: String,
}

#[derive(Debug, Serialize)]
pub struct TransferOwnerResponse {
    pub success: bool,
    pub room_id: String,
    pub previous_owner: String,
    pub new_owner: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMemberRequest {
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct UpdateMemberResponse {
    pub success: bool,
}

/// Create a new room with password protection and receive authoritative owner and member tokens.
pub async fn create_room(
    State(state): State<AppState>,
    Json(payload): Json<CreateRoomRequest>,
) -> Result<(StatusCode, Json<CreateRoomResponse>), (StatusCode, Json<ErrorResponse>)> {
    let room_id = payload.room_id.trim().to_lowercase();
    if room_id.len() < 3 || room_id.len() > 64 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Room ID must be between 3 and 64 characters".to_string(),
                code: "INVALID_ROOM_ID".to_string(),
                details: None,
            }),
        ));
    }

    if !room_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Room ID must contain only alphanumeric characters, dashes, or underscores"
                    .to_string(),
                code: "INVALID_ROOM_ID".to_string(),
                details: None,
            }),
        ));
    }

    if payload.password.len() < 4 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Password must be at least 4 characters".to_string(),
                code: "PASSWORD_TOO_SHORT".to_string(),
                details: None,
            }),
        ));
    }

    let password_hash = hash_password(&payload.password).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to hash password".to_string(),
                code: "HASHING_ERROR".to_string(),
                details: Some(e.to_string()),
            }),
        )
    })?;

    let owner_token = generate_high_entropy_token();
    let member_token = generate_high_entropy_token();
    let owner_member_id = format!("owner-{}", &owner_token[..8]);
    let owner_display_name = normalize_display_name(payload.display_name, &owner_member_id)?;

    let mut tx = state.db.begin().await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Database transaction failed".to_string(),
                code: "DATABASE_ERROR".to_string(),
                details: Some(e.to_string()),
            }),
        )
    })?;

    let exists: Option<String> = sqlx::query_scalar("SELECT room_id FROM rooms WHERE room_id = $1")
        .bind(&room_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_error)?;

    if exists.is_some() {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: format!("Room '{room_id}' already exists"),
                code: "ROOM_EXISTS".to_string(),
                details: None,
            }),
        ));
    }

    sqlx::query(
        "INSERT INTO rooms (room_id, revision, password_hash, owner_token, game_build, expires_at) \
         VALUES ($1, 0, $2, $3, $4, NOW() + INTERVAL '24 hours')",
    )
    .bind(&room_id)
    .bind(&password_hash)
    .bind(&owner_token)
    .bind(&payload.game_build)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;

    sqlx::query(
        "INSERT INTO room_members (room_id, member_id, display_name, member_token, role, last_acknowledged_revision, ack_status) \
         VALUES ($1, $2, $3, $4, 'owner', 0, 'synchronized')",
    )
    .bind(&room_id)
    .bind(&owner_member_id)
    .bind(&owner_display_name)
    .bind(&member_token)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;

    tx.commit().await.map_err(db_error)?;

    record_audit_event(
        &state.db,
        &room_id,
        &owner_member_id,
        "owner",
        "room_created",
        Some(serde_json::json!({
            "game_build": payload.game_build
        })),
        None,
    )
    .await;

    info!(room_id = %room_id, "Room created successfully with owner role");

    Ok((
        StatusCode::CREATED,
        Json(CreateRoomResponse {
            room_id,
            member_id: owner_member_id,
            owner_token,
            member_token,
            role: "owner",
        }),
    ))
}

/// Join an existing room with password authentication, protected by rate limiting.
pub async fn join_room(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<JoinRoomRequest>,
) -> Result<Json<JoinRoomResponse>, (StatusCode, Json<ErrorResponse>)> {
    let client_ip = extract_client_ip(&headers);

    if !state.rate_limiter.check(client_ip, &room_id).await {
        warn!(room_id = %room_id, ip = %client_ip, "Rate limit exceeded on join attempts");
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse {
                error: "Too many failed attempts. Please wait a minute and try again.".to_string(),
                code: "RATE_LIMITED".to_string(),
                details: None,
            }),
        ));
    }

    let room: Option<(String, i64, bool)> =
        sqlx::query_as("SELECT password_hash, revision, (expires_at < NOW()) as is_expired FROM rooms WHERE room_id = $1")
            .bind(&room_id)
            .fetch_optional(&state.db)
            .await
            .map_err(db_error)?;

    let Some((password_hash, revision, is_expired)) = room else {
        state.rate_limiter.record_failure(client_ip, &room_id).await;
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Room '{room_id}' not found"),
                code: "ROOM_NOT_FOUND".to_string(),
                details: None,
            }),
        ));
    };

    if is_expired {
        return Err((
            StatusCode::GONE,
            Json(ErrorResponse {
                error: format!("Room '{room_id}' has expired due to inactivity"),
                code: "ROOM_EXPIRED".to_string(),
                details: None,
            }),
        ));
    }

    if !verify_password(&payload.password, &password_hash) {
        state.rate_limiter.record_failure(client_ip, &room_id).await;
        warn!(room_id = %room_id, ip = %client_ip, "Authentication failed: invalid password");
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid room password".to_string(),
                code: "INVALID_PASSWORD".to_string(),
                details: None,
            }),
        ));
    }

    state.rate_limiter.reset(client_ip, &room_id).await;

    let member_token = generate_high_entropy_token();
    let member_id = payload
        .member_id
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| format!("member-{}", &member_token[..8]));
    let display_name = normalize_display_name(payload.display_name, &member_id)?;

    sqlx::query(
        "INSERT INTO room_members (room_id, member_id, display_name, member_token, role, last_acknowledged_revision, ack_status, last_seen_at) \
         VALUES ($1, $2, $3, $4, 'member', 0, 'joined', NOW()) \
         ON CONFLICT (room_id, member_id) DO UPDATE SET display_name = $3, member_token = $4, last_seen_at = NOW()",
    )
    .bind(&room_id)
    .bind(&member_id)
    .bind(&display_name)
    .bind(&member_token)
    .execute(&state.db)
    .await
    .map_err(db_error)?;

    let _ =
        sqlx::query("UPDATE rooms SET expires_at = NOW() + INTERVAL '24 hours' WHERE room_id = $1")
            .bind(&room_id)
            .execute(&state.db)
            .await;

    let channel = state.get_or_create_room_channel(&room_id).await;
    let _ = channel.send(RoomEvent {
        room_id: room_id.clone(),
        event_type: "member_joined".to_string(),
        payload: serde_json::json!({
            "member_id": member_id.clone(),
            "display_name": display_name.clone(),
            "role": "member",
        }),
    });

    record_audit_event(
        &state.db,
        &room_id,
        &member_id,
        "member",
        "member_joined",
        None,
        Some(&client_ip.to_string()),
    )
    .await;

    info!(room_id = %room_id, member_id = %member_id, "Member authenticated and joined room");

    Ok(Json(JoinRoomResponse {
        room_id,
        member_id,
        member_token,
        role: "member",
        revision,
    }))
}

/// Update the visible computer name for an authenticated membership. The opaque member ID remains
/// the authorization key, so duplicate hostnames cannot overwrite another user.
pub async fn update_member_display_name(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<UpdateMemberRequest>,
) -> Result<Json<UpdateMemberResponse>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;
    let (member_id, role) = authenticate_room_actor(&state, &room_id, &token).await?;
    let display_name = normalize_display_name(Some(payload.display_name), &member_id)?;

    sqlx::query(
        "UPDATE room_members SET display_name = $1, last_seen_at = NOW() \
         WHERE room_id = $2 AND member_id = $3",
    )
    .bind(&display_name)
    .bind(&room_id)
    .bind(&member_id)
    .execute(&state.db)
    .await
    .map_err(db_error)?;

    let channel = state.get_or_create_room_channel(&room_id).await;
    let _ = channel.send(RoomEvent {
        room_id: room_id.clone(),
        event_type: "member_updated".to_string(),
        payload: serde_json::json!({
            "member_id": member_id.clone(),
            "display_name": display_name.clone(),
            "role": role.clone(),
        }),
    });

    record_audit_event(
        &state.db,
        &room_id,
        &member_id,
        &role,
        "member_display_name_updated",
        None,
        None,
    )
    .await;

    Ok(Json(UpdateMemberResponse { success: true }))
}

/// Retrieve room metadata, requiring a valid member or owner token in the Authorization header.
pub async fn get_room_info(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RoomInfoResponse>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;
    verify_room_authorization(&state, &room_id, &token).await?;

    let room: Option<(i64, Option<String>, chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>, bool)> =
        sqlx::query_as("SELECT revision, game_build, created_at, expires_at, (expires_at < NOW()) as is_expired FROM rooms WHERE room_id = $1")
            .bind(&room_id)
            .fetch_optional(&state.db)
            .await
            .map_err(db_error)?;

    let Some((revision, game_build, created_at, expires_at, is_expired)) = room else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Room '{room_id}' not found"),
                code: "ROOM_NOT_FOUND".to_string(),
                details: None,
            }),
        ));
    };

    if is_expired {
        return Err((
            StatusCode::GONE,
            Json(ErrorResponse {
                error: format!("Room '{room_id}' has expired due to inactivity"),
                code: "ROOM_EXPIRED".to_string(),
                details: None,
            }),
        ));
    }

    let active_members: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM room_members WHERE room_id = $1 AND last_seen_at > NOW() - INTERVAL '5 minutes'",
    )
    .bind(&room_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(0);

    Ok(Json(RoomInfoResponse {
        room_id,
        revision,
        game_build,
        created_at,
        expires_at,
        active_members,
    }))
}

/// Publish a new authoritative manifest revision with Compare-and-Swap (CAS) update.
/// Every authenticated room member collaborates on the same shared profile. CAS prevents two
/// simultaneous edits from silently overwriting one another.
pub async fn publish_manifest(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<PublishManifestRequest>,
) -> Result<(StatusCode, Json<PublishManifestResponse>), (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;

    let room_res: Option<(i64, bool)> = sqlx::query_as(
        "SELECT revision, (expires_at < NOW()) as is_expired FROM rooms WHERE room_id = $1",
    )
    .bind(&room_id)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;

    let Some((current_revision, is_expired)) = room_res else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Room '{room_id}' not found"),
                code: "ROOM_NOT_FOUND".to_string(),
                details: None,
            }),
        ));
    };

    if is_expired {
        return Err((
            StatusCode::GONE,
            Json(ErrorResponse {
                error: format!("Room '{room_id}' has expired due to inactivity"),
                code: "ROOM_EXPIRED".to_string(),
                details: None,
            }),
        ));
    }

    let (actor_member_id, actor_role) = authenticate_room_actor(&state, &room_id, &token).await?;

    // CAS check: previous_revision must match current_revision
    if current_revision != payload.previous_revision {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "Manifest revision conflict: expected previous revision does not match current revision".to_string(),
                code: "REVISION_CONFLICT".to_string(),
                details: Some(format!(
                    "Current server revision is {}, but request asserted {}",
                    current_revision, payload.previous_revision
                )),
            }),
        ));
    }

    let new_revision = current_revision + 1;

    // Validate manifest content & limits
    payload
        .manifest
        .validate(&room_id, new_revision as u64)
        .map_err(|err| {
            (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("Manifest validation failed: {err}"),
                    code: "INVALID_MANIFEST".to_string(),
                    details: None,
                }),
            )
        })?;

    let mod_count = payload.manifest.mods.len();
    let total_size_bytes: u64 = payload.manifest.mods.iter().map(|m| m.size_bytes).sum();
    for room_mod in &payload.manifest.mods {
        let actual_size = state.storage.blob_size(&room_mod.content_hash);
        let expected_format = match room_mod.format {
            RoomModFormat::Modpkg => "modpkg",
            RoomModFormat::Fantome => "fantome",
        };
        let metadata: Option<(i64, String, Option<String>)> = sqlx::query_as(
            "SELECT size_bytes, format, storage_path FROM room_blobs WHERE content_hash = $1",
        )
        .bind(&room_mod.content_hash)
        .fetch_optional(&state.db)
        .await
        .map_err(db_error)?;
        let metadata_matches = metadata.as_ref().is_some_and(|(size, format, path)| {
            *size == room_mod.size_bytes as i64
                && format == expected_format
                && path.is_some()
                && actual_size == Some(room_mod.size_bytes)
        });
        if !metadata_matches {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error:
                        "Manifest references an incomplete blob or mismatched immutable metadata"
                            .to_string(),
                    code: "BLOB_METADATA_MISMATCH".to_string(),
                    details: Some(room_mod.content_hash.clone()),
                }),
            ));
        }
    }
    let manifest_json = serde_json::to_value(&payload.manifest).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: "Failed to serialize manifest JSON".to_string(),
                code: "SERIALIZATION_ERROR".to_string(),
                details: Some(e.to_string()),
            }),
        )
    })?;

    let mut tx = state.db.begin().await.map_err(db_error)?;

    // Atomic CAS update on rooms
    let update_res = sqlx::query(
        "UPDATE rooms SET revision = $1, game_build = $2, updated_at = NOW(), expires_at = NOW() + INTERVAL '24 hours' \
         WHERE room_id = $3 AND revision = $4",
    )
    .bind(new_revision)
    .bind(&payload.manifest.game_build)
    .bind(&room_id)
    .bind(current_revision)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;

    if update_res.rows_affected() == 0 {
        return Err((
            StatusCode::CONFLICT,
            Json(ErrorResponse {
                error: "Concurrent update conflict detected on room revision".to_string(),
                code: "REVISION_CONFLICT".to_string(),
                details: None,
            }),
        ));
    }

    // Insert into room_manifests
    sqlx::query(
        "INSERT INTO room_manifests (room_id, revision, schema_version, game_build, manifest_json, created_at) \
         VALUES ($1, $2, $3, $4, $5, NOW())",
    )
    .bind(&room_id)
    .bind(new_revision)
    .bind(payload.manifest.schema_version as i32)
    .bind(&payload.manifest.game_build)
    .bind(&manifest_json)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;

    // The publisher is automatically acknowledged at the revision it just created.
    sqlx::query(
        "UPDATE room_members SET last_acknowledged_revision = $1, ack_status = 'synchronized', last_seen_at = NOW() \
         WHERE room_id = $2 AND member_id = $3",
    )
    .bind(new_revision)
    .bind(&room_id)
    .bind(&actor_member_id)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;

    tx.commit().await.map_err(db_error)?;

    let channel = state.get_or_create_room_channel(&room_id).await;
    let _ = channel.send(RoomEvent {
        room_id: room_id.clone(),
        event_type: "manifest_published".to_string(),
        payload: serde_json::json!({
            "room_id": room_id,
            "revision": new_revision,
            "game_build": payload.manifest.game_build,
            "mod_count": mod_count,
            "total_size_bytes": total_size_bytes,
            "manifest": payload.manifest
        }),
    });

    record_audit_event(
        &state.db,
        &room_id,
        &actor_member_id,
        &actor_role,
        "manifest_published",
        Some(serde_json::json!({
            "revision": new_revision,
            "mod_count": mod_count,
            "total_size_bytes": total_size_bytes
        })),
        None,
    )
    .await;

    info!(
        room_id = %room_id,
        revision = new_revision,
        mods = mod_count,
        "Authoritative manifest published successfully"
    );

    Ok((
        StatusCode::CREATED,
        Json(PublishManifestResponse {
            room_id,
            revision: new_revision,
            mod_count,
            total_size_bytes,
        }),
    ))
}

/// Retrieve the latest published manifest for the room.
pub async fn get_latest_manifest(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;
    verify_room_authorization(&state, &room_id, &token).await?;

    let manifest: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT manifest_json FROM room_manifests WHERE room_id = $1 ORDER BY revision DESC LIMIT 1",
    )
    .bind(&room_id)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;

    let Some(manifest_json) = manifest else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("No manifest has been published for room '{room_id}' yet"),
                code: "NO_MANIFEST".to_string(),
                details: None,
            }),
        ));
    };

    Ok(Json(manifest_json))
}

/// Retrieve a specific revision manifest for the room.
pub async fn get_revision_manifest(
    State(state): State<AppState>,
    Path((room_id, revision)): Path<(String, i64)>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;
    verify_room_authorization(&state, &room_id, &token).await?;

    let manifest: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT manifest_json FROM room_manifests WHERE room_id = $1 AND revision = $2",
    )
    .bind(&room_id)
    .bind(revision)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;

    let Some(manifest_json) = manifest else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Manifest revision {revision} not found for room '{room_id}'"),
                code: "MANIFEST_REVISION_NOT_FOUND".to_string(),
                details: None,
            }),
        ));
    };

    Ok(Json(manifest_json))
}

/// Acknowledge synchronization state for a verified revision.
pub async fn ack_revision(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<AckRevisionRequest>,
) -> Result<Json<AckRevisionResponse>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;

    let member: Option<(String, String)> = sqlx::query_as(
        "SELECT member_id, role FROM room_members WHERE room_id = $1 AND member_token = $2 \
         UNION \
         SELECT member_id, 'owner' as role FROM room_members rm JOIN rooms r ON rm.room_id = r.room_id \
         WHERE r.room_id = $1 AND r.owner_token = $2 LIMIT 1",
    )
    .bind(&room_id)
    .bind(&token)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;

    let Some((member_id, _role)) = member else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid token for this room".to_string(),
                code: "UNAUTHORIZED".to_string(),
                details: None,
            }),
        ));
    };

    let status = payload.status.unwrap_or_else(|| "synchronized".to_string());

    sqlx::query(
        "UPDATE room_members \
         SET last_acknowledged_revision = $1, ack_status = $2, last_seen_at = NOW() \
         WHERE room_id = $3 AND member_id = $4",
    )
    .bind(payload.revision)
    .bind(&status)
    .bind(&room_id)
    .bind(&member_id)
    .execute(&state.db)
    .await
    .map_err(db_error)?;

    let _ =
        sqlx::query("UPDATE rooms SET expires_at = NOW() + INTERVAL '24 hours' WHERE room_id = $1")
            .bind(&room_id)
            .execute(&state.db)
            .await;

    let channel = state.get_or_create_room_channel(&room_id).await;
    let _ = channel.send(RoomEvent {
        room_id: room_id.clone(),
        event_type: "member_acknowledged".to_string(),
        payload: serde_json::json!({
            "room_id": room_id,
            "member_id": member_id,
            "revision": payload.revision,
            "status": status,
        }),
    });

    record_audit_event(
        &state.db,
        &room_id,
        &member_id,
        &_role,
        "revision_acknowledged",
        Some(serde_json::json!({
            "revision": payload.revision,
            "status": status
        })),
        None,
    )
    .await;

    info!(
        room_id = %room_id,
        member_id = %member_id,
        revision = payload.revision,
        status = %status,
        "Member acknowledged revision"
    );

    Ok(Json(AckRevisionResponse {
        success: true,
        member_id,
        revision: payload.revision,
        status,
    }))
}

/// Retrieve all members of a room, their roles, synchronization statuses, and presence.
pub async fn get_room_members(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<MemberInfo>>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;
    verify_room_authorization(&state, &room_id, &token).await?;

    let members: Vec<(
        String,
        String,
        String,
        i64,
        String,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::Utc>,
    )> = sqlx::query_as(
        "SELECT member_id, display_name, role, last_acknowledged_revision, ack_status, joined_at, last_seen_at \
         FROM room_members \
         WHERE room_id = $1 \
         ORDER BY (role = 'owner') DESC, joined_at ASC",
    )
    .bind(&room_id)
    .fetch_all(&state.db)
    .await
    .map_err(db_error)?;

    let mut result = Vec::with_capacity(members.len());
    let now = chrono::Utc::now();
    let stale_threshold = chrono::Duration::minutes(5);

    for (member_id, display_name, role, last_ack, ack_status, joined_at, last_seen_at) in members {
        let is_connected = state.is_member_connected(&room_id, &member_id).await;
        let is_recent = (now - last_seen_at) < stale_threshold;
        let is_online = is_connected || is_recent;
        let display_name = display_name_or_fallback(&display_name, &member_id);

        result.push(MemberInfo {
            member_id,
            display_name,
            role,
            last_acknowledged_revision: last_ack,
            ack_status,
            joined_at,
            last_seen_at,
            is_online,
            is_stale: !is_online,
        });
    }

    Ok(Json(result))
}

/// Transfer room ownership atomically to another active member.
/// Only the current owner can perform this operation.
pub async fn transfer_ownership(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<TransferOwnerRequest>,
) -> Result<Json<TransferOwnerResponse>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;

    let owner_info: Option<(String, String)> = sqlx::query_as(
        "SELECT r.owner_token, rm.member_id FROM rooms r \
         JOIN room_members rm ON r.room_id = rm.room_id AND rm.role = 'owner' \
         WHERE r.room_id = $1",
    )
    .bind(&room_id)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;

    let Some((owner_token, current_owner_id)) = owner_info else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Room '{room_id}' not found"),
                code: "ROOM_NOT_FOUND".to_string(),
                details: None,
            }),
        ));
    };

    if owner_token != token {
        return Err((
            StatusCode::FORBIDDEN,
            Json(ErrorResponse {
                error: "Only the current room owner can transfer ownership".to_string(),
                code: "NOT_ROOM_OWNER".to_string(),
                details: None,
            }),
        ));
    }

    if payload.new_owner_member_id == current_owner_id {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Target member is already the owner of this room".to_string(),
                code: "ALREADY_OWNER".to_string(),
                details: None,
            }),
        ));
    }

    let target_token: Option<String> = sqlx::query_scalar(
        "SELECT member_token FROM room_members WHERE room_id = $1 AND member_id = $2",
    )
    .bind(&room_id)
    .bind(&payload.new_owner_member_id)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;

    let Some(new_owner_token) = target_token else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!(
                    "Target member '{}' not found in room",
                    payload.new_owner_member_id
                ),
                code: "MEMBER_NOT_FOUND".to_string(),
                details: None,
            }),
        ));
    };

    let mut tx = state.db.begin().await.map_err(db_error)?;

    // 1. Assign target member's token as the room's owner_token
    sqlx::query("UPDATE rooms SET owner_token = $1, updated_at = NOW() WHERE room_id = $2")
        .bind(&new_owner_token)
        .bind(&room_id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;

    // 2. Set target member's role to 'owner'
    sqlx::query("UPDATE room_members SET role = 'owner' WHERE room_id = $1 AND member_id = $2")
        .bind(&room_id)
        .bind(&payload.new_owner_member_id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;

    // 3. Set previous owner's role to 'member'
    sqlx::query("UPDATE room_members SET role = 'member' WHERE room_id = $1 AND member_id = $2")
        .bind(&room_id)
        .bind(&current_owner_id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;

    tx.commit().await.map_err(db_error)?;

    let channel = state.get_or_create_room_channel(&room_id).await;
    let _ = channel.send(RoomEvent {
        room_id: room_id.clone(),
        event_type: "owner_transferred".to_string(),
        payload: serde_json::json!({
            "room_id": room_id,
            "previous_owner": current_owner_id,
            "new_owner": payload.new_owner_member_id,
        }),
    });

    record_audit_event(
        &state.db,
        &room_id,
        &current_owner_id,
        "owner",
        "owner_transferred",
        Some(serde_json::json!({
            "previous_owner": current_owner_id,
            "new_owner": payload.new_owner_member_id
        })),
        None,
    )
    .await;

    info!(
        room_id = %room_id,
        previous_owner = %current_owner_id,
        new_owner = %payload.new_owner_member_id,
        "Ownership transferred successfully"
    );

    Ok(Json(TransferOwnerResponse {
        success: true,
        room_id,
        previous_owner: current_owner_id,
        new_owner: payload.new_owner_member_id,
    }))
}

/// Retrieve privacy-preserving audit logs for a room.
pub async fn get_audit_logs(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<AuditLogEntry>>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;
    verify_room_authorization(&state, &room_id, &token).await?;

    let logs: Vec<AuditLogEntry> = sqlx::query_as(
        "SELECT id, room_id, actor_member_id, actor_role, action, details, client_ip, created_at \
         FROM room_audit_logs \
         WHERE room_id = $1 \
         ORDER BY id ASC \
         LIMIT 200",
    )
    .bind(&room_id)
    .fetch_all(&state.db)
    .await
    .map_err(db_error)?;

    Ok(Json(logs))
}

async fn verify_room_authorization(
    state: &AppState,
    room_id: &str,
    token: &str,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let member_id: Option<String> = sqlx::query_scalar(
        "SELECT member_id FROM room_members WHERE room_id = $1 AND member_token = $2 \
         UNION ALL \
         SELECT rm.member_id FROM room_members rm JOIN rooms r ON r.room_id = rm.room_id \
         WHERE r.room_id = $1 AND r.owner_token = $2 AND rm.role = 'owner' LIMIT 1",
    )
    .bind(room_id)
    .bind(token)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;

    if let Some(member_id) = member_id {
        sqlx::query(
            "UPDATE room_members SET last_seen_at = NOW() WHERE room_id = $1 AND member_id = $2",
        )
        .bind(room_id)
        .bind(member_id)
        .execute(&state.db)
        .await
        .map_err(db_error)?;
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid or missing token for this room".to_string(),
                code: "UNAUTHORIZED".to_string(),
                details: None,
            }),
        ))
    }
}

async fn authenticate_room_actor(
    state: &AppState,
    room_id: &str,
    token: &str,
) -> Result<(String, String), (StatusCode, Json<ErrorResponse>)> {
    let actor: Option<(String, String)> = sqlx::query_as(
        "SELECT member_id, role FROM room_members WHERE room_id = $1 AND member_token = $2 \
         UNION ALL \
         SELECT rm.member_id, 'owner' FROM room_members rm JOIN rooms r ON r.room_id = rm.room_id \
         WHERE r.room_id = $1 AND r.owner_token = $2 AND rm.role = 'owner' LIMIT 1",
    )
    .bind(room_id)
    .bind(token)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;

    actor.ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid or missing token for this room".to_string(),
                code: "UNAUTHORIZED".to_string(),
                details: None,
            }),
        )
    })
}

fn normalize_display_name(
    requested: Option<String>,
    fallback: &str,
) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let display_name = requested
        .unwrap_or_else(|| fallback.to_string())
        .trim()
        .to_string();
    if display_name.is_empty()
        || display_name.chars().count() > 64
        || display_name.chars().any(char::is_control)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Computer name must contain 1 to 64 non-control characters".to_string(),
                code: "INVALID_DISPLAY_NAME".to_string(),
                details: None,
            }),
        ));
    }
    Ok(display_name)
}

fn display_name_or_fallback(display_name: &str, fallback: &str) -> String {
    let display_name = display_name.trim();
    if display_name.is_empty() {
        fallback.to_string()
    } else {
        display_name.to_string()
    }
}

pub fn extract_token(headers: &HeaderMap) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
    let Some(auth_header) = headers.get("Authorization") else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Authorization header required".to_string(),
                code: "MISSING_TOKEN".to_string(),
                details: None,
            }),
        ));
    };

    let auth_str = auth_header.to_str().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Invalid characters in Authorization header".to_string(),
                code: "INVALID_HEADER".to_string(),
                details: None,
            }),
        )
    })?;

    if let Some(token) = auth_str.strip_prefix("Bearer ") {
        Ok(token.trim().to_string())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Authorization format must be 'Bearer <token>'".to_string(),
                code: "INVALID_AUTH_SCHEME".to_string(),
                details: None,
            }),
        ))
    }
}

fn db_error(e: sqlx::Error) -> (StatusCode, Json<ErrorResponse>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorResponse {
            error: "Database error encountered".to_string(),
            code: "DATABASE_ERROR".to_string(),
            details: Some(e.to_string()),
        }),
    )
}

fn extract_client_ip(headers: &HeaderMap) -> std::net::IpAddr {
    headers
        .get("x-forwarded-for")
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.trim().parse().ok())
        })
        .unwrap_or_else(|| std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}
