use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::auth::{generate_high_entropy_token, hash_password, verify_password};
use crate::error::ErrorResponse;
use crate::state::{AppState, RoomEvent};

#[derive(Debug, Deserialize)]
pub struct CreateRoomRequest {
    pub room_id: String,
    pub password: String,
    pub game_build: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreateRoomResponse {
    pub room_id: String,
    pub owner_token: String,
    pub member_token: String,
    pub role: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct JoinRoomRequest {
    pub password: String,
    pub member_id: Option<String>,
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
    pub active_members: i64,
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

    if !room_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Room ID must contain only alphanumeric characters, dashes, or underscores".to_string(),
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

    // Check if room already exists
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

    // Insert room
    sqlx::query(
        "INSERT INTO rooms (room_id, revision, password_hash, owner_token, game_build) VALUES ($1, 0, $2, $3, $4)",
    )
    .bind(&room_id)
    .bind(&password_hash)
    .bind(&owner_token)
    .bind(&payload.game_build)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;

    // Register owner as initial member
    sqlx::query(
        "INSERT INTO room_members (room_id, member_id, member_token, role, last_acknowledged_revision) VALUES ($1, $2, $3, 'owner', 0)",
    )
    .bind(&room_id)
    .bind(&owner_member_id)
    .bind(&member_token)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;

    tx.commit().await.map_err(db_error)?;

    info!(room_id = %room_id, "Room created successfully with owner role");

    Ok((
        StatusCode::CREATED,
        Json(CreateRoomResponse {
            room_id,
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

    // Rate-limiting check
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

    let room: Option<(String, i64)> =
        sqlx::query_as("SELECT password_hash, revision FROM rooms WHERE room_id = $1")
            .bind(&room_id)
            .fetch_optional(&state.db)
            .await
            .map_err(db_error)?;

    let Some((password_hash, revision)) = room else {
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

    // Success: reset rate limit counter
    state.rate_limiter.reset(client_ip, &room_id).await;

    let member_token = generate_high_entropy_token();
    let member_id = payload
        .member_id
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| format!("member-{}", &member_token[..8]));

    sqlx::query(
        "INSERT INTO room_members (room_id, member_id, member_token, role, last_acknowledged_revision, last_seen_at) \
         VALUES ($1, $2, $3, 'member', 0, NOW()) \
         ON CONFLICT (room_id, member_id) DO UPDATE SET member_token = $3, last_seen_at = NOW()",
    )
    .bind(&room_id)
    .bind(&member_id)
    .bind(&member_token)
    .execute(&state.db)
    .await
    .map_err(db_error)?;

    // Broadcast presence event to room WebSocket subscribers
    let channel = state.get_or_create_room_channel(&room_id).await;
    let _ = channel.send(RoomEvent {
        room_id: room_id.clone(),
        event_type: "member_joined".to_string(),
        payload: serde_json::json!({
            "member_id": member_id,
            "role": "member",
        }),
    });

    info!(room_id = %room_id, member_id = %member_id, "Member authenticated and joined room");

    Ok(Json(JoinRoomResponse {
        room_id,
        member_id,
        member_token,
        role: "member",
        revision,
    }))
}

/// Retrieve room metadata, requiring a valid member or owner token in the Authorization header.
pub async fn get_room_info(
    State(state): State<AppState>,
    Path(room_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<RoomInfoResponse>, (StatusCode, Json<ErrorResponse>)> {
    let token = extract_token(&headers)?;

    // Check if token matches owner or any member
    let is_owner: Option<bool> =
        sqlx::query_scalar("SELECT (owner_token = $1) FROM rooms WHERE room_id = $2")
            .bind(&token)
            .bind(&room_id)
            .fetch_optional(&state.db)
            .await
            .map_err(db_error)?;

    let is_member: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM room_members WHERE room_id = $1 AND member_token = $2)",
    )
    .bind(&room_id)
    .bind(&token)
    .fetch_optional(&state.db)
    .await
    .map_err(db_error)?;

    let authorized = is_owner.unwrap_or(false) || is_member.unwrap_or(false);
    if !authorized {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorResponse {
                error: "Invalid or missing token for this room".to_string(),
                code: "UNAUTHORIZED".to_string(),
                details: None,
            }),
        ));
    }

    let room: Option<(i64, Option<String>, chrono::DateTime<chrono::Utc>)> =
        sqlx::query_as("SELECT revision, game_build, created_at FROM rooms WHERE room_id = $1")
            .bind(&room_id)
            .fetch_optional(&state.db)
            .await
            .map_err(db_error)?;

    let Some((revision, game_build, created_at)) = room else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Room '{room_id}' not found"),
                code: "ROOM_NOT_FOUND".to_string(),
                details: None,
            }),
        ));
    };

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
        active_members,
    }))
}

fn extract_token(headers: &HeaderMap) -> Result<String, (StatusCode, Json<ErrorResponse>)> {
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

