//! Explicit client-side room synchronization state machine.
//!
//! The machine coordinates manifests, cache verification, and durable revision acceptance only. It
//! has no dependency on profiles, the patcher, the launcher, or game paths.

use super::{
    CanonicalRoomArtifact, ContentHash, ManifestLimits, RoomCache, RoomCacheError, RoomManifest,
    RoomStateError, RoomStateStore,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub enum RoomSyncPhase {
    Disconnected,
    Connecting,
    Comparing,
    Transferring,
    Verifying,
    Synchronized,
    Stale,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub enum RoomSyncBlockReason {
    Authorization,
    Protocol,
    Storage,
    Integrity,
    Compatibility,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct RoomSyncSnapshot {
    pub room_id: String,
    pub phase: RoomSyncPhase,
    /// Last revision committed transactionally and safe to keep using.
    pub active_revision: u64,
    /// Revision being compared/downloaded, or one announced by the server.
    pub target_revision: Option<u64>,
    pub total_blobs: usize,
    pub verified_blobs: usize,
    pub missing_blobs: usize,
    pub previous_revision_available: bool,
    pub block_reason: Option<RoomSyncBlockReason>,
}

#[derive(Debug, Clone)]
pub struct RoomSyncSession {
    room_id: String,
    phase: RoomSyncPhase,
    active_revision: u64,
    announced_revision: Option<u64>,
    target: Option<TargetRevision>,
    block_reason: Option<RoomSyncBlockReason>,
}

impl RoomSyncSession {
    /// Restore accepted and staged revisions after process restart without assuming connectivity.
    pub fn restore(
        store: &RoomStateStore,
        cache: &RoomCache,
        room_id: &str,
        limits: ManifestLimits,
    ) -> Result<Self, RoomClientError> {
        let joined = store
            .room(room_id)?
            .ok_or_else(|| RoomClientError::RoomNotJoined(room_id.to_string()))?;
        let target = store
            .staged_manifest(room_id)?
            .map(|manifest| TargetRevision::from_manifest(manifest, cache, limits))
            .transpose()?;
        if target
            .as_ref()
            .is_some_and(|target| target.manifest.revision <= joined.last_accepted_revision)
        {
            return Err(RoomClientError::InvalidStagedRevision);
        }

        Ok(Self {
            room_id: room_id.to_string(),
            phase: RoomSyncPhase::Disconnected,
            active_revision: joined.last_accepted_revision,
            announced_revision: target.as_ref().map(|target| target.manifest.revision),
            target,
            block_reason: None,
        })
    }

    pub fn snapshot(&self) -> RoomSyncSnapshot {
        let (target_revision, total_blobs, verified_blobs) = match &self.target {
            Some(target) => (
                Some(target.manifest.revision),
                target.manifest.mods.len(),
                target.verified.len(),
            ),
            None => (self.announced_revision, 0, 0),
        };
        RoomSyncSnapshot {
            room_id: self.room_id.clone(),
            phase: self.phase,
            active_revision: self.active_revision,
            target_revision,
            total_blobs,
            verified_blobs,
            missing_blobs: total_blobs.saturating_sub(verified_blobs),
            previous_revision_available: self.active_revision != 0
                && target_revision.is_some_and(|revision| revision > self.active_revision),
            block_reason: self.block_reason,
        }
    }

    pub fn begin_connect(&mut self) -> Result<(), RoomClientError> {
        self.expect_phase(&[
            RoomSyncPhase::Disconnected,
            RoomSyncPhase::Stale,
            RoomSyncPhase::Synchronized,
        ])?;
        self.phase = RoomSyncPhase::Connecting;
        self.block_reason = None;
        Ok(())
    }

    pub fn connected(&mut self) -> Result<(), RoomClientError> {
        self.expect_phase(&[RoomSyncPhase::Connecting])?;
        self.phase = RoomSyncPhase::Comparing;
        Ok(())
    }

    /// Record an announcement received before its full manifest is fetched.
    pub fn remote_revision_announced(&mut self, revision: u64) -> Result<(), RoomClientError> {
        if revision == 0 {
            return Err(RoomClientError::InvalidRevision);
        }
        self.announced_revision = Some(
            self.announced_revision
                .map_or(revision, |current| current.max(revision)),
        );
        if revision > self.active_revision && self.phase == RoomSyncPhase::Synchronized {
            self.phase = RoomSyncPhase::Stale;
        }
        Ok(())
    }

    /// Compare a received manifest, persist its target references, and choose transfer/verify state.
    pub fn compare_manifest(
        &mut self,
        store: &RoomStateStore,
        cache: &RoomCache,
        manifest: RoomManifest,
        limits: ManifestLimits,
    ) -> Result<RoomSyncSnapshot, RoomClientError> {
        self.expect_phase(&[RoomSyncPhase::Comparing])?;
        if manifest.room_id != self.room_id {
            return Err(RoomClientError::RoomMismatch);
        }
        if manifest.revision < self.active_revision {
            self.block(RoomSyncBlockReason::Protocol);
            return Err(RoomClientError::OlderManifest {
                active: self.active_revision,
                received: manifest.revision,
            });
        }

        store.stage_manifest(&manifest, limits)?;
        let target = TargetRevision::from_manifest(manifest, cache, limits)?;
        let all_verified = target.all_verified();
        let same_revision = target.manifest.revision == self.active_revision;
        self.announced_revision = Some(target.manifest.revision);
        self.target = Some(target);
        self.phase = if all_verified {
            if same_revision {
                self.target = None;
                self.announced_revision = None;
                RoomSyncPhase::Synchronized
            } else {
                RoomSyncPhase::Verifying
            }
        } else {
            RoomSyncPhase::Transferring
        };
        Ok(self.snapshot())
    }

    /// Re-verify one completed transfer from cache; a progress event alone can never advance state.
    pub fn record_blob_verified(
        &mut self,
        cache: &RoomCache,
        content_hash: &ContentHash,
    ) -> Result<RoomSyncSnapshot, RoomClientError> {
        self.expect_phase(&[RoomSyncPhase::Transferring, RoomSyncPhase::Verifying])?;
        let target = self.target.as_mut().ok_or(RoomClientError::MissingTarget)?;
        let room_mod = target
            .manifest
            .mods
            .iter()
            .find(|room_mod| &room_mod.content_hash == content_hash)
            .ok_or(RoomClientError::UnknownBlob)?;
        let artifact = artifact_of(room_mod);
        if !cache.contains(&artifact)? {
            return Err(RoomClientError::BlobNotVerified(content_hash.clone()));
        }
        target.verified.insert(content_hash.clone());
        if target.all_verified() {
            self.phase = RoomSyncPhase::Verifying;
        }
        Ok(self.snapshot())
    }

    /// Atomically make the fully verified target the accepted revision.
    pub fn commit_verified_revision(
        &mut self,
        store: &RoomStateStore,
        cache: &RoomCache,
        limits: ManifestLimits,
    ) -> Result<RoomSyncSnapshot, RoomClientError> {
        self.expect_phase(&[RoomSyncPhase::Verifying])?;
        let target = self.target.as_mut().ok_or(RoomClientError::MissingTarget)?;
        target.refresh(cache)?;
        if !target.all_verified() {
            self.phase = RoomSyncPhase::Transferring;
            return Err(RoomClientError::NotAllBlobsVerified);
        }

        store.accept_verified_manifest(cache, &target.manifest, limits)?;
        self.active_revision = target.manifest.revision;
        self.target = None;
        self.announced_revision = None;
        self.block_reason = None;
        self.phase = RoomSyncPhase::Synchronized;
        Ok(self.snapshot())
    }

    /// Connection loss never discards the previous revision or the resumable staged target.
    pub fn disconnect(&mut self) {
        self.phase = RoomSyncPhase::Disconnected;
        self.block_reason = None;
    }

    pub fn block(&mut self, reason: RoomSyncBlockReason) {
        self.phase = RoomSyncPhase::Blocked;
        self.block_reason = Some(reason);
    }

    pub fn clear_block(&mut self) -> Result<(), RoomClientError> {
        self.expect_phase(&[RoomSyncPhase::Blocked])?;
        self.phase = RoomSyncPhase::Disconnected;
        self.block_reason = None;
        Ok(())
    }

    /// Explicitly abandon only the pending revision. Accepted cache references remain intact.
    pub fn discard_target(
        &mut self,
        store: &RoomStateStore,
    ) -> Result<RoomSyncSnapshot, RoomClientError> {
        store.discard_staged_manifest(&self.room_id)?;
        self.target = None;
        self.announced_revision = None;
        self.block_reason = None;
        self.phase = RoomSyncPhase::Disconnected;
        Ok(self.snapshot())
    }

    fn expect_phase(&self, allowed: &[RoomSyncPhase]) -> Result<(), RoomClientError> {
        if allowed.contains(&self.phase) {
            Ok(())
        } else {
            Err(RoomClientError::UnexpectedPhase(self.phase))
        }
    }
}

#[derive(Debug, Clone)]
struct TargetRevision {
    manifest: RoomManifest,
    verified: HashSet<ContentHash>,
}

impl TargetRevision {
    fn from_manifest(
        manifest: RoomManifest,
        cache: &RoomCache,
        limits: ManifestLimits,
    ) -> Result<Self, RoomClientError> {
        manifest.validate(limits).map_err(RoomStateError::from)?;
        let mut target = Self {
            manifest,
            verified: HashSet::new(),
        };
        target.refresh(cache)?;
        Ok(target)
    }

    fn refresh(&mut self, cache: &RoomCache) -> Result<(), RoomClientError> {
        self.verified.clear();
        for room_mod in &self.manifest.mods {
            if cache.contains(&artifact_of(room_mod))? {
                self.verified.insert(room_mod.content_hash.clone());
            }
        }
        Ok(())
    }

    fn all_verified(&self) -> bool {
        self.verified.len() == self.manifest.mods.len()
    }
}

fn artifact_of(room_mod: &super::RoomMod) -> CanonicalRoomArtifact {
    CanonicalRoomArtifact {
        content_hash: room_mod.content_hash.clone(),
        size_bytes: room_mod.size_bytes,
        format: room_mod.format,
    }
}

#[derive(Debug, Error)]
pub enum RoomClientError {
    #[error(transparent)]
    State(#[from] RoomStateError),
    #[error(transparent)]
    Cache(#[from] RoomCacheError),
    #[error("room is not joined locally: {0}")]
    RoomNotJoined(String),
    #[error("operation is invalid while room synchronization is {0:?}")]
    UnexpectedPhase(RoomSyncPhase),
    #[error("manifest belongs to a different room")]
    RoomMismatch,
    #[error("manifest revision {received} is older than active revision {active}")]
    OlderManifest { active: u64, received: u64 },
    #[error("room revision must be greater than zero")]
    InvalidRevision,
    #[error("persisted staged revision is not newer than the accepted revision")]
    InvalidStagedRevision,
    #[error("room synchronization has no target manifest")]
    MissingTarget,
    #[error("content hash is not part of the target manifest")]
    UnknownBlob,
    #[error("cache blob is not present and verified: {0}")]
    BlobNotVerified(ContentHash),
    #[error("not every target blob is present and verified")]
    NotAllBlobsVerified,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_sync::{ROOM_MANIFEST_SCHEMA_VERSION, RoomMod, RoomModFormat};
    use fs_err as fs;
    use std::path::Path;

    fn artifact(bytes: &[u8]) -> CanonicalRoomArtifact {
        CanonicalRoomArtifact {
            content_hash: ContentHash::from_reader(bytes).unwrap(),
            size_bytes: bytes.len() as u64,
            format: RoomModFormat::Modpkg,
        }
    }

    fn manifest(room_id: &str, revision: u64, artifacts: &[CanonicalRoomArtifact]) -> RoomManifest {
        RoomManifest {
            schema_version: ROOM_MANIFEST_SCHEMA_VERSION,
            room_id: room_id.to_string(),
            revision,
            game_build: None,
            mods: artifacts
                .iter()
                .enumerate()
                .map(|(index, artifact)| RoomMod {
                    content_hash: artifact.content_hash.clone(),
                    size_bytes: artifact.size_bytes,
                    format: artifact.format,
                    display_name: format!("Mod {index}"),
                    version: String::new(),
                    enabled: true,
                    suggested_layers: Vec::new(),
                })
                .collect(),
        }
    }

    fn setup(directory: &Path) -> (RoomStateStore, RoomCache) {
        let library = directory.join("library");
        fs::create_dir_all(&library).unwrap();
        let cache = RoomCache::open(directory.join("cache"), &[library]).unwrap();
        let store = RoomStateStore::open(directory.join("rooms.sqlite3")).unwrap();
        store.join_room("room_a", "member-1").unwrap();
        (store, cache)
    }

    fn put(cache: &RoomCache, artifact: &CanonicalRoomArtifact, bytes: &[u8]) {
        let partial = cache.prepare_partial(&artifact.content_hash).unwrap();
        fs::write(partial, bytes).unwrap();
        cache.commit_partial(artifact).unwrap();
    }

    fn comparing(store: &RoomStateStore, cache: &RoomCache) -> RoomSyncSession {
        let mut session =
            RoomSyncSession::restore(store, cache, "room_a", ManifestLimits::default()).unwrap();
        session.begin_connect().unwrap();
        session.connected().unwrap();
        session
    }

    #[test]
    fn normal_flow_requires_verified_blobs_before_atomic_acceptance() {
        let directory = tempfile::tempdir().unwrap();
        let (store, cache) = setup(directory.path());
        let first = artifact(b"first");
        let second = artifact(b"second");
        let target = manifest("room_a", 1, &[first.clone(), second.clone()]);
        let mut session = comparing(&store, &cache);

        let compared = session
            .compare_manifest(&store, &cache, target, ManifestLimits::default())
            .unwrap();
        assert_eq!(compared.phase, RoomSyncPhase::Transferring);
        assert_eq!(compared.missing_blobs, 2);
        assert!(matches!(
            session.commit_verified_revision(&store, &cache, ManifestLimits::default()),
            Err(RoomClientError::UnexpectedPhase(
                RoomSyncPhase::Transferring
            ))
        ));

        put(&cache, &first, b"first");
        let progress = session
            .record_blob_verified(&cache, &first.content_hash)
            .unwrap();
        assert_eq!(progress.phase, RoomSyncPhase::Transferring);
        assert_eq!(progress.verified_blobs, 1);

        put(&cache, &second, b"second");
        assert_eq!(
            session
                .record_blob_verified(&cache, &second.content_hash)
                .unwrap()
                .phase,
            RoomSyncPhase::Verifying
        );
        let synced = session
            .commit_verified_revision(&store, &cache, ManifestLimits::default())
            .unwrap();
        assert_eq!(synced.phase, RoomSyncPhase::Synchronized);
        assert_eq!(synced.active_revision, 1);
        assert!(!synced.previous_revision_available);
        assert_eq!(
            store
                .room("room_a")
                .unwrap()
                .unwrap()
                .last_accepted_revision,
            1
        );
    }

    #[test]
    fn old_revision_stays_referenced_until_new_revision_commits() {
        let directory = tempfile::tempdir().unwrap();
        let (store, cache) = setup(directory.path());
        let old = artifact(b"old");
        let new = artifact(b"new");
        put(&cache, &old, b"old");
        store
            .accept_verified_manifest(
                &cache,
                &manifest("room_a", 1, std::slice::from_ref(&old)),
                ManifestLimits::default(),
            )
            .unwrap();

        let mut session = comparing(&store, &cache);
        session
            .compare_manifest(
                &store,
                &cache,
                manifest("room_a", 2, std::slice::from_ref(&new)),
                ManifestLimits::default(),
            )
            .unwrap();
        let retained = store.cache_references().unwrap();
        assert!(retained.contains(&old.content_hash));
        assert!(retained.contains(&new.content_hash));
        assert_eq!(session.snapshot().active_revision, 1);
        assert!(session.snapshot().previous_revision_available);

        put(&cache, &new, b"new");
        session
            .record_blob_verified(&cache, &new.content_hash)
            .unwrap();
        session
            .commit_verified_revision(&store, &cache, ManifestLimits::default())
            .unwrap();
        let retained = store.cache_references().unwrap();
        assert!(!retained.contains(&old.content_hash));
        assert!(retained.contains(&new.content_hash));
    }

    #[test]
    fn staged_target_survives_disconnect_and_process_restore() {
        let directory = tempfile::tempdir().unwrap();
        let (store, cache) = setup(directory.path());
        let content = artifact(b"pending");
        let mut session = comparing(&store, &cache);
        session
            .compare_manifest(
                &store,
                &cache,
                manifest("room_a", 3, std::slice::from_ref(&content)),
                ManifestLimits::default(),
            )
            .unwrap();
        session.disconnect();

        let restored =
            RoomSyncSession::restore(&store, &cache, "room_a", ManifestLimits::default()).unwrap();
        let snapshot = restored.snapshot();
        assert_eq!(snapshot.phase, RoomSyncPhase::Disconnected);
        assert_eq!(snapshot.active_revision, 0);
        assert_eq!(snapshot.target_revision, Some(3));
        assert_eq!(snapshot.missing_blobs, 1);
    }

    #[test]
    fn announcements_and_blocks_preserve_the_active_revision() {
        let directory = tempfile::tempdir().unwrap();
        let (store, cache) = setup(directory.path());
        let content = artifact(b"active");
        put(&cache, &content, b"active");
        store
            .accept_verified_manifest(
                &cache,
                &manifest("room_a", 1, std::slice::from_ref(&content)),
                ManifestLimits::default(),
            )
            .unwrap();
        let mut session = comparing(&store, &cache);
        session
            .compare_manifest(
                &store,
                &cache,
                manifest("room_a", 1, std::slice::from_ref(&content)),
                ManifestLimits::default(),
            )
            .unwrap();
        session.remote_revision_announced(2).unwrap();
        assert_eq!(session.snapshot().phase, RoomSyncPhase::Stale);
        assert_eq!(session.snapshot().active_revision, 1);

        session.block(RoomSyncBlockReason::Compatibility);
        assert_eq!(session.snapshot().phase, RoomSyncPhase::Blocked);
        assert_eq!(session.snapshot().active_revision, 1);
        session.clear_block().unwrap();
        assert_eq!(session.snapshot().phase, RoomSyncPhase::Disconnected);
    }

    #[test]
    fn a_progress_claim_cannot_mark_missing_or_unknown_content_verified() {
        let directory = tempfile::tempdir().unwrap();
        let (store, cache) = setup(directory.path());
        let content = artifact(b"expected");
        let unknown = artifact(b"unknown");
        let mut session = comparing(&store, &cache);
        session
            .compare_manifest(
                &store,
                &cache,
                manifest("room_a", 1, std::slice::from_ref(&content)),
                ManifestLimits::default(),
            )
            .unwrap();

        assert!(matches!(
            session.record_blob_verified(&cache, &unknown.content_hash),
            Err(RoomClientError::UnknownBlob)
        ));
        assert!(matches!(
            session.record_blob_verified(&cache, &content.content_hash),
            Err(RoomClientError::BlobNotVerified(_))
        ));
        assert_eq!(session.snapshot().verified_blobs, 0);
    }

    #[test]
    fn invalid_transitions_and_older_manifests_are_explicit() {
        let directory = tempfile::tempdir().unwrap();
        let (store, cache) = setup(directory.path());
        let mut session =
            RoomSyncSession::restore(&store, &cache, "room_a", ManifestLimits::default()).unwrap();
        assert!(matches!(
            session.connected(),
            Err(RoomClientError::UnexpectedPhase(
                RoomSyncPhase::Disconnected
            ))
        ));

        let active = artifact(b"active");
        put(&cache, &active, b"active");
        store
            .accept_verified_manifest(
                &cache,
                &manifest("room_a", 2, std::slice::from_ref(&active)),
                ManifestLimits::default(),
            )
            .unwrap();
        let mut session = comparing(&store, &cache);
        assert!(matches!(
            session.compare_manifest(
                &store,
                &cache,
                manifest("room_a", 1, &[]),
                ManifestLimits::default()
            ),
            Err(RoomClientError::OlderManifest { .. })
        ));
        assert_eq!(session.snapshot().phase, RoomSyncPhase::Blocked);
        assert_eq!(session.snapshot().active_revision, 2);
    }
}
