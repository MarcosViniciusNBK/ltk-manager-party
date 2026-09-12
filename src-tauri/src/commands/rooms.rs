//! Typed IPC for the room-synchronization boundary.
//!
//! These commands only coordinate room state, cache files, disabled preparation, and an explicit
//! non-active profile. They deliberately do not call patcher, launcher, settings, or profile-switch
//! operations. Network clients added later submit manifests here; a renderer never supplies a URL,
//! access token, or command that can affect a running game.

use crate::error::{AppErrorResponse, IpcResult, RoomSyncErrorKind, RoomSyncErrorReason};
use crate::mods::ModLibraryState;
use crate::rooms::{
    RemoteMemberInfo, RoomCacheStatus, RoomLocalStatus, RoomRuntimeError, RoomSyncState,
};
use crate::state::SettingsState;
use ltk_manager_core::room_sync::{
    CachePruneReport, JoinedRoom, RoomManifest, RoomProfileWorkflowResult, RoomSyncSnapshot,
};
use tauri::{AppHandle, Manager};
use thiserror::Error;

/// Result of creating/updating the dedicated room profile. The profile is not selected; the user
/// must use the existing profile chooser and Start/Play flow themselves.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RoomProfileSummary {
    pub room_id: String,
    pub revision: u64,
    pub profile_id: String,
}

impl From<RoomProfileWorkflowResult> for RoomProfileSummary {
    fn from(value: RoomProfileWorkflowResult) -> Self {
        Self {
            room_id: value.room_id,
            revision: value.revision,
            profile_id: value.profile.id,
        }
    }
}

/// Create a room, publish the selected source profile, and materialize its dedicated shared
/// profile locally. The shared profile is not selected or applied.
#[tauri::command]
#[specta::specta]
pub async fn create_remote_room(
    room_id: String,
    password: String,
    profile_id: String,
    app_handle: AppHandle,
) -> IpcResult<JoinedRoom> {
    let room_id = room_id.trim().to_lowercase();
    let rooms = rooms(&app_handle);
    let library = app_handle.state::<ModLibraryState>().0.clone();
    let config = app_handle.state::<SettingsState>().config();
    room_task(move || {
        let _operation = rooms.lock_operation();
        if !rooms.rooms()?.is_empty() {
            return Err(RoomCommandError::AlreadyInRoom);
        }
        let joined = rooms.create_remote_room(&room_id, &password)?;
        rooms.publish_profile_to_remote_room(&room_id, Some(&profile_id), &library, &config)?;
        Ok::<_, RoomCommandError>(joined)
    })
    .await
}

/// Join an existing room. The real-time background worker then downloads, prepares, and
/// creates/updates its dedicated local profile while the room UI can display transfer progress.
/// It remains unapplied until the user uses the existing Start/Play flow.
#[tauri::command]
#[specta::specta]
pub async fn join_remote_room(
    room_id: String,
    password: String,
    app_handle: AppHandle,
) -> IpcResult<JoinedRoom> {
    let room_id = room_id.trim().to_lowercase();
    let rooms = rooms(&app_handle);
    room_task(move || {
        let _operation = rooms.lock_operation();
        if !rooms.rooms()?.is_empty() {
            return Err(RoomCommandError::AlreadyInRoom);
        }
        rooms
            .join_remote_room(&room_id, &password)
            .map_err(Into::into)
    })
    .await
}

/// Retrieve active members and synchronization state from the server.
#[tauri::command]
#[specta::specta]
pub async fn get_remote_room_members(
    room_id: String,
    app_handle: AppHandle,
) -> IpcResult<Vec<RemoteMemberInfo>> {
    let rooms = rooms(&app_handle);
    room_task(move || rooms.remote_room_members(&room_id)).await
}

/// Create local draft state for a room code. This intentionally does not create a remote room;
/// server-side creation, password handling, and owner tokens arrive with the authoritative service.
#[tauri::command]
#[specta::specta]
pub async fn create_room_draft(room_id: String, app_handle: AppHandle) -> IpcResult<JoinedRoom> {
    let rooms = rooms(&app_handle);
    room_task(move || {
        let _operation = rooms.lock_operation();
        if !rooms.rooms()?.is_empty() {
            return Err(RoomCommandError::AlreadyInRoom);
        }
        rooms.create_draft(&room_id).map_err(Into::into)
    })
    .await
}

