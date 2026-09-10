pub mod blobs;
pub mod health;
pub mod rooms;
pub mod version;
pub mod ws;

use axum::routing::{get, head, post};
use axum::Router;

use crate::state::AppState;

pub fn create_router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health::health_check))
        .route("/ready", get(health::readiness_check))
        .route("/v1/version", get(version::get_version))
        .route("/v1/rooms", post(rooms::create_room))
        .route("/v1/rooms/{room_id}", get(rooms::get_room_info))
        .route("/v1/rooms/{room_id}/join", post(rooms::join_room))
        .route("/v1/rooms/{room_id}/manifest", post(rooms::publish_manifest).get(rooms::get_latest_manifest))
        .route("/v1/rooms/{room_id}/manifests/{revision}", get(rooms::get_revision_manifest))
        .route("/v1/rooms/{room_id}/ack", post(rooms::ack_revision))
        .route("/v1/rooms/{room_id}/members", get(rooms::get_room_members))
        .route("/v1/rooms/{room_id}/transfer_owner", post(rooms::transfer_ownership))
        .route("/v1/rooms/{room_id}/audit", get(rooms::get_audit_logs))
        .route("/v1/rooms/{room_id}/ws", get(ws::ws_handler))
        // Etapa 15: Content-Addressed Storage Endpoints
        .route("/v1/rooms/{room_id}/blobs/check", post(blobs::check_blobs))
        .route("/v1/rooms/{room_id}/blobs/upload_url", post(blobs::request_upload_url))
        .route("/v1/rooms/{room_id}/blobs/{content_hash}/download_url", get(blobs::request_download_url))
        .route("/v1/blobs/upload/{content_hash}", head(blobs::probe_upload_blob).put(blobs::put_upload_blob))
        .route("/v1/blobs/download/{content_hash}", head(blobs::probe_download_blob).get(blobs::get_download_blob))
        .with_state(state)
}
