//! Desktop-owned state for room synchronization.
//!
//! This is deliberately separate from patcher, launcher, settings, and active-profile state. It
//! owns only the local SQLite state, the isolated cache, and short-lived synchronization sessions.
//! A future network service can submit a validated [`RoomManifest`] through this module without
//! gaining a route to game-affecting commands.

use fs_err as fs;
use ltk_manager_core::config::Config;
use ltk_manager_core::events::{BackendEvent, EventSink, RoomPresenceChanged, RoomPresenceState};
use ltk_manager_core::mods::ModLibrary;
use ltk_manager_core::room_sync::{
    create_or_update_room_profile, prepare_accepted_revision, CachePruneReport, ManifestLimits,
    RoomCache, RoomCacheError, RoomClientError, RoomManifest, RoomPreparationError,
    RoomPreparationResult, RoomProfileBinding, RoomProfileWorkflowError, RoomProfileWorkflowResult,
    RoomStateError, RoomStateStore, RoomSyncPhase, RoomSyncSession, RoomSyncSnapshot,
    TransferProgress, TransferProgressCallback,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

const ROOM_STATE_DIRECTORY: &str = "room-sync";
const ROOM_STATE_DATABASE: &str = "rooms.sqlite3";
const ROOM_CACHE_DIRECTORY: &str = "cache";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

type PresenceEventTimes = HashMap<(String, String), (RoomPresenceState, Instant)>;

/// Tauri-managed room state. Every clone shares the same SQLite store, cache, sessions, and
/// event throttles, so commands can safely move a clone to a blocking thread.
#[derive(Clone)]
pub struct RoomSyncState {
    store: Arc<RoomStateStore>,
    cache: RoomCache,
    sessions: Arc<Mutex<HashMap<String, RoomSyncSession>>>,
    events: RoomEventReporter,
}

impl RoomSyncState {
    /// Open durable room state below application data and keep the cache away from library content.
    pub fn open(
        app_data_dir: &Path,
        protected_library_roots: &[PathBuf],
        events: Arc<dyn EventSink>,
    ) -> Result<Self, RoomRuntimeError> {
        let root = app_data_dir.join(ROOM_STATE_DIRECTORY);
        fs::create_dir_all(&root)?;
        let cache = RoomCache::open(root.join(ROOM_CACHE_DIRECTORY), protected_library_roots)?;
        let store = RoomStateStore::open(root.join(ROOM_STATE_DATABASE))?;
        Ok(Self {
            store: Arc::new(store),
            cache,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            events: RoomEventReporter::new(events),
        })
    }

    /// Create a local draft membership. It does not create a server-side room or send a request.
    pub fn create_draft(
        &self,
        room_id: &str,
    ) -> Result<ltk_manager_core::room_sync::JoinedRoom, RoomRuntimeError> {
        self.join_draft(room_id)
    }

    /// Add a local draft membership. Server-issued identities replace this temporary ID in step 12.
    pub fn join_draft(
        &self,
        room_id: &str,
    ) -> Result<ltk_manager_core::room_sync::JoinedRoom, RoomRuntimeError> {
        let member_id = self
            .store
            .room(room_id)?
            .map(|room| room.member_id)
            .unwrap_or_else(|| format!("local-{}", Uuid::new_v4()));
        let joined = self.store.join_room(room_id, &member_id)?;
        let session = RoomSyncSession::restore(
            &self.store,
            &self.cache,
            &joined.room_id,
            ManifestLimits::default(),
        )?;
        let snapshot = session.snapshot();
        self.sessions.lock().insert(joined.room_id.clone(), session);
        self.events.emit_presence(RoomPresenceChanged {
            room_id: joined.room_id.clone(),
            member_id: joined.member_id.clone(),
            state: RoomPresenceState::Joined,
        });
        self.events.emit_sync(snapshot);
        Ok(joined)
    }

    pub fn rooms(&self) -> Result<Vec<ltk_manager_core::room_sync::JoinedRoom>, RoomRuntimeError> {
        Ok(self.store.rooms()?)
    }

    /// Leave local room state without touching an imported mod, profile, or cached blob.
    pub fn leave(&self, room_id: &str) -> Result<bool, RoomRuntimeError> {
        let joined = self.store.room(room_id)?;
        let removed = self.store.leave_room(room_id)?;
        self.sessions.lock().remove(room_id);
        if removed {
            if let Some(joined) = joined {
                self.events.emit_presence(RoomPresenceChanged {
                    room_id: joined.room_id,
                    member_id: joined.member_id,
                    state: RoomPresenceState::Left,
                });
            }
        }
        Ok(removed)
    }

    /// Restore a room's snapshot from durable state; this performs no network request.
    pub fn snapshot(&self, room_id: &str) -> Result<RoomSyncSnapshot, RoomRuntimeError> {
        let session =
            RoomSyncSession::restore(&self.store, &self.cache, room_id, ManifestLimits::default())?;
        let snapshot = session.snapshot();
        self.sessions.lock().insert(room_id.to_string(), session);
        Ok(snapshot)
    }

    /// Compare a manifest submitted by the future room transport, and accept it only if cache
    /// already contains every verified blob. This operation cannot prepare, apply, or launch mods.
    pub fn synchronize_manifest(
        &self,
        manifest: RoomManifest,
    ) -> Result<RoomSyncSnapshot, RoomRuntimeError> {
        let room_id = manifest.room_id.clone();
        let mut session = RoomSyncSession::restore(
            &self.store,
            &self.cache,
            &room_id,
            ManifestLimits::default(),
        )?;
        session.begin_connect()?;
        self.events.emit_sync(session.snapshot());
        session.connected()?;
        self.events.emit_sync(session.snapshot());
        let mut snapshot = session.compare_manifest(
            &self.store,
            &self.cache,
            manifest,
            ManifestLimits::default(),
        )?;
        self.events.emit_sync(snapshot.clone());
        if snapshot.phase == RoomSyncPhase::Verifying {
            snapshot = session.commit_verified_revision(
                &self.store,
                &self.cache,
                ManifestLimits::default(),
            )?;
            self.events.emit_sync(snapshot.clone());
        }
        self.sessions.lock().insert(room_id, session);
        Ok(snapshot)
    }

    pub fn discard_target(&self, room_id: &str) -> Result<RoomSyncSnapshot, RoomRuntimeError> {
        let mut session =
            RoomSyncSession::restore(&self.store, &self.cache, room_id, ManifestLimits::default())?;
        let snapshot = session.discard_target(&self.store)?;
        self.events.emit_sync(snapshot.clone());
        self.sessions.lock().insert(room_id.to_string(), session);
        Ok(snapshot)
    }

    pub fn accepted_manifest(
        &self,
        room_id: &str,
    ) -> Result<Option<RoomManifest>, RoomRuntimeError> {
        Ok(self.store.accepted_manifest(room_id)?)
    }

    /// Read the durable local preparation and profile facts for one room.
    ///
    /// This is deliberately a read-only view: it cannot select the profile, start the patcher,
    /// or otherwise change a game session. The renderer uses it to distinguish what has been
    /// synchronized from what the user has explicitly prepared locally.
    pub fn local_status(&self, room_id: &str) -> Result<RoomLocalStatus, RoomRuntimeError> {
        Ok(RoomLocalStatus {
            prepared_revision: self
                .store
                .prepared_revision(room_id)?
                .map(|prepared| prepared.revision),
            profile: self.store.room_profile(room_id)?,
        })
    }

    /// Report cache usage metadata without exposing cache paths to the webview.
    pub fn cache_status(&self) -> Result<RoomCacheStatus, RoomRuntimeError> {
        let rooms = self.store.rooms()?;
        let references = self.store.cache_references()?;
        let pending_transfers = rooms
            .iter()
            .map(|room| self.store.pending_transfers(&room.room_id))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|transfers| transfers.len())
            .sum();
        Ok(RoomCacheStatus {
            joined_rooms: rooms.len(),
            referenced_blobs: references.blob_count(),
            pending_transfers,
        })
    }

    /// Explicitly remove unreferenced cache blobs. Imported library copies are not cache data.
    pub fn prune_cache(&self) -> Result<CachePruneReport, RoomRuntimeError> {
        let references = self.store.cache_references()?;
        Ok(self.cache.prune_unreferenced(&references)?)
    }

    /// Import the complete accepted revision through the existing disabled preparation pipeline.
    pub fn prepare_revision(
        &self,
        library: &ModLibrary,
        config: &Config,
        room_id: &str,
    ) -> Result<RoomPreparationResult, RoomRuntimeError> {
        let prepared = prepare_accepted_revision(
            &self.store,
            &self.cache,
            library,
            config,
            room_id,
            ManifestLimits::default(),
        )?;
        library.announce_change();
        Ok(prepared)
    }

    /// Create/update a non-active profile for an already prepared revision.
    pub fn create_profile(
        &self,
        library: &ModLibrary,
        config: &Config,
        room_id: &str,
    ) -> Result<RoomProfileWorkflowResult, RoomRuntimeError> {
        let profile = create_or_update_room_profile(
            &self.store,
            library,
            config,
            room_id,
            ManifestLimits::default(),
        )?;
        library.announce_change();
        Ok(profile)
    }

    /// Callback a future HTTP/WebSocket transport supplies to the transfer engine.
    pub fn transfer_progress_callback(&self) -> TransferProgressCallback {
        let events = self.events.clone();
        Arc::new(move |progress| events.emit_transfer(progress))
    }
}