/// Join local draft state for a room code. It has no network side effect and does not accept a
/// password or token, so neither can accidentally be logged or stored in renderer state.
#[tauri::command]
#[specta::specta]
pub async fn join_room_draft(room_id: String, app_handle: AppHandle) -> IpcResult<JoinedRoom> {
    let rooms = rooms(&app_handle);
    room_task(move || {
        let _operation = rooms.lock_operation();
        if !rooms.rooms()?.is_empty() {
            return Err(RoomCommandError::AlreadyInRoom);
        }
        rooms.join_draft(&room_id).map_err(Into::into)
    })
    .await
}

/// List local memberships and their last accepted revision.
#[tauri::command]
#[specta::specta]
pub async fn list_room_memberships(app_handle: AppHandle) -> IpcResult<Vec<JoinedRoom>> {
    let rooms = rooms(&app_handle);
    room_task(move || rooms.rooms()).await
}

/// Leave local room state. Imported library mods and profiles remain local; cache cleanup requires
/// the separate explicit prune command.
#[tauri::command]
#[specta::specta]
pub async fn leave_room(room_id: String, app_handle: AppHandle) -> IpcResult<bool> {
    let rooms = rooms(&app_handle);
    room_task(move || {
        let Some(_operation) = rooms.try_lock_operation() else {
            return Err(RoomCommandError::OperationInProgress);
        };
        rooms.leave(&room_id).map_err(Into::into)
    })
    .await
}

/// Restore the durable snapshot for one room without making a network request.
#[tauri::command]
#[specta::specta]
pub async fn get_room_sync_snapshot(
    room_id: String,
    app_handle: AppHandle,
) -> IpcResult<RoomSyncSnapshot> {
    let rooms = rooms(&app_handle);
    room_task(move || rooms.snapshot(&room_id)).await
}

/// Submit one manifest from the isolated room transport.
///
/// It stages, verifies, and atomically accepts only already-cached blobs. Missing blobs remain a
/// pending room transfer; no mod is installed, enabled, applied, or launched.
#[tauri::command]
#[specta::specta]
pub async fn synchronize_room_manifest(
    manifest: RoomManifest,
    app_handle: AppHandle,
) -> IpcResult<RoomSyncSnapshot> {
    let rooms = rooms(&app_handle);
    room_task(move || {
        let _operation = rooms.lock_operation();
        rooms.synchronize_manifest(manifest)
    })
    .await
}

/// Discard an incomplete target revision while retaining the last accepted revision and its cache
/// references.
#[tauri::command]
#[specta::specta]
pub async fn discard_room_target(
    room_id: String,
    app_handle: AppHandle,
) -> IpcResult<RoomSyncSnapshot> {
    let rooms = rooms(&app_handle);
    room_task(move || {
        let _operation = rooms.lock_operation();
        rooms.discard_target(&room_id)
    })
    .await
}

/// Read the complete accepted room manifest, if this machine has one.
#[tauri::command]
#[specta::specta]
pub async fn get_accepted_room_manifest(
    room_id: String,
    app_handle: AppHandle,
) -> IpcResult<Option<RoomManifest>> {
    let rooms = rooms(&app_handle);
    room_task(move || rooms.accepted_manifest(&room_id)).await
}

/// Read durable local workflow facts for a room. This is informational only: a profile binding
/// does not select the profile, and neither preparation nor profile creation applies anything to
/// the game.
#[tauri::command]
#[specta::specta]
pub async fn get_room_local_status(
    room_id: String,
    app_handle: AppHandle,
) -> IpcResult<RoomLocalStatus> {
    let rooms = rooms(&app_handle);
    room_task(move || rooms.local_status(&room_id)).await
}

/// Report cache reference and pending-transfer counts without revealing local paths.
#[tauri::command]
#[specta::specta]
pub async fn get_room_cache_status(app_handle: AppHandle) -> IpcResult<RoomCacheStatus> {
    let rooms = rooms(&app_handle);
    room_task(move || rooms.cache_status()).await
}

/// Explicitly prune only unreferenced room-cache blobs. It cannot delete an installed library mod.
#[tauri::command]
#[specta::specta]
pub async fn prune_room_cache(app_handle: AppHandle) -> IpcResult<CachePruneReport> {
    let rooms = rooms(&app_handle);
    room_task(move || rooms.prune_cache()).await
}

