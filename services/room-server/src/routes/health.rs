//! Health and readiness endpoints.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub version: &'static str,
}

#[derive(Serialize)]
pub struct ReadyResponse {
    pub status: &'static str,
    pub database: &'static str,
}

/// Liveness check (Kubernetes / Docker healthcheck).
pub async fn health_check() -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// Readiness check (verifies database pool connectivity).
pub async fn readiness_check(
    State(state): State<AppState>,
) -> Result<Json<ReadyResponse>, (StatusCode, Json<ReadyResponse>)> {
    match sqlx::query("SELECT 1").execute(&state.db).await {
        Ok(_) => Ok(Json(ReadyResponse {
            status: "ready",
            database: "connected",
        })),
        Err(err) => {
            tracing::error!(error = %err, "Readiness probe failed: database ping error");
            Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ReadyResponse {
                    status: "unready",
                    database: "disconnected",
                }),
            ))
        }
    }
}
