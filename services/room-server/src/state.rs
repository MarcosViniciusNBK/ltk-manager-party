//! Shared server application state.

use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex, MutexGuard, RwLock};

use crate::rate_limit::RateLimiter;
use crate::storage::StorageManager;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub rooms: Arc<RwLock<HashMap<String, broadcast::Sender<RoomEvent>>>>,
    pub active_connections: Arc<RwLock<HashMap<String, HashSet<String>>>>,
    pub rate_limiter: RateLimiter,
    pub storage: StorageManager,
    pub updates_dir: PathBuf,
    pub public_server_url: String,
    upload_locks: Arc<Vec<Mutex<()>>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoomEvent {
    pub room_id: String,
    pub event_type: String,
    pub payload: serde_json::Value,
}

impl AppState {
    pub fn new(
        db: PgPool,
        storage: StorageManager,
        updates_dir: PathBuf,
        public_server_url: String,
    ) -> Self {
        Self {
            db,
            rooms: Arc::new(RwLock::new(HashMap::new())),
            active_connections: Arc::new(RwLock::new(HashMap::new())),
            rate_limiter: RateLimiter::default(),
            storage,
            updates_dir,
            public_server_url,
            upload_locks: Arc::new((0..256).map(|_| Mutex::new(())).collect()),
        }
    }

    /// Serialize writes for the same content hash using a bounded set of lock stripes.
    pub async fn lock_upload(&self, content_hash: &str) -> MutexGuard<'_, ()> {
        let stripe = content_hash
            .get(..2)
            .and_then(|prefix| usize::from_str_radix(prefix, 16).ok())
            .unwrap_or_default();
        self.upload_locks[stripe].lock().await
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

    pub async fn add_connection(&self, room_id: &str, member_id: &str) {
        let mut conns = self.active_connections.write().await;
        conns
            .entry(room_id.to_string())
            .or_default()
            .insert(member_id.to_string());
    }

    pub async fn remove_connection(&self, room_id: &str, member_id: &str) -> bool {
        let mut conns = self.active_connections.write().await;
        if let Some(members) = conns.get_mut(room_id) {
            members.remove(member_id);
            members.is_empty()
        } else {
            false
        }
    }

    pub async fn is_member_connected(&self, room_id: &str, member_id: &str) -> bool {
        let conns = self.active_connections.read().await;
        conns
            .get(room_id)
            .map(|members| members.contains(member_id))
            .unwrap_or(false)
    }
}