/// Publish a local profile to the collaborative room as an authenticated member.
#[tauri::command]
#[specta::specta]
pub async fn publish_room_profile(
    room_id: String,
    profile_id: Option<String>,
    app_handle: AppHandle,
) -> IpcResult<RoomSyncSnapshot> {
    let rooms = rooms(&app_handle);
    let library = app_handle.state::<ModLibraryState>().0.clone();
    let config = app_handle.state::<SettingsState>().config();
    room_task(move || {
        let Some(_operation) = rooms.try_lock_operation() else {
            return Err(RoomCommandError::OperationInProgress);
        };
        rooms
            .publish_profile_to_remote_room(&room_id, profile_id.as_deref(), &library, &config)
            .map_err(Into::into)
    })
    .await
}

/// Sync the room's manifest, prepare it in the library, and create or update this member's
/// non-active room profile in one call. Never selects or activates that profile.
#[tauri::command]
#[specta::specta]
pub async fn sync_room_profile(
    room_id: String,
    app_handle: AppHandle,
) -> IpcResult<RoomProfileSummary> {
    let rooms = rooms(&app_handle);
    let library = app_handle.state::<ModLibraryState>().0.clone();
    let config = app_handle.state::<SettingsState>().config();
    room_task(move || {
        let Some(_operation) = rooms.try_lock_operation() else {
            return Err(RoomCommandError::OperationInProgress);
        };
        rooms
            .sync_room_profile(&room_id, &library, &config)
            .map(RoomProfileSummary::from)
            .map_err(Into::into)
    })
    .await
}

fn rooms(app_handle: &AppHandle) -> RoomSyncState {
    app_handle.state::<RoomSyncState>().inner().clone()
}

async fn room_task<T, F, E>(work: F) -> IpcResult<T>
where
    T: Send + 'static,
    E: Into<RoomCommandError> + Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    match tauri::async_runtime::spawn_blocking(work).await {
        Ok(result) => result.map_err(Into::into).into(),
        Err(_) => IpcResult::err(RoomCommandError::Interrupted),
    }
}

