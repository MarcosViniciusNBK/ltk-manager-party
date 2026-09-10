//! Typed IPC for the room-synchronization boundary.
//!
//! These commands only coordinate room state, cache files, disabled preparation, and an explicit
//! non-active profile. They deliberately do not call patcher, launcher, settings, or profile-switch
//! operations. Network clients added later submit manifests here; a renderer never supplies a URL,
//! access token, or command that can affect a running game.

use crate::error::{AppErrorResponse, IpcResult, RoomSyncErrorKind};
use crate::mods::ModLibraryState;
use crate::rooms::{RoomCacheStatus, RoomLocalStatus, RoomRuntimeError, RoomSyncState};
use crate::state::SettingsState;
use ltk_manager_core::room_sync::{
    CachePruneReport, JoinedRoom, RoomManifest, RoomPreparationResult, RoomProfileWorkflowResult,
    RoomSyncSnapshot,
};
use tauri::{AppHandle, Manager};
use thiserror::Error;

/// Summary of an explicit local preparation. Local UUIDs remain internal to the library and are
/// available through its existing APIs; no game-changing action has happened at this point.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RoomPreparationSummary {
    pub room_id: String,
    pub revision: u64,
    pub imported_count: usize,
    pub reused_count: usize,
}

impl From<RoomPreparationResult> for RoomPreparationSummary {
    fn from(value: RoomPreparationResult) -> Self {
        Self {
            room_id: value.room_id,
            revision: value.revision,
            imported_count: value.imported_count,
            reused_count: value.reused_count,
        }
    }
}

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

/// Create local draft state for a room code. This intentionally does not create a remote room;
/// server-side creation, password handling, and owner tokens arrive with the authoritative service.
#[tauri::command]
#[specta::specta]
pub async fn create_room_draft(room_id: String, app_handle: AppHandle) -> IpcResult<JoinedRoom> {
    let rooms = rooms(&app_handle);
    room_task(move || rooms.create_draft(&room_id)).await
}

/// Join local draft state for a room code. It has no network side effect and does not accept a
/// password or token, so neither can accidentally be logged or stored in renderer state.
#[tauri::command]
#[specta::specta]
pub async fn join_room_draft(room_id: String, app_handle: AppHandle) -> IpcResult<JoinedRoom> {
    let rooms = rooms(&app_handle);
    room_task(move || rooms.join_draft(&room_id)).await
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
    room_task(move || rooms.leave(&room_id)).await
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
    room_task(move || rooms.synchronize_manifest(manifest)).await
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
    room_task(move || rooms.discard_target(&room_id)).await
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

/// Explicitly import the accepted revision through the existing archive pipeline, registered
/// disabled. This command never selects a profile or starts the patcher.
#[tauri::command]
#[specta::specta]
pub async fn prepare_room_revision(
    room_id: String,
    app_handle: AppHandle,
) -> IpcResult<RoomPreparationSummary> {
    let rooms = rooms(&app_handle);
    let library = app_handle.state::<ModLibraryState>().0.clone();
    let config = app_handle.state::<SettingsState>().config();
    room_task(move || {
        rooms
            .prepare_revision(&library, &config, &room_id)
            .map(RoomPreparationSummary::from)
    })
    .await
}

/// Explicitly create/update the non-active profile corresponding to an already prepared revision.
/// The existing profile switch and Start/Play commands remain separate user actions.
#[tauri::command]
#[specta::specta]
pub async fn create_room_profile(
    room_id: String,
    app_handle: AppHandle,
) -> IpcResult<RoomProfileSummary> {
    let rooms = rooms(&app_handle);
    let library = app_handle.state::<ModLibraryState>().0.clone();
    let config = app_handle.state::<SettingsState>().config();
    room_task(move || {
        rooms
            .create_profile(&library, &config, &room_id)
            .map(RoomProfileSummary::from)
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
    #[error("room operation was interrupted")]
    Interrupted,
}

impl From<RoomCommandError> for AppErrorResponse {
    fn from(error: RoomCommandError) -> Self {
        let kind = match &error {
            RoomCommandError::Runtime(RoomRuntimeError::State(_)) => RoomSyncErrorKind::State,
            RoomCommandError::Runtime(RoomRuntimeError::Cache(_)) => RoomSyncErrorKind::Cache,
            RoomCommandError::Runtime(RoomRuntimeError::Client(_)) => {
                RoomSyncErrorKind::Synchronization
            }
            RoomCommandError::Runtime(RoomRuntimeError::Preparation(_)) => {
                RoomSyncErrorKind::Preparation
            }
            RoomCommandError::Runtime(RoomRuntimeError::Profile(_)) => RoomSyncErrorKind::Profile,
            RoomCommandError::Runtime(RoomRuntimeError::Io(_)) => RoomSyncErrorKind::State,
            RoomCommandError::Interrupted => RoomSyncErrorKind::Interrupted,
        };
        // Some lower-level room errors name local paths. The log is application-local; the IPC
        // payload intentionally contains only `kind` so a webview never receives them.
        tracing::warn!(error = %error, ?kind, "Room synchronization command failed");
        AppErrorResponse::RoomSync { kind }
    }
}
