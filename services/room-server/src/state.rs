//! Shared server application state.

use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use crate::rate_limit::RateLimiter;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub rooms: Arc<RwLock<HashMap<String, broadcast::Sender<RoomEvent>>>>,
    pub rate_limiter: RateLimiter,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoomEvent {
    pub room_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
}

impl AppState {
    pub fn new(db: PgPool) -> Self {
        Self {
            db,
            rooms: Arc::new(RwLock::new(HashMap::new())),
            rate_limiter: RateLimiter::default(),
        }
    }

    pub async fn get_or_create_room_channel(&self, room_id: &str) -> broadcast::Sender<RoomEvent> {
        let mut rooms = self.rooms.write().await;
        if let Some(sender) = rooms.get(room_id) {
            sender.clone()
        } else {
            let (sender, _) = broadcast::channel(128);
            rooms.insert(room_id.to_string(), sender.clone());
            sender
        }
    }
}
