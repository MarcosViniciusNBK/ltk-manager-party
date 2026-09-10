//! Authenticated WebSocket events for presence and room synchronization.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct WsAuthQuery {
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WsIncoming {
    pub action: String,
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct WsOutgoing {
    pub event: String,
    pub data: serde_json::Value,
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(room_id): Path<String>,
    Query(query): Query<WsAuthQuery>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Result<impl IntoResponse, StatusCode> {
    let token = query.token.or_else(|| {
        headers
            .get("Authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(|t| t.trim().to_string())
    });

    let Some(token) = token else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let is_owner: Option<bool> =
        sqlx::query_scalar("SELECT (owner_token = $1) FROM rooms WHERE room_id = $2")
            .bind(&token)
            .bind(&room_id)
            .fetch_optional(&state.db)
            .await
            .unwrap_or(None);

    let is_member: Option<bool> = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM room_members WHERE room_id = $1 AND member_token = $2)",
    )
    .bind(&room_id)
    .bind(&token)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    let authorized = is_owner.unwrap_or(false) || is_member.unwrap_or(false);
    if !authorized {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(ws.on_upgrade(move |socket| handle_socket(socket, room_id, state)))
}

async fn handle_socket(socket: WebSocket, room_id: String, state: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let broadcaster = state.get_or_create_room_channel(&room_id).await;
    let mut rx = broadcaster.subscribe();

    info!(room_id = %room_id, "WebSocket client connected to room");

    // Spawn broadcast receiver task -> send to client
    let room_id_clone = room_id.clone();
    let mut send_task = tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            let msg = serde_json::to_string(&WsOutgoing {
                event: event.event_type,
                data: event.payload,
            })
            .unwrap_or_default();

            if sender.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });

    // Receive task from client
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    debug!(room_id = %room_id_clone, message = %text, "Received WS message");
                    if let Ok(incoming) = serde_json::from_str::<WsIncoming>(&text) {
                        if incoming.action == "ping" {
                            // Client ping - can be echoed or answered
                        }
                    }
                }
                Message::Ping(_) => {}
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // If either task completes, abort the other
    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    }

    info!(room_id = %room_id, "WebSocket client disconnected from room");
}
