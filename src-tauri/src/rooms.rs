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
            return Err(RoomRuntimeError::Network(format!("Server error ({status}): {text}")));
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
            let _ = vault.write(room_id, ltk_manager_core::room_sync::RoomSecretKind::OwnerToken, owner_token.as_bytes());
            let _ = vault.write(room_id, ltk_manager_core::room_sync::RoomSecretKind::MemberToken, member_token.as_bytes());
        }

        let owner_member_id = format!("owner-{}", &owner_token[..8.min(owner_token.len())]);
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
            return Err(RoomRuntimeError::Network(format!("Server error ({status}): {text}")));
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
            let _ = vault.write(room_id, ltk_manager_core::room_sync::RoomSecretKind::MemberToken, member_token.as_bytes());
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
            return Err(RoomRuntimeError::Network(format!("Server error ({status}): {text}")));
        }

        let members: Vec<RemoteMemberInfo> = res
            .json()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        Ok(members)
    }

    /// Synchronize manifest and missing blobs from the authoritative server.
    pub fn sync_remote_room(
        &self,
        room_id: &str,
    ) -> Result<RoomSyncSnapshot, RoomRuntimeError> {
        let base_url = room_server_url();
        let manifest_url = format!("{base_url}/v1/rooms/{room_id}/manifest");
        let token = self.get_room_token(room_id)?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
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
            return Err(RoomRuntimeError::Network(format!("Failed to fetch manifest ({status}): {text}")));
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
                continue;
            }

            let dl_url_endpoint = format!("{base_url}/v1/rooms/{room_id}/blobs/{}/download_url", room_mod.content_hash.as_str());
            let dl_res = client
                .get(&dl_url_endpoint)
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

            if !dl_res.status().is_success() {
                return Err(RoomRuntimeError::Network(format!("Failed download URL for {}: {}", room_mod.content_hash.as_str(), dl_res.status())));
            }

            let dl_info: serde_json::Value = dl_res.json().map_err(|e| RoomRuntimeError::Network(e.to_string()))?;
            let signed_download_url = dl_info.get("download_url").and_then(|v| v.as_str()).ok_or_else(|| RoomRuntimeError::Network("Missing download_url".to_string()))?;

            let partial_path = self.cache.prepare_partial(&room_mod.content_hash)?;
            let mut file = fs::File::create(&partial_path)?;
            let mut blob_resp = client.get(signed_download_url).send().map_err(|e| RoomRuntimeError::Network(e.to_string()))?;
            if !blob_resp.status().is_success() {
                return Err(RoomRuntimeError::Network(format!("Failed downloading blob {}: {}", room_mod.content_hash.as_str(), blob_resp.status())));
            }
            std::io::copy(&mut blob_resp, &mut file)?;
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

    fn get_owner_token(&self, room_id: &str) -> Result<String, RoomRuntimeError> {
        #[cfg(windows)]
        {
            let vault = ltk_manager_core::room_sync::RoomCredentialVault::default();
            if let Ok(Some(secret)) = vault.read(room_id, ltk_manager_core::room_sync::RoomSecretKind::OwnerToken) {
                if let Ok(s) = std::str::from_utf8(secret.as_bytes()) {
                    return Ok(s.to_string());
                }
            }
        }
        Err(RoomRuntimeError::Network("Only the room owner can publish mods to this room".to_string()))
    }

    pub fn publish_profile_to_remote_room(
        &self,
        room_id: &str,
        profile_id: Option<&str>,
        library: &ltk_manager_core::mods::ModLibrary,
        config: &ltk_manager_core::config::Config,
    ) -> Result<RoomSyncSnapshot, RoomRuntimeError> {
        let owner_token = self.get_owner_token(room_id)?;
        let (_profile, artifacts) = library
            .collect_profile_room_artifacts(config, profile_id)
            .map_err(|e| RoomRuntimeError::Network(format!("Failed collecting profile mods: {e}")))?;

        if artifacts.is_empty() {
            return Err(RoomRuntimeError::Network(
                "The selected profile has no enabled mods with archives to publish.".to_string(),
            ));
        }

        let base_url = room_server_url();
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        // 1. Fetch current room info to find current_revision
        let info_url = format!("{base_url}/v1/rooms/{room_id}");
        let info_res = client
            .get(&info_url)
            .header("Authorization", format!("Bearer {owner_token}"))
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        let current_revision: i64 = if info_res.status().is_success() {
            let body: serde_json::Value = info_res.json().unwrap_or_default();
            body["revision"].as_i64().unwrap_or(0)
        } else {
            0
        };

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
            .header("Authorization", format!("Bearer {owner_token}"))
            .json(&serde_json::json!({ "hashes": hashes }))
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        let missing_hashes: Vec<String> = if check_res.status().is_success() {
            let body: serde_json::Value = check_res.json().unwrap_or_default();
            body["missing_hashes"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            hashes.clone()
        };

        // 3. Upload missing blobs
        for hash in &missing_hashes {
            let Some((path, mod_info)) = artifacts.iter().find(|(_, m)| m.content_hash.as_str() == hash) else {
                continue;
            };

            let upload_url_endpoint = format!("{base_url}/v1/rooms/{room_id}/blobs/upload_url");
            let grant_res = client
                .post(&upload_url_endpoint)
                .header("Authorization", format!("Bearer {owner_token}"))
                .json(&serde_json::json!({
                    "content_hash": hash,
                    "size_bytes": mod_info.size_bytes,
                }))
                .send()
                .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

            if !grant_res.status().is_success() {
                let err_text = grant_res.text().unwrap_or_default();
                return Err(RoomRuntimeError::Network(format!("Failed to request upload URL for blob {hash}: {err_text}")));
            }

            let grant_body: serde_json::Value = grant_res.json().map_err(|e| RoomRuntimeError::Network(e.to_string()))?;
            let upload_url = grant_body["upload_url"].as_str().ok_or_else(|| {
                RoomRuntimeError::Network("Server did not return upload_url".to_string())
            })?;

            let file_bytes = std::fs::read(path).map_err(|e| RoomRuntimeError::Io(e))?;
            let put_res = client
                .put(upload_url)
                .header("X-Content-SHA256", hash)
                .header("Content-Type", "application/octet-stream")
                .body(file_bytes)
                .send()
                .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

            if !put_res.status().is_success() {
                let err_text = put_res.text().unwrap_or_default();
                return Err(RoomRuntimeError::Network(format!("Failed to upload blob {hash}: {err_text}")));
            }
        }

        // 4. Publish the manifest revision
        let publish_url = format!("{base_url}/v1/rooms/{room_id}/manifest");
        let pub_res = client
            .post(&publish_url)
            .header("Authorization", format!("Bearer {owner_token}"))
            .json(&serde_json::json!({
                "previous_revision": current_revision,
                "manifest": manifest,
            }))
            .send()
            .map_err(|e| RoomRuntimeError::Network(e.to_string()))?;

        if !pub_res.status().is_success() {
            let err_text = pub_res.text().unwrap_or_default();
            return Err(RoomRuntimeError::Network(format!("Failed to publish manifest: {err_text}")));
        }

        // 5. Commit local files into host's cache so host is immediately synchronized
        for (path, mod_info) in &artifacts {
            let canonical = ltk_manager_core::room_sync::CanonicalRoomArtifact {
                content_hash: mod_info.content_hash.clone(),
                size_bytes: mod_info.size_bytes,
                format: mod_info.format,
            };
            if !self.cache.contains(&canonical)? {
                let partial = self.cache.prepare_partial(&canonical.content_hash)?;
                let _ = std::fs::copy(path, &partial);
                let _ = self.cache.commit_partial(&canonical);
            }
        }

        self.synchronize_manifest(manifest)?;
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
        self.sync_remote_room(room_id)?;
        self.prepare_revision(library, config, room_id)?;
        self.create_profile(library, config, room_id)
    }

    fn get_room_token(&self, room_id: &str) -> Result<String, RoomRuntimeError> {
        #[cfg(windows)]
        {
            let vault = ltk_manager_core::room_sync::RoomCredentialVault::default();
            if let Ok(Some(secret)) = vault.read(room_id, ltk_manager_core::room_sync::RoomSecretKind::MemberToken) {
                if let Ok(s) = std::str::from_utf8(secret.as_bytes()) {
                    return Ok(s.to_string());
                }
            }
            if let Ok(Some(secret)) = vault.read(room_id, ltk_manager_core::room_sync::RoomSecretKind::OwnerToken) {
                if let Ok(s) = std::str::from_utf8(secret.as_bytes()) {
                    return Ok(s.to_string());
                }
            }
        }
        Err(RoomRuntimeError::Network("No authentication token found for room".to_string()))
    }

    /// Callback a future HTTP/WebSocket transport supplies to the transfer engine.
    pub fn transfer_progress_callback(&self) -> TransferProgressCallback {
        let events = self.events.clone();
        Arc::new(move |progress| events.emit_transfer(progress))
    }
}

pub fn room_server_url() -> String {
    std::env::var("LTK_ROOM_SERVER_URL")
        .unwrap_or_else(|_| "http://177.153.59.168:3000".to_string())
}

/// Remote member presence information from the room server.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RemoteMemberInfo {
    pub member_id: String,
    pub role: String,
    pub last_acknowledged_revision: i64,
    pub ack_status: String,
    pub is_online: bool,
    pub is_stale: bool,
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
    #[error("Network error: {0}")]
    Network(String),
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
