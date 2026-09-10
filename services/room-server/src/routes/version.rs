//! Version information endpoint.

use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub struct VersionInfo {
    pub name: &'static str,
    pub version: &'static str,
    pub protocol_version: u32,
}

pub async fn get_version() -> impl IntoResponse {
    Json(VersionInfo {
        name: "ltk-room-server",
        version: env!("CARGO_PKG_VERSION"),
        protocol_version: 1,
    })
}
