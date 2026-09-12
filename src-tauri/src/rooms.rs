//! Desktop-owned state for room synchronization.
//!
//! This is deliberately separate from patcher, launcher, settings, and active-profile state. It
//! owns only the local SQLite state, the isolated cache, and short-lived synchronization sessions.
//! A future network service can submit a validated [`RoomManifest`] through this module without
//! gaining a route to game-affecting commands.

use fs_err as fs;
use ltk_manager_core::config::Config;
use ltk_manager_core::events::{
    BackendEvent, EventSink, RoomActivity, RoomActivityStage, RoomPresenceChanged,
    RoomPresenceState, RoomPublishProgress, RoomPublishStage,
};
use ltk_manager_core::mods::ModLibrary;
use ltk_manager_core::room_sync::{
    create_or_update_room_profile, prepare_accepted_revision, CachePruneReport, ContentHash,
    ManifestLimits, RoomCache, RoomCacheError, RoomClientError, RoomManifest, RoomPreparationError,
    RoomPreparationResult, RoomProfileBinding, RoomProfileWorkflowError, RoomProfileWorkflowResult,
    RoomStateError, RoomStateStore, RoomSyncPhase, RoomSyncSession, RoomSyncSnapshot,
    TransferDirection, TransferProgress, TransferProgressCallback,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

const ROOM_STATE_DIRECTORY: &str = "room-sync";
const ROOM_STATE_DATABASE: &str = "rooms.sqlite3";
const ROOM_CACHE_DIRECTORY: &str = "cache";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const API_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TRANSFER_REQUEST_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const REALTIME_READ_TIMEOUT: Duration = Duration::from_millis(250);
const FALLBACK_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

type PresenceEventTimes = HashMap<(String, String), (RoomPresenceState, Instant)>;

/// Tauri-managed room state. Every clone shares the same SQLite store, cache, sessions, and
/// event throttles, so commands can safely move a clone to a blocking thread.
#[derive(Clone)]
pub struct RoomSyncState {
    store: Arc<RoomStateStore>,
    cache: RoomCache,
    sessions: Arc<Mutex<HashMap<String, RoomSyncSession>>>,
    profile_signatures: Arc<Mutex<HashMap<String, u64>>>,
    operation_lock: Arc<Mutex<()>>,
    library_change_generation: Arc<AtomicU64>,
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
            profile_signatures: Arc::new(Mutex::new(HashMap::new())),
            operation_lock: Arc::new(Mutex::new(())),
            library_change_generation: Arc::new(AtomicU64::new(0)),
            events: RoomEventReporter::new(events),
        })
    }

    /// Wake signal used by the core event adapter after any library mutation. The real-time room
    /// worker observes this within one WebSocket read timeout (currently 250 ms).
    pub fn notify_library_changed(&self) {
        self.library_change_generation
            .fetch_add(1, Ordering::Release);
    }

    /// Serialize network/profile mutations with the background reconciler. The guard is acquired
    /// inside blocking workers, so it never stalls Tauri's async executor or the renderer thread.
    pub fn lock_operation(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.operation_lock.lock()
    }

    pub fn try_lock_operation(&self) -> Option<parking_lot::MutexGuard<'_, ()>> {
        self.operation_lock.try_lock()
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
        self.profile_signatures.lock().remove(room_id);
        if removed {
            #[cfg(windows)]
            {
                let vault = ltk_manager_core::room_sync::RoomCredentialVault::default();
                vault.delete(
                    room_id,
                    ltk_manager_core::room_sync::RoomSecretKind::MemberToken,
                )?;
                vault.delete(
                    room_id,
                    ltk_manager_core::room_sync::RoomSecretKind::OwnerToken,
                )?;
            }
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
        // Expose the staged target while it is downloading so the renderer can show one
        // progress row per mod. Once verified, the accepted manifest remains the source of truth.
        Ok(self
            .store
            .staged_manifest(room_id)?
            .or(self.store.accepted_manifest(room_id)?))
    }

    /// Read the durable local preparation and profile facts for one room.
    ///
    /// This is deliberately a read-only view: it cannot select the profile, start the patcher,
    /// or otherwise change a game session. The renderer uses it to distinguish what has been
    /// synchronized from what the user has explicitly prepared locally.
    pub fn local_status(&self, room_id: &str) -> Result<RoomLocalStatus, RoomRuntimeError> {
        let manifest = self
            .store
            .staged_manifest(room_id)?
            .or(self.store.accepted_manifest(room_id)?);
        let mut cached_content_hashes = Vec::new();
        if let Some(manifest) = manifest {
            for room_mod in manifest.mods {
                let artifact = ltk_manager_core::room_sync::CanonicalRoomArtifact {
                    content_hash: room_mod.content_hash.clone(),
                    size_bytes: room_mod.size_bytes,
                    format: room_mod.format,
                };
                if self.cache.contains(&artifact)? {
                    cached_content_hashes.push(room_mod.content_hash.as_str().to_string());
                }
            }
        }
        Ok(RoomLocalStatus {
            prepared_revision: self
                .store
                .prepared_revision(room_id)?
                .map(|prepared| prepared.revision),
            profile: self.store.room_profile(room_id)?,
            cached_content_hashes,
            activity: self.events.activity(room_id),
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

    /// Create a new online room on the authoritative room server.
    pub fn create_remote_room(
        &self,
        room_id: &str,
        password: &str,
    ) -> Result<ltk_manager_core::room_sync::JoinedRoom, RoomRuntimeError> {
        let base_url = room_server_url();
        let url = format!("{base_url}/v1/rooms");
        let payload = serde_json::json!({
            "room_id": room_id,
            "password": password,
        });

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        let res = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload.to_string())
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().unwrap_or_default();
            return Err(RoomRuntimeError::Network(format!(
                "Server error ({status}): {text}"
            )));
        }

        let body: serde_json::Value = res
            .json()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        let owner_token = body
            .get("owner_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RoomRuntimeError::Network("Missing owner_token".to_string()))?;
        let member_token = body
            .get("member_token")
            .and_then(|v| v.as_str())
            .unwrap_or(owner_token);

        #[cfg(windows)]
        {
            let vault = ltk_manager_core::room_sync::RoomCredentialVault::default();
            vault.write(
                room_id,
                ltk_manager_core::room_sync::RoomSecretKind::OwnerToken,
                owner_token.as_bytes(),
            )?;
            vault.write(
                room_id,
                ltk_manager_core::room_sync::RoomSecretKind::MemberToken,
                member_token.as_bytes(),
            )?;
        }

        let owner_member_id = body
            .get("member_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RoomRuntimeError::Network("Missing member_id".to_string()))?;
        let joined = self.store.join_room(room_id, &owner_member_id)?;
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

    /// Join an existing online room on the authoritative room server.
    pub fn join_remote_room(
        &self,
        room_id: &str,
        password: &str,
    ) -> Result<ltk_manager_core::room_sync::JoinedRoom, RoomRuntimeError> {
        let base_url = room_server_url();
        let url = format!("{base_url}/v1/rooms/{room_id}/join");
        let payload = serde_json::json!({
            "password": password,
        });

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        let res = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(payload.to_string())
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().unwrap_or_default();
            return Err(RoomRuntimeError::Network(format!(
                "Server error ({status}): {text}"
            )));
        }

        let body: serde_json::Value = res
            .json()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        let member_id = body
            .get("member_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RoomRuntimeError::Network("Missing member_id".to_string()))?;
        let member_token = body
            .get("member_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RoomRuntimeError::Network("Missing member_token".to_string()))?;

        #[cfg(windows)]
        {
            let vault = ltk_manager_core::room_sync::RoomCredentialVault::default();
            vault.write(
                room_id,
                ltk_manager_core::room_sync::RoomSecretKind::MemberToken,
                member_token.as_bytes(),
            )?;
        }

        let joined = self.store.join_room(room_id, member_id)?;
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

    /// Retrieve active members and synchronization state from the server.
    pub fn remote_room_members(
        &self,
        room_id: &str,
    ) -> Result<Vec<RemoteMemberInfo>, RoomRuntimeError> {
        let base_url = room_server_url();
        let url = format!("{base_url}/v1/rooms/{room_id}/members");
        let token = self.get_room_token(room_id)?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        let res = client
            .get(&url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().unwrap_or_default();
            return Err(RoomRuntimeError::Network(format!(
                "Server error ({status}): {text}"
            )));
        }

        let members: Vec<RemoteMemberInfoWire> = res
            .json()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        Ok(members.into_iter().map(RemoteMemberInfo::from).collect())
    }

    /// Synchronize manifest and missing blobs from the authoritative server.
    pub fn sync_remote_room(&self, room_id: &str) -> Result<RoomSyncSnapshot, RoomRuntimeError> {
        let base_url = room_server_url();
        let manifest_url = format!("{base_url}/v1/rooms/{room_id}/manifest");
        let token = self.get_room_token(room_id)?;

        let client = reqwest::blocking::Client::builder()
            .timeout(API_REQUEST_TIMEOUT)
            .build()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;
        let transfer_client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(TRANSFER_REQUEST_TIMEOUT)
            .build()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        let res = client
            .get(&manifest_url)
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return self.snapshot(room_id);
        }

        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().unwrap_or_default();
            return Err(RoomRuntimeError::Network(format!(
                "Failed to fetch manifest ({status}): {text}"
            )));
        }

        let manifest: RoomManifest = res
            .json()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        // 1. Stage the manifest and check missing blobs
        self.synchronize_manifest(manifest.clone())?;

        // 2. Download any missing blobs
        for room_mod in &manifest.mods {
            let artifact = ltk_manager_core::room_sync::CanonicalRoomArtifact {
                content_hash: room_mod.content_hash.clone(),
                size_bytes: room_mod.size_bytes,
                format: room_mod.format,
            };

            if self.cache.contains(&artifact)? {
                self.events.emit_transfer(TransferProgress {
                    room_id: room_id.to_string(),
                    content_hash: room_mod.content_hash.clone(),
                    display_name: Some(room_mod.display_name.clone()),
                    direction: TransferDirection::Download,
                    transferred_bytes: room_mod.size_bytes,
                    total_bytes: room_mod.size_bytes,
                    attempt: 1,
                });
                continue;
            }

            let dl_url_endpoint = format!(
                "{base_url}/v1/rooms/{room_id}/blobs/{}/download_url",
                room_mod.content_hash.as_str()
            );
            let dl_res = client
                .get(&dl_url_endpoint)
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

            if !dl_res.status().is_success() {
                return Err(RoomRuntimeError::Network(format!(
                    "Failed download URL for {}: {}",
                    room_mod.content_hash.as_str(),
                    dl_res.status()
                )));
            }

            let dl_info: serde_json::Value = dl_res
                .json()
                .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;
            let signed_download_url = dl_info
                .get("download_url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RoomRuntimeError::Network("Missing download_url".to_string()))?;

            let partial_path = self.cache.prepare_partial(&room_mod.content_hash)?;
            let mut offset = fs::metadata(&partial_path).map_or(0, |metadata| metadata.len());
            if offset > room_mod.size_bytes {
                fs::File::create(&partial_path)?;
                offset = 0;
            }
            if offset == room_mod.size_bytes {
                self.cache.commit_partial(&artifact)?;
                self.events.emit_transfer(TransferProgress {
                    room_id: room_id.to_string(),
                    content_hash: room_mod.content_hash.clone(),
                    display_name: Some(room_mod.display_name.clone()),
                    direction: TransferDirection::Download,
                    transferred_bytes: room_mod.size_bytes,
                    total_bytes: room_mod.size_bytes,
                    attempt: 1,
                });
                continue;
            }

            let mut request = transfer_client.get(signed_download_url);
            if offset > 0 {
                request = request.header("Range", format!("bytes={offset}-")).header(
                    "If-Range",
                    format!("\"{}\"", room_mod.content_hash.as_str()),
                );
            }
            let mut blob_resp = request
                .send()
                .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;
            if !blob_resp.status().is_success() {
                return Err(RoomRuntimeError::Network(format!(
                    "Failed downloading blob {}: {}",
                    room_mod.content_hash.as_str(),
                    blob_resp.status()
                )));
            }
            if offset > 0 && blob_resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                fs::File::create(&partial_path)?;
                offset = 0;
            }
            let mut file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&partial_path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut transferred = offset;
            self.events.emit_transfer(TransferProgress {
                room_id: room_id.to_string(),
                content_hash: room_mod.content_hash.clone(),
                display_name: Some(room_mod.display_name.clone()),
                direction: TransferDirection::Download,
                transferred_bytes: transferred,
                total_bytes: room_mod.size_bytes,
                attempt: 1,
            });
            let mut buffer = [0_u8; 256 * 1024];
            loop {
                let read = blob_resp.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                file.write_all(&buffer[..read])?;
                transferred = transferred.saturating_add(read as u64);
                self.events.emit_transfer(TransferProgress {
                    room_id: room_id.to_string(),
                    content_hash: room_mod.content_hash.clone(),
                    display_name: Some(room_mod.display_name.clone()),
                    direction: TransferDirection::Download,
                    transferred_bytes: transferred.min(room_mod.size_bytes),
                    total_bytes: room_mod.size_bytes,
                    attempt: 1,
                });
            }
            file.sync_data()?;
            drop(file);

            self.cache.commit_partial(&artifact)?;
        }

        // 3. Atomically accept manifest now that all blobs are cached
        let snapshot = self.synchronize_manifest(manifest.clone())?;

        // 4. Send Ack to server
        let ack_url = format!("{base_url}/v1/rooms/{room_id}/ack");
        let ack_payload = serde_json::json!({
            "revision": manifest.revision,
            "status": "synchronized"
        });
        let _ = client
            .post(&ack_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body(ack_payload.to_string())
            .send();

        Ok(snapshot)
    }

    pub fn publish_profile_to_remote_room(
        &self,
        room_id: &str,
        profile_id: Option<&str>,
        library: &ltk_manager_core::mods::ModLibrary,
        config: &ltk_manager_core::config::Config,
    ) -> Result<RoomSyncSnapshot, RoomRuntimeError> {
        self.events.emit_publish(RoomPublishProgress {
            room_id: room_id.to_string(),
            stage: RoomPublishStage::Preparing,
            completed_mods: 0,
            total_mods: 0,
        });
        let result =
            self.publish_profile_to_remote_room_inner(room_id, profile_id, library, config);
        self.events.emit_publish(RoomPublishProgress {
            room_id: room_id.to_string(),
            stage: if result.is_ok() {
                RoomPublishStage::Complete
            } else {
                RoomPublishStage::Failed
            },
            completed_mods: 0,
            total_mods: 0,
        });
        result
    }

    fn publish_profile_to_remote_room_inner(
        &self,
        room_id: &str,
        profile_id: Option<&str>,
        library: &ltk_manager_core::mods::ModLibrary,
        config: &ltk_manager_core::config::Config,
    ) -> Result<RoomSyncSnapshot, RoomRuntimeError> {
        let room_token = self.get_room_token(room_id)?;
        let (_profile, artifacts) = library
            .collect_profile_room_artifacts(config, profile_id)
            .map_err(|e| {
                RoomRuntimeError::Network(format!("Failed collecting profile mods: {e}"))
            })?;

        let base_url = room_server_url();
        let client = reqwest::blocking::Client::builder()
            .timeout(API_REQUEST_TIMEOUT)
            .build()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;
        let transfer_client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(TRANSFER_REQUEST_TIMEOUT)
            .build()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        // 1. Fetch current room info to find current_revision
        let info_url = format!("{base_url}/v1/rooms/{room_id}");
        let info_res = client
            .get(&info_url)
            .header("Authorization", format!("Bearer {room_token}"))
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        if !info_res.status().is_success() {
            let status = info_res.status();
            let body = info_res.text().unwrap_or_default();
            return Err(RoomRuntimeError::Network(format!(
                "Failed to inspect room before publishing ({status}): {body}"
            )));
        }
        let info_body: serde_json::Value = info_res.json().map_err(|error| {
            RoomRuntimeError::Network(format!("Server returned invalid room information: {error}"))
        })?;
        let current_revision = info_body["revision"].as_i64().ok_or_else(|| {
            RoomRuntimeError::Network(
                "Server room information did not contain a revision".to_string(),
            )
        })?;

        let next_revision = (current_revision + 1) as u64;
        let room_mods: Vec<ltk_manager_core::room_sync::RoomMod> =
            artifacts.iter().map(|(_, m)| m.clone()).collect();

        let manifest = ltk_manager_core::room_sync::RoomManifest {
            schema_version: 1,
            room_id: room_id.to_string(),
            revision: next_revision,
            game_build: None,
            mods: room_mods,
        };

        // 2. Check which blobs the server is missing
        let check_url = format!("{base_url}/v1/rooms/{room_id}/blobs/check");
        let hashes: Vec<String> = artifacts
            .iter()
            .map(|(_, m)| m.content_hash.as_str().to_string())
            .collect();

        let check_res = client
            .post(&check_url)
            .header("Authorization", format!("Bearer {room_token}"))
            .json(&serde_json::json!({ "hashes": hashes }))
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        if !check_res.status().is_success() {
            let status = check_res.status();
            let body = check_res.text().unwrap_or_default();
            return Err(RoomRuntimeError::Network(format!(
                "Failed to check room files ({status}): {body}"
            )));
        }
        let check_body: serde_json::Value = check_res.json().map_err(|error| {
            RoomRuntimeError::Network(format!("Server returned an invalid file check: {error}"))
        })?;
        let missing_hashes: Vec<String> = check_body["missing_hashes"]
            .as_array()
            .ok_or_else(|| {
                RoomRuntimeError::Network(
                    "Server file check did not contain missing_hashes".to_string(),
                )
            })?
            .iter()
            .filter_map(|value| value.as_str().map(String::from))
            .collect();

        self.events.emit_publish(RoomPublishProgress {
            room_id: room_id.to_string(),
            stage: RoomPublishStage::Uploading,
            completed_mods: 0,
            total_mods: missing_hashes.len(),
        });

        // 3. Upload missing blobs
        for (upload_index, hash) in missing_hashes.iter().enumerate() {
            let Some((path, mod_info)) = artifacts
                .iter()
                .find(|(_, m)| m.content_hash.as_str() == hash)
            else {
                continue;
            };

            let upload_url_endpoint = format!("{base_url}/v1/rooms/{room_id}/blobs/upload_url");
            let grant_res = client
                .post(&upload_url_endpoint)
                .header("Authorization", format!("Bearer {room_token}"))
                .json(&serde_json::json!({
                    "content_hash": hash,
                    "size_bytes": mod_info.size_bytes,
                    "format": mod_info.format,
                }))
                .send()
                .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

            if !grant_res.status().is_success() {
                let err_text = grant_res.text().unwrap_or_default();
                return Err(RoomRuntimeError::Network(format!(
                    "Failed to request upload URL for blob {hash}: {err_text}"
                )));
            }

            let grant_body: serde_json::Value = grant_res
                .json()
                .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;
            let upload_url = grant_body["upload_url"].as_str().ok_or_else(|| {
                RoomRuntimeError::Network("Server did not return upload_url".to_string())
            })?;

            let probe = transfer_client
                .head(upload_url)
                .send()
                .map_err(|error| RoomRuntimeError::Network(error.to_string()))?;
            if !probe.status().is_success() {
                return Err(RoomRuntimeError::Network(format!(
                    "Failed to inspect upload offset for blob {hash}: {}",
                    probe.status()
                )));
            }
            let offset = probe
                .headers()
                .get("Upload-Offset")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            if offset > mod_info.size_bytes {
                return Err(RoomRuntimeError::Network(format!(
                    "Server returned an invalid upload offset for blob {hash}"
                )));
            }
            if offset == mod_info.size_bytes {
                self.events.emit_transfer(TransferProgress {
                    room_id: room_id.to_string(),
                    content_hash: mod_info.content_hash.clone(),
                    display_name: Some(mod_info.display_name.clone()),
                    direction: TransferDirection::Upload,
                    transferred_bytes: mod_info.size_bytes,
                    total_bytes: mod_info.size_bytes,
                    attempt: 1,
                });
                self.events.emit_publish(RoomPublishProgress {
                    room_id: room_id.to_string(),
                    stage: RoomPublishStage::Uploading,
                    completed_mods: upload_index + 1,
                    total_mods: missing_hashes.len(),
                });
                continue;
            }

            let mut file = fs::File::open(path)?;
            file.seek(SeekFrom::Start(offset))?;
            let remaining = mod_info.size_bytes - offset;
            self.events.emit_transfer(TransferProgress {
                room_id: room_id.to_string(),
                content_hash: mod_info.content_hash.clone(),
                display_name: Some(mod_info.display_name.clone()),
                direction: TransferDirection::Upload,
                transferred_bytes: offset,
                total_bytes: mod_info.size_bytes,
                attempt: 1,
            });
            let reader = UploadProgressReader {
                file,
                room_id: room_id.to_string(),
                content_hash: mod_info.content_hash.clone(),
                display_name: mod_info.display_name.clone(),
                transferred: offset,
                total: mod_info.size_bytes,
                events: self.events.clone(),
            };
            let put_res = transfer_client
                .put(upload_url)
                .header("X-Content-SHA256", hash)
                .header("Content-Type", "application/octet-stream")
                .header("Content-Length", remaining)
                .header(
                    "Content-Range",
                    format!(
                        "bytes {offset}-{}/{size}",
                        mod_info.size_bytes - 1,
                        size = mod_info.size_bytes
                    ),
                )
                .body(reqwest::blocking::Body::sized(reader, remaining))
                .send()
                .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

            if !put_res.status().is_success() {
                let err_text = put_res.text().unwrap_or_default();
                return Err(RoomRuntimeError::Network(format!(
                    "Failed to upload blob {hash}: {err_text}"
                )));
            }
            let receipt = put_res
                .headers()
                .get("X-Content-SHA256")
                .and_then(|value| value.to_str().ok());
            if receipt != Some(hash.as_str()) {
                return Err(RoomRuntimeError::Network(format!(
                    "Upload integrity receipt did not match blob {hash}"
                )));
            }
            self.events.emit_publish(RoomPublishProgress {
                room_id: room_id.to_string(),
                stage: RoomPublishStage::Uploading,
                completed_mods: upload_index + 1,
                total_mods: missing_hashes.len(),
            });
        }

        // 4. Publish the manifest revision
        self.events.emit_publish(RoomPublishProgress {
            room_id: room_id.to_string(),
            stage: RoomPublishStage::Publishing,
            completed_mods: missing_hashes.len(),
            total_mods: missing_hashes.len(),
        });
        let publish_url = format!("{base_url}/v1/rooms/{room_id}/manifest");
        let pub_res = client
            .post(&publish_url)
            .header("Authorization", format!("Bearer {room_token}"))
            .json(&serde_json::json!({
                "previous_revision": current_revision,
                "manifest": manifest,
            }))
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        if !pub_res.status().is_success() {
            let err_text = pub_res.text().unwrap_or_default();
            return Err(RoomRuntimeError::Network(format!(
                "Failed to publish manifest: {err_text}"
            )));
        }

        // 5. Commit local files into host's cache so host is immediately synchronized
        self.events.emit_publish(RoomPublishProgress {
            room_id: room_id.to_string(),
            stage: RoomPublishStage::Finalizing,
            completed_mods: missing_hashes.len(),
            total_mods: missing_hashes.len(),
        });
        for (path, mod_info) in &artifacts {
            let canonical = ltk_manager_core::room_sync::CanonicalRoomArtifact {
                content_hash: mod_info.content_hash.clone(),
                size_bytes: mod_info.size_bytes,
                format: mod_info.format,
            };
            if !self.cache.contains(&canonical)? {
                let partial = self.cache.prepare_partial(&canonical.content_hash)?;
                fs::copy(path, &partial)?;
                self.cache.commit_partial(&canonical)?;
            }
        }

        self.synchronize_manifest(manifest)?;
        self.prepare_revision(library, config, room_id)?;
        self.create_profile(library, config, room_id)?;
        self.snapshot(room_id)
    }

    /// Sync the room's manifest, prepare it in the library, and create or update this member's
    /// non-active room profile in one call. Never selects or activates that profile.
    pub fn sync_room_profile(
        &self,
        room_id: &str,
        library: &ltk_manager_core::mods::ModLibrary,
        config: &ltk_manager_core::config::Config,
    ) -> Result<RoomProfileWorkflowResult, RoomRuntimeError> {
        self.events
            .emit_activity(room_id, RoomActivityStage::Downloading);
        let result = (|| {
            self.sync_remote_room(room_id)?;
            self.events
                .emit_activity(room_id, RoomActivityStage::UpdatingProfile);
            self.prepare_revision(library, config, room_id)?;
            self.create_profile(library, config, room_id)
        })();
        self.events.emit_activity(
            room_id,
            if result.is_ok() {
                RoomActivityStage::Complete
            } else {
                RoomActivityStage::Failed
            },
        );
        result
    }

    /// Keep every joined room converged in the background. A local edit to the dedicated room
    /// profile is published by any authenticated member; a newer server revision is downloaded,
    /// prepared, and materialized locally. This never selects the profile or invokes the patcher.
    pub fn maintain_in_background(&self, library: ModLibrary, config: Config) {
        let rooms = self.clone();
        std::thread::Builder::new()
            .name("room-profile-sync".to_string())
            .spawn(move || rooms.run_realtime_sync(library, config))
            .expect("room synchronization worker must start");
    }

    fn run_realtime_sync(&self, library: ModLibrary, config: Config) {
        loop {
            if let Err(error) = self.reconcile_all_rooms(&library, &config) {
                tracing::warn!(%error, "Room background synchronization pass failed");
            }

            let room_id = match self.store.rooms() {
                Ok(rooms) => rooms.first().map(|room| room.room_id.clone()),
                Err(error) => {
                    tracing::warn!(%error, "Could not inspect room membership for real-time sync");
                    None
                }
            };
            let Some(room_id) = room_id else {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };

            if let Err(error) = self.listen_for_room_events(&room_id, &library, &config) {
                tracing::warn!(room_id = %room_id, %error, "Room real-time connection ended");
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn listen_for_room_events(
        &self,
        room_id: &str,
        library: &ModLibrary,
        config: &Config,
    ) -> Result<(), RoomRuntimeError> {
        use tungstenite::client::IntoClientRequest;

        let token = self.get_room_token(room_id)?;
        let base_url = room_server_url();
        let ws_base = base_url
            .strip_prefix("https://")
            .map(|value| format!("wss://{value}"))
            .or_else(|| {
                base_url
                    .strip_prefix("http://")
                    .map(|value| format!("ws://{value}"))
            })
            .ok_or_else(|| RoomRuntimeError::Network("Invalid room server URL".to_string()))?;
        let endpoint = format!("{}/v1/rooms/{room_id}/ws", ws_base.trim_end_matches('/'));
        let mut request = endpoint
            .into_client_request()
            .map_err(|error| RoomRuntimeError::Network(error.to_string()))?;
        let authorization = tungstenite::http::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| RoomRuntimeError::Network("Invalid room credentials".to_string()))?;
        request.headers_mut().insert("Authorization", authorization);

        let (mut socket, _) = tungstenite::connect(request)
            .map_err(|error| RoomRuntimeError::Network(error.to_string()))?;
        set_websocket_timeouts(socket.get_ref(), REALTIME_READ_TIMEOUT)?;
        tracing::info!(room_id = %room_id, "Room real-time connection established");

        let mut last_fallback = Instant::now();
        let mut last_ping = Instant::now();
        let mut observed_library_generation =
            self.library_change_generation.load(Ordering::Acquire);
        loop {
            match socket.read() {
                Ok(tungstenite::Message::Text(text)) => {
                    let event = room_event_name(&text);
                    if event.as_deref() == Some("manifest_published") {
                        self.reconcile_all_rooms(library, config)?;
                        last_fallback = Instant::now();
                    }
                    // Any room event can change the member list. Emitting a local snapshot makes
                    // the renderer invalidate its read-only room queries immediately.
                    self.events.emit_sync(self.snapshot(room_id)?);
                }
                Ok(tungstenite::Message::Ping(payload)) => {
                    socket
                        .send(tungstenite::Message::Pong(payload))
                        .map_err(|error| RoomRuntimeError::Network(error.to_string()))?;
                }
                Ok(tungstenite::Message::Close(_)) => return Ok(()),
                Ok(_) => {}
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(())
                }
                Err(error) => return Err(RoomRuntimeError::Network(error.to_string())),
            }

            if self.store.room(room_id)?.is_none() {
                let _ = socket.close(None);
                return Ok(());
            }
            if last_ping.elapsed() >= Duration::from_secs(20) {
                socket
                    .send(tungstenite::Message::Text(r#"{"action":"ping"}"#.into()))
                    .map_err(|error| RoomRuntimeError::Network(error.to_string()))?;
                last_ping = Instant::now();
            }
            let library_generation = self.library_change_generation.load(Ordering::Acquire);
            if library_generation != observed_library_generation {
                self.reconcile_all_rooms(library, config)?;
                observed_library_generation = library_generation;
                last_fallback = Instant::now();
            } else if last_fallback.elapsed() >= FALLBACK_RECONCILE_INTERVAL {
                self.reconcile_all_rooms(library, config)?;
                last_fallback = Instant::now();
            }
        }
    }

    pub fn reconcile_all_rooms(
        &self,
        library: &ModLibrary,
        config: &Config,
    ) -> Result<(), RoomRuntimeError> {
        for room in self.store.rooms()? {
            if let Err(error) = self.reconcile_room(&room.room_id, library, config) {
                tracing::warn!(room_id = %room.room_id, %error, "Could not reconcile room");
            }
        }
        Ok(())
    }

    fn reconcile_room(
        &self,
        room_id: &str,
        library: &ModLibrary,
        config: &Config,
    ) -> Result<(), RoomRuntimeError> {
        self.events
            .emit_activity(room_id, RoomActivityStage::Checking);
        let requires_operation = match self.room_requires_operation(room_id, library, config) {
            Ok(requires_operation) => requires_operation,
            Err(error) => {
                self.events.finish_check(room_id, RoomActivityStage::Failed);
                return Err(error);
            }
        };
        if !requires_operation {
            self.events
                .finish_check(room_id, RoomActivityStage::Complete);
            return Ok(());
        }

        // Periodic/read-only checks never own the operation lock. Once actual work is discovered,
        // the background worker yields to an already running user operation instead of causing a
        // misleading OperationInProgress response.
        let Some(_operation) = self.try_lock_operation() else {
            self.events.finish_check(room_id, RoomActivityStage::Idle);
            return Ok(());
        };
        let result = self.reconcile_room_locked(room_id, library, config);
        // From this point the operation lock gives this reconciliation exclusive ownership of
        // the visible activity. Publish its final outcome even when an intermediate upload or
        // download stage replaced `Checking`.
        self.events.emit_activity(
            room_id,
            if result.is_ok() {
                RoomActivityStage::Complete
            } else {
                RoomActivityStage::Failed
            },
        );
        result
    }

    fn room_requires_operation(
        &self,
        room_id: &str,
        library: &ModLibrary,
        config: &Config,
    ) -> Result<bool, RoomRuntimeError> {
        if let Some(binding) = self.store.room_profile(room_id)? {
            let profile = library
                .get_profiles(config)
                .map_err(|error| RoomRuntimeError::Network(error.to_string()))?
                .into_iter()
                .find(|profile| profile.id == binding.local_profile_id);
            if let Some(profile) = profile {
                let signature = profile_signature(&profile);
                let previous = self.profile_signatures.lock().get(room_id).copied();
                if previous != Some(signature) {
                    let (_, artifacts) = library
                        .collect_profile_room_artifacts(config, Some(&profile.id))
                        .map_err(|error| RoomRuntimeError::Network(error.to_string()))?;
                    let candidate: Vec<_> =
                        artifacts.iter().map(|(_, item)| item.clone()).collect();
                    let changed = self
                        .store
                        .accepted_manifest(room_id)?
                        .as_ref()
                        .is_none_or(|manifest| manifest.mods != candidate);
                    if changed {
                        return Ok(true);
                    }
                    self.profile_signatures
                        .lock()
                        .insert(room_id.to_string(), signature);
                }
            }
        }

        let remote_revision = self.remote_revision(room_id)?;
        let local_revision = self
            .store
            .accepted_manifest(room_id)?
            .map_or(0, |manifest| manifest.revision);
        if remote_revision > local_revision {
            return Ok(true);
        }
        if local_revision == 0 {
            return Ok(false);
        }
        let prepared_revision = self
            .store
            .prepared_revision(room_id)?
            .map(|prepared| prepared.revision);
        let profile_revision = self
            .store
            .room_profile(room_id)?
            .map(|profile| profile.revision);
        Ok(prepared_revision != Some(local_revision) || profile_revision != Some(local_revision))
    }

    fn reconcile_room_locked(
        &self,
        room_id: &str,
        library: &ModLibrary,
        config: &Config,
    ) -> Result<(), RoomRuntimeError> {
        // Publish local collaborative-profile changes first. This makes concurrent edits
        // deterministic: the last successful CAS revision wins and every client then converges.
        if let Some(binding) = self.store.room_profile(room_id)? {
            let profile = library
                .get_profiles(config)
                .map_err(|error| RoomRuntimeError::Network(error.to_string()))?
                .into_iter()
                .find(|profile| profile.id == binding.local_profile_id);
            if let Some(profile) = profile {
                let signature = profile_signature(&profile);
                let previous = self.profile_signatures.lock().get(room_id).copied();
                if previous != Some(signature) {
                    let (_, artifacts) = library
                        .collect_profile_room_artifacts(config, Some(&profile.id))
                        .map_err(|error| RoomRuntimeError::Network(error.to_string()))?;
                    let candidate: Vec<_> =
                        artifacts.iter().map(|(_, item)| item.clone()).collect();
                    let accepted = self.store.accepted_manifest(room_id)?;
                    if accepted
                        .as_ref()
                        .is_none_or(|manifest| manifest.mods != candidate)
                    {
                        self.publish_profile_to_remote_room(
                            room_id,
                            Some(&profile.id),
                            library,
                            config,
                        )?;
                    }
                    self.profile_signatures
                        .lock()
                        .insert(room_id.to_string(), signature);
                }
            }
        }

        let remote_revision = self.remote_revision(room_id)?;
        let local_revision = self
            .store
            .accepted_manifest(room_id)?
            .map_or(0, |manifest| manifest.revision);
        if remote_revision > local_revision {
            let result = self.sync_room_profile(room_id, library, config)?;
            let signature = profile_signature(&result.profile);
            self.profile_signatures
                .lock()
                .insert(room_id.to_string(), signature);
        } else if local_revision > 0 {
            // Joining again can restore an already accepted/cached revision without its local
            // preparation binding. Finish that work automatically even though the server does not
            // have a numerically newer manifest.
            let prepared_revision = self
                .store
                .prepared_revision(room_id)?
                .map(|prepared| prepared.revision);
            if prepared_revision != Some(local_revision) {
                self.prepare_revision(library, config, room_id)?;
            }
            let profile_revision = self
                .store
                .room_profile(room_id)?
                .map(|profile| profile.revision);
            if profile_revision != Some(local_revision) {
                let result = self.create_profile(library, config, room_id)?;
                self.profile_signatures
                    .lock()
                    .insert(room_id.to_string(), profile_signature(&result.profile));
            }
        }
        Ok(())
    }

    fn remote_revision(&self, room_id: &str) -> Result<u64, RoomRuntimeError> {
        let url = format!("{}/v1/rooms/{room_id}", room_server_url());
        let token = self.get_room_token(room_id)?;
        let response = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| RoomRuntimeError::Network(error.to_string()))?
            .get(url)
            .bearer_auth(token)
            .send()
            .map_err(|error| RoomRuntimeError::Network(error.to_string()))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().unwrap_or_default();
            return Err(RoomRuntimeError::Network(format!(
                "Failed to inspect room revision ({status}): {body}"
            )));
        }
        let body: serde_json::Value = response
            .json()
            .map_err(|error| RoomRuntimeError::Network(error.to_string()))?;
        body.get("revision")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| RoomRuntimeError::Network("Missing room revision".to_string()))
    }

    fn get_room_token(&self, room_id: &str) -> Result<String, RoomRuntimeError> {
        #[cfg(windows)]
        {
            let vault = ltk_manager_core::room_sync::RoomCredentialVault::default();
            if let Some(secret) = vault.read(
                room_id,
                ltk_manager_core::room_sync::RoomSecretKind::MemberToken,
            )? {
                if let Ok(s) = std::str::from_utf8(secret.as_bytes()) {
                    return Ok(s.to_string());
                }
            }
            if let Some(secret) = vault.read(
                room_id,
                ltk_manager_core::room_sync::RoomSecretKind::OwnerToken,
            )? {
                if let Ok(s) = std::str::from_utf8(secret.as_bytes()) {
                    return Ok(s.to_string());
                }
            }
        }
        Err(RoomRuntimeError::Network(
            "No authentication token found for room".to_string(),
        ))
    }

    /// Progress adapter retained for the reusable transfer engine and its event contract tests.
    #[allow(dead_code)]
    pub fn transfer_progress_callback(&self) -> TransferProgressCallback {
        let events = self.events.clone();
        Arc::new(move |progress| events.emit_transfer(progress))
    }
}

pub fn room_server_url() -> String {
    std::env::var("LTK_ROOM_SERVER_URL")
        .unwrap_or_else(|_| "https://mag.horuzprod.com/ltk-rooms".to_string())
}

fn set_websocket_timeouts(
    stream: &tungstenite::stream::MaybeTlsStream<TcpStream>,
    timeout: Duration,
) -> Result<(), RoomRuntimeError> {
    let tcp = match stream {
        tungstenite::stream::MaybeTlsStream::Plain(stream) => stream,
        tungstenite::stream::MaybeTlsStream::Rustls(stream) => stream.get_ref(),
        _ => {
            return Err(RoomRuntimeError::Network(
                "Unsupported room real-time transport".to_string(),
            ));
        }
    };
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(Duration::from_secs(10)))?;
    Ok(())
}

fn room_event_name(message: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(message)
        .ok()?
        .get("event")?
        .as_str()
        .map(str::to_owned)
}

fn profile_signature(profile: &ltk_manager_core::mods::Profile) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    profile.id.hash(&mut hasher);
    profile.mod_order.hash(&mut hasher);
    profile.enabled_mods.hash(&mut hasher);
    let mut mods: Vec<_> = profile.layer_states.iter().collect();
    mods.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
    for (mod_id, layers) in mods {
        mod_id.hash(&mut hasher);
        let mut layers: Vec<_> = layers.iter().collect();
        layers.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        for (layer, enabled) in layers {
            layer.hash(&mut hasher);
            enabled.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Remote member presence information from the room server.
#[derive(Debug, Clone, serde::Serialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RemoteMemberInfo {
    pub member_id: String,
    pub role: String,
    pub last_acknowledged_revision: i64,
    pub ack_status: String,
    pub is_online: bool,
    pub is_stale: bool,
}

#[derive(Debug, serde::Deserialize)]
struct RemoteMemberInfoWire {
    member_id: String,
    role: String,
    last_acknowledged_revision: i64,
    ack_status: String,
    is_online: bool,
    is_stale: bool,
}

impl From<RemoteMemberInfoWire> for RemoteMemberInfo {
    fn from(member: RemoteMemberInfoWire) -> Self {
        Self {
            member_id: member.member_id,
            role: member.role,
            last_acknowledged_revision: member.last_acknowledged_revision,
            ack_status: member.ack_status,
            is_online: member.is_online,
            is_stale: member.is_stale,
        }
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
    pub cached_content_hashes: Vec<String>,
    pub activity: RoomActivity,
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
    Credential(#[from] ltk_manager_core::room_sync::CredentialError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Network error: {0}")]
    Network(String),
}

struct UploadProgressReader {
    file: fs::File,
    room_id: String,
    content_hash: ContentHash,
    display_name: String,
    transferred: u64,
    total: u64,
    events: RoomEventReporter,
}

impl Read for UploadProgressReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.file.read(buffer)?;
        self.transferred = self.transferred.saturating_add(read as u64);
        if read > 0 {
            self.events.emit_transfer(TransferProgress {
                room_id: self.room_id.clone(),
                content_hash: self.content_hash.clone(),
                display_name: Some(self.display_name.clone()),
                direction: TransferDirection::Upload,
                transferred_bytes: self.transferred.min(self.total),
                total_bytes: self.total,
                attempt: 1,
            });
        }
        Ok(read)
    }
}

#[derive(Clone)]
struct RoomEventReporter {
    events: Arc<dyn EventSink>,
    transfers: Arc<Mutex<HashMap<String, Instant>>>,
    presence: Arc<Mutex<PresenceEventTimes>>,
    activities: Arc<Mutex<HashMap<String, RoomActivity>>>,
}

impl RoomEventReporter {
    fn new(events: Arc<dyn EventSink>) -> Self {
        Self {
            events,
            transfers: Arc::new(Mutex::new(HashMap::new())),
            presence: Arc::new(Mutex::new(HashMap::new())),
            activities: Arc::new(Mutex::new(HashMap::new())),
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

    fn emit_publish(&self, progress: RoomPublishProgress) {
        let activity = match progress.stage {
            RoomPublishStage::Preparing => RoomActivityStage::Preparing,
            RoomPublishStage::Uploading => RoomActivityStage::Uploading,
            RoomPublishStage::Publishing | RoomPublishStage::Finalizing => {
                RoomActivityStage::Publishing
            }
            RoomPublishStage::Complete => RoomActivityStage::Complete,
            RoomPublishStage::Failed => RoomActivityStage::Failed,
        };
        self.emit_activity(&progress.room_id, activity);
        self.events
            .emit(BackendEvent::RoomPublishProgress(progress));
    }

    fn activity(&self, room_id: &str) -> RoomActivity {
        self.activities
            .lock()
            .get(room_id)
            .cloned()
            .unwrap_or_else(|| RoomActivity {
                room_id: room_id.to_string(),
                stage: RoomActivityStage::Idle,
                updated_at_ms: unix_time_ms(),
            })
    }

    fn emit_activity(&self, room_id: &str, stage: RoomActivityStage) {
        let activity = RoomActivity {
            room_id: room_id.to_string(),
            stage,
            updated_at_ms: unix_time_ms(),
        };
        self.activities
            .lock()
            .insert(room_id.to_string(), activity.clone());
        self.events
            .emit(BackendEvent::RoomActivityChanged(activity));
    }

    /// Complete a read-only check only if no real operation has replaced its visible state.
    fn finish_check(&self, room_id: &str, stage: RoomActivityStage) {
        let activity = {
            let mut activities = self.activities.lock();
            if !activities
                .get(room_id)
                .is_some_and(|activity| activity.stage == RoomActivityStage::Checking)
            {
                return;
            }

            let activity = RoomActivity {
                room_id: room_id.to_string(),
                stage,
                updated_at_ms: unix_time_ms(),
            };
            activities.insert(room_id.to_string(), activity.clone());
            activity
        };
        self.events
            .emit(BackendEvent::RoomActivityChanged(activity));
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

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