/// Cache facts suitable for IPC. File paths and room credentials never cross this boundary.
#[derive(Debug, Clone, Copy, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RoomCacheStatus {
    pub joined_rooms: usize,
    pub referenced_blobs: usize,
    pub pending_transfers: usize,
}

/// Durable local workflow facts suitable for the room UI.
///
/// It contains no cache paths, credentials, local-mod mappings, or game state. A profile binding
/// only identifies the profile the user may choose through the existing profile flow.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RoomLocalStatus {
    pub prepared_revision: Option<u64>,
    pub profile: Option<RoomProfileBinding>,
}

/// Errors retained inside the room IPC boundary. Their raw sources can contain local paths, so the
/// command layer maps them to a stable category before serializing an error to the frontend.
#[derive(Debug, Error)]
pub enum RoomRuntimeError {
    #[error(transparent)]
    State(#[from] RoomStateError),
    #[error(transparent)]
    Cache(#[from] RoomCacheError),
    #[error(transparent)]
    Client(#[from] RoomClientError),
    #[error(transparent)]
    Preparation(#[from] RoomPreparationError),
    #[error(transparent)]
    Profile(#[from] RoomProfileWorkflowError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
struct RoomEventReporter {
    events: Arc<dyn EventSink>,
    transfers: Arc<Mutex<HashMap<String, Instant>>>,
    presence: Arc<Mutex<PresenceEventTimes>>,
}

impl RoomEventReporter {
    fn new(events: Arc<dyn EventSink>) -> Self {
        Self {
            events,
            transfers: Arc::new(Mutex::new(HashMap::new())),
            presence: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn emit_sync(&self, snapshot: RoomSyncSnapshot) {
        self.events.emit(BackendEvent::RoomSyncProgress(snapshot));
    }

    fn emit_transfer(&self, progress: TransferProgress) {
        let key = format!(
            "{}:{}:{:?}",
            progress.room_id, progress.content_hash, progress.direction
        );
        let completed = progress.transferred_bytes >= progress.total_bytes;
        let now = Instant::now();
        let mut sent = self.transfers.lock();
        if !completed
            && sent
                .get(&key)
                .is_some_and(|last| now.duration_since(*last) < PROGRESS_INTERVAL)
        {
            return;
        }
        sent.insert(key, now);
        self.events
            .emit(BackendEvent::RoomTransferProgress(progress));
    }

    fn emit_presence(&self, presence: RoomPresenceChanged) {
        let key = (presence.room_id.clone(), presence.member_id.clone());
        let now = Instant::now();
        let mut sent = self.presence.lock();
        if sent.get(&key).is_some_and(|(state, last)| {
            *state == presence.state && now.duration_since(*last) < PROGRESS_INTERVAL
        }) {
            return;
        }
        sent.insert(key, (presence.state, now));
        self.events
            .emit(BackendEvent::RoomPresenceChanged(presence));
    }
}

#[cfg(test)]
mod tests;
