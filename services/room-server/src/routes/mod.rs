pub mod health;
pub mod rooms;
pub mod version;
pub mod ws;

use axum::routing::{get, post};
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
        .route("/v1/rooms/{room_id}/ws", get(ws::ws_handler))
        .with_state(state)
}