#[derive(Debug, Error)]
enum RoomCommandError {
    #[error(transparent)]
    Runtime(#[from] RoomRuntimeError),
    #[error("the user is already connected to a room")]
    AlreadyInRoom,
    #[error("another room operation is already in progress")]
    OperationInProgress,
    #[error("room operation was interrupted")]
    Interrupted,
}

impl From<RoomCommandError> for AppErrorResponse {
    fn from(error: RoomCommandError) -> Self {
        let (kind, reason) = match &error {
            RoomCommandError::Runtime(RoomRuntimeError::State(_)) => {
                (RoomSyncErrorKind::State, RoomSyncErrorReason::LocalState)
            }
            RoomCommandError::Runtime(RoomRuntimeError::Cache(_)) => {
                (RoomSyncErrorKind::Cache, RoomSyncErrorReason::LocalCache)
            }
            RoomCommandError::Runtime(RoomRuntimeError::Client(_)) => (
                RoomSyncErrorKind::Synchronization,
                RoomSyncErrorReason::Synchronization,
            ),
            RoomCommandError::Runtime(RoomRuntimeError::Preparation(_)) => (
                RoomSyncErrorKind::Preparation,
                RoomSyncErrorReason::LocalPreparation,
            ),
            RoomCommandError::Runtime(RoomRuntimeError::Profile(_)) => (
                RoomSyncErrorKind::Profile,
                RoomSyncErrorReason::LocalProfile,
            ),
            RoomCommandError::Runtime(RoomRuntimeError::Credential(_)) => (
                RoomSyncErrorKind::State,
                RoomSyncErrorReason::CredentialStore,
            ),
            RoomCommandError::Runtime(RoomRuntimeError::Io(_)) => {
                (RoomSyncErrorKind::State, RoomSyncErrorReason::LocalFile)
            }
            RoomCommandError::Runtime(RoomRuntimeError::Network(detail)) => (
                RoomSyncErrorKind::Synchronization,
                classify_network_error(detail),
            ),
            RoomCommandError::AlreadyInRoom => {
                (RoomSyncErrorKind::State, RoomSyncErrorReason::AlreadyInRoom)
            }
            RoomCommandError::OperationInProgress => (
                RoomSyncErrorKind::Synchronization,
                RoomSyncErrorReason::OperationInProgress,
            ),
            RoomCommandError::Interrupted => (
                RoomSyncErrorKind::Interrupted,
                RoomSyncErrorReason::Interrupted,
            ),
        };
        // Some lower-level room errors name local paths. The log is application-local; the IPC
        // payload intentionally contains only `kind` so a webview never receives them.
        tracing::warn!(error = %error, ?kind, "Room synchronization command failed");
        AppErrorResponse::RoomSync { kind, reason }
    }
}

fn classify_network_error(detail: &str) -> RoomSyncErrorReason {
    let lowercase = detail.to_ascii_lowercase();
    if lowercase.contains("timed out") || lowercase.contains("timeout") {
        return RoomSyncErrorReason::RequestTimedOut;
    }
    if lowercase.contains("error sending request")
        || lowercase.contains("connection refused")
        || lowercase.contains("failed to connect")
        || lowercase.contains("dns")
    {
        return RoomSyncErrorReason::ServerUnavailable;
    }
    if lowercase.contains("missing ")
        || lowercase.contains("error decoding response")
        || lowercase.contains("invalid room server url")
    {
        return RoomSyncErrorReason::InvalidServerResponse;
    }
    if lowercase.contains("collecting profile mods") {
        return RoomSyncErrorReason::LocalFile;
    }

    let server_code = detail
        .find('{')
        .and_then(|start| serde_json::from_str::<serde_json::Value>(&detail[start..]).ok())
        .and_then(|payload| {
            payload
                .get("code")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });

    match server_code.as_deref() {
        Some("ROOM_NOT_FOUND") => RoomSyncErrorReason::RoomNotFound,
        Some("ROOM_EXPIRED") => RoomSyncErrorReason::RoomExpired,
        Some("INVALID_PASSWORD") => RoomSyncErrorReason::InvalidPassword,
        Some("RATE_LIMITED") => RoomSyncErrorReason::RateLimited,
        Some("ROOM_EXISTS") => RoomSyncErrorReason::RoomAlreadyExists,
        Some("INVALID_ROOM_ID") => RoomSyncErrorReason::InvalidRoomId,
        Some("PASSWORD_TOO_SHORT") => RoomSyncErrorReason::PasswordTooShort,
        Some("UNAUTHORIZED" | "MISSING_TOKEN" | "INVALID_AUTH_SCHEME" | "INVALID_HEADER") => {
            RoomSyncErrorReason::SessionExpired
        }
        Some("REVISION_CONFLICT") => RoomSyncErrorReason::RevisionConflict,
        Some("QUOTA_EXCEEDED") => RoomSyncErrorReason::StorageQuotaExceeded,
        Some(
            "BLOB_NOT_FOUND" | "BLOB_NOT_IN_ROOM" | "NO_MANIFEST" | "MANIFEST_REVISION_NOT_FOUND",
        ) => RoomSyncErrorReason::SharedFileUnavailable,
        Some(
            "INTEGRITY_MISMATCH"
            | "BLOB_METADATA_MISMATCH"
            | "INVALID_CONTENT_HASH"
            | "INVALID_FORMAT"
            | "INVALID_SIZE"
            | "INVALID_MANIFEST",
        ) => RoomSyncErrorReason::IntegrityCheckFailed,
        Some(
            "DATABASE_ERROR"
            | "HASHING_ERROR"
            | "IO_ERROR"
            | "STORAGE_IO_ERROR"
            | "SERIALIZATION_ERROR",
        ) => RoomSyncErrorReason::ServerError,
        Some(_) => RoomSyncErrorReason::ServerError,
        None => RoomSyncErrorReason::Synchronization,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_server_error_codes() {
        assert_eq!(
            classify_network_error(
                r#"Server error (401 Unauthorized): {"code":"INVALID_PASSWORD"}"#,
            ),
            RoomSyncErrorReason::InvalidPassword
        );
        assert_eq!(
            classify_network_error(r#"Server error (404): {"code":"ROOM_NOT_FOUND"}"#),
            RoomSyncErrorReason::RoomNotFound
        );
    }

    #[test]
    fn classifies_transport_and_protocol_errors() {
        assert_eq!(
            classify_network_error("request timed out"),
            RoomSyncErrorReason::RequestTimedOut
        );
        assert_eq!(
            classify_network_error("error sending request for url"),
            RoomSyncErrorReason::ServerUnavailable
        );
        assert_eq!(
            classify_network_error("Missing member_token"),
            RoomSyncErrorReason::InvalidServerResponse
        );
    }
}
