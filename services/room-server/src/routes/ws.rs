//! Authenticated WebSocket events for presence, acknowledgements, and room synchronization.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::state::{AppState, RoomEvent};

#[derive(Debug, Deserialize)]
pub struct WsAuthQuery {
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
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

    // Identify member and role from token
    let member_info: Option<(String, String)> = sqlx::query_as(
        "SELECT member_id, role FROM room_members WHERE room_id = $1 AND member_token = $2 \
         UNION \
         SELECT rm.member_id, 'owner' as role FROM room_members rm JOIN rooms r ON rm.room_id = r.room_id \
         WHERE r.room_id = $1 AND r.owner_token = $2 LIMIT 1",
    )
    .bind(&room_id)
    .bind(&token)
    .fetch_optional(&state.db)
    .await
    .unwrap_or(None);

    let Some((member_id, role)) = member_info else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    Ok(ws.on_upgrade(move |socket| handle_socket(socket, room_id, member_id, role, state)))
}

async fn handle_socket(
    socket: WebSocket,
    room_id: String,
    member_id: String,
    role: String,
    state: AppState,
) {
    let (mut sender, mut receiver) = socket.split();
    let broadcaster = state.get_or_create_room_channel(&room_id).await;
    let mut rx = broadcaster.subscribe();

    // Register active presence
    state.add_connection(&room_id, &member_id).await;
    let _ = sqlx::query(
        "UPDATE room_members SET last_seen_at = NOW() WHERE room_id = $1 AND member_id = $2",
    )
    .bind(&room_id)
    .bind(&member_id)
    .execute(&state.db)
    .await;

    info!(room_id = %room_id, member_id = %member_id, role = %role, "WebSocket client connected");

    // Broadcast member_presence (joined)
    let _ = broadcaster.send(RoomEvent {
        room_id: room_id.clone(),
        event_type: "member_presence".to_string(),
        payload: serde_json::json!({
            "member_id": member_id,
            "role": role,
            "state": "joined",
        }),
    });

    // Task 1: Broadcast events to client
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

    // Task 2: Receive messages from client
    let room_id_recv = room_id.clone();
    let member_id_recv = member_id.clone();
    let state_recv = state.clone();
    let broadcaster_recv = broadcaster.clone();

    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    debug!(room_id = %room_id_recv, member_id = %member_id_recv, "Received WS message: {}", text);
                    if let Ok(incoming) = serde_json::from_str::<WsIncoming>(&text) {
                        match incoming.action.as_str() {
                            "ping" => {
                                let _ = sqlx::query("UPDATE room_members SET last_seen_at = NOW() WHERE room_id = $1 AND member_id = $2")
                                    .bind(&room_id_recv)
                                    .bind(&member_id_recv)
                                    .execute(&state_recv.db)
                                    .await;
                            }
                            "ack" => {
                                if let Some(payload) = incoming.payload {
                                    if let Some(rev) =
                                        payload.get("revision").and_then(|r| r.as_i64())
                                    {
                                        let status = payload
                                            .get("status")
                                            .and_then(|s| s.as_str())
                                            .unwrap_or("synchronized");

                                        let _ = sqlx::query(
                                            "UPDATE room_members SET last_acknowledged_revision = $1, ack_status = $2, last_seen_at = NOW() \
                                             WHERE room_id = $3 AND member_id = $4",
                                        )
                                        .bind(rev)
                                        .bind(status)
                                        .bind(&room_id_recv)
                                        .bind(&member_id_recv)
                                        .execute(&state_recv.db)
                                        .await;

                                        let _ = broadcaster_recv.send(RoomEvent {
                                            room_id: room_id_recv.clone(),
                                            event_type: "member_acknowledged".to_string(),
                                            payload: serde_json::json!({
                                                "room_id": room_id_recv,
                                                "member_id": member_id_recv,
                                                "revision": rev,
                                                "status": status,
                                            }),
                                        });
                                    }
                                }
                            }
                            other => {
                                warn!(action = %other, "Unknown WebSocket action requested");
                            }
                        }
                    }
                }
                Message::Ping(_) => {}
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = (&mut send_task) => recv_task.abort(),
        _ = (&mut recv_task) => send_task.abort(),
    }

    // Cleanup presence on disconnect
    state.remove_connection(&room_id, &member_id).await;

    // Broadcast member_presence (left)
    let _ = broadcaster.send(RoomEvent {
        room_id: room_id.clone(),
        event_type: "member_presence".to_string(),
        payload: serde_json::json!({
            "member_id": member_id,
            "role": role,
            "state": "left",
        }),
    });

    // If the disconnecting client was the owner, broadcast owner_disconnected alert
    if role == "owner" {
        warn!(room_id = %room_id, member_id = %member_id, "Room owner disconnected");
        let _ = broadcaster.send(RoomEvent {
            room_id: room_id.clone(),
            event_type: "owner_disconnected".to_string(),
            payload: serde_json::json!({
                "room_id": room_id,
                "member_id": member_id,
                "warning": "Room owner disconnected from the session",
            }),
        });
    }

    info!(room_id = %room_id, member_id = %member_id, "WebSocket client disconnected");
}
