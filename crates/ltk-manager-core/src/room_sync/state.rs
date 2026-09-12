//! Transactional local state for joined rooms and resumable transfers.
//!
//! The database contains identifiers, revisions, progress, and content hashes only. Authentication
//! tokens are deliberately absent and belong to [`super::RoomCredentialVault`].

use super::{
    CanonicalRoomArtifact, ContentHash, ManifestError, ManifestLimits, RoomCache, RoomCacheError,
    RoomCacheReferences, RoomManifest, RoomModFormat, validate_room_id,
};
use chrono::Utc;
use fs_err as fs;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

const STATE_SCHEMA_VERSION: i64 = 4;
const MAX_MEMBER_ID_CHARS: usize = 128;
const MAX_LOCAL_MOD_ID_CHARS: usize = 128;
const MAX_LOCAL_PROFILE_ID_CHARS: usize = 128;
const MAX_ETAG_CHARS: usize = 1_024;

const SCHEMA_V1: &str = r#"
CREATE TABLE rooms (
    room_id TEXT PRIMARY KEY NOT NULL,
    member_id TEXT NOT NULL,
    last_accepted_revision INTEGER NOT NULL DEFAULT 0 CHECK (last_accepted_revision >= 0),
    joined_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE cache_references (
    room_id TEXT NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL CHECK (
        length(content_hash) = 64 AND content_hash NOT GLOB '*[^0-9a-f]*'
    ),
    PRIMARY KEY (room_id, content_hash)
) STRICT, WITHOUT ROWID;

CREATE TABLE pending_transfers (
    room_id TEXT NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL CHECK (
        length(content_hash) = 64 AND content_hash NOT GLOB '*[^0-9a-f]*'
    ),
    direction TEXT NOT NULL CHECK (direction IN ('download', 'upload')),
    state TEXT NOT NULL CHECK (state IN ('queued', 'transferring', 'paused')),
    format TEXT NOT NULL CHECK (format IN ('modpkg', 'fantome')),
    expected_size_bytes INTEGER NOT NULL CHECK (expected_size_bytes > 0),
    transferred_bytes INTEGER NOT NULL CHECK (
        transferred_bytes >= 0 AND transferred_bytes <= expected_size_bytes
    ),
    etag TEXT,
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (room_id, content_hash, direction)
) STRICT, WITHOUT ROWID;

PRAGMA user_version = 1;
"#;

const SCHEMA_V2: &str = r#"
CREATE TABLE staged_manifests (
    room_id TEXT PRIMARY KEY NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
    revision INTEGER NOT NULL CHECK (revision > 0),
    manifest_json TEXT NOT NULL,
    staged_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE staged_cache_references (
    room_id TEXT NOT NULL REFERENCES staged_manifests(room_id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL CHECK (
        length(content_hash) = 64 AND content_hash NOT GLOB '*[^0-9a-f]*'
    ),
    PRIMARY KEY (room_id, content_hash)
) STRICT, WITHOUT ROWID;

PRAGMA user_version = 2;
"#;

const SCHEMA_V3: &str = r#"
CREATE TABLE accepted_manifests (
    room_id TEXT PRIMARY KEY NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
    revision INTEGER NOT NULL CHECK (revision > 0),
    manifest_json TEXT NOT NULL,
    accepted_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE prepared_revisions (
    room_id TEXT PRIMARY KEY NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
    revision INTEGER NOT NULL CHECK (revision > 0),
    prepared_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE prepared_mod_mappings (
    room_id TEXT NOT NULL REFERENCES prepared_revisions(room_id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL CHECK (
        length(content_hash) = 64 AND content_hash NOT GLOB '*[^0-9a-f]*'
    ),
    local_mod_id TEXT NOT NULL,
    PRIMARY KEY (room_id, content_hash),
    UNIQUE (room_id, local_mod_id)
) STRICT, WITHOUT ROWID;

PRAGMA user_version = 3;
"#;

const SCHEMA_V4: &str = r#"
CREATE TABLE room_profiles (
    room_id TEXT PRIMARY KEY NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
    revision INTEGER NOT NULL CHECK (revision > 0),
    local_profile_id TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

PRAGMA user_version = 4;
"#;

#[derive(Debug)]
pub struct RoomStateStore {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl RoomStateStore {
    /// Open the state database, create its schema, and recover transfers interrupted by a crash.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RoomStateError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        Self::from_connection(path.to_path_buf(), connection)
    }

    #[cfg(test)]
    pub(super) fn open_in_memory() -> Result<Self, RoomStateError> {
        Self::from_connection(PathBuf::from(":memory:"), Connection::open_in_memory()?)
    }

    fn from_connection(path: PathBuf, connection: Connection) -> Result<Self, RoomStateError> {
        configure(&connection)?;
        migrate(&connection)?;
        connection.execute(
            "UPDATE pending_transfers SET state = 'queued', updated_at_ms = ?1 \
             WHERE state = 'transferring'",
            [now_ms()],
        )?;
        Ok(Self {
            path,
            connection: Mutex::new(connection),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Join locally without accepting a manifest. Rejoining updates member identity while retaining
    /// the last accepted revision and cached references.
    pub fn join_room(&self, room_id: &str, member_id: &str) -> Result<JoinedRoom, RoomStateError> {
        validate_ids(room_id, member_id)?;
        let now = now_ms();
        self.connection.lock().execute(
            "INSERT INTO rooms (
                room_id, member_id, last_accepted_revision, joined_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 0, ?3, ?3)
             ON CONFLICT(room_id) DO UPDATE SET
                member_id = excluded.member_id,
                updated_at_ms = excluded.updated_at_ms",
            params![room_id, member_id, now],
        )?;
        self.room(room_id)?
            .ok_or_else(|| RoomStateError::RoomNotJoined(room_id.to_string()))
    }

    pub fn room(&self, room_id: &str) -> Result<Option<JoinedRoom>, RoomStateError> {
        validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
        self.connection
            .lock()
            .query_row(
                "SELECT room_id, member_id, last_accepted_revision, joined_at_ms, updated_at_ms
                 FROM rooms WHERE room_id = ?1",
                [room_id],
                joined_room_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn rooms(&self) -> Result<Vec<JoinedRoom>, RoomStateError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT room_id, member_id, last_accepted_revision, joined_at_ms, updated_at_ms
             FROM rooms ORDER BY joined_at_ms, room_id",
        )?;
        statement
            .query_map([], joined_room_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    /// Remove local membership and its database references. Cached files remain untouched until an
    /// explicit [`RoomCache::prune_unreferenced`] call using the remaining reference set.
    pub fn leave_room(&self, room_id: &str) -> Result<bool, RoomStateError> {
        validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
        Ok(self
            .connection
            .lock()
            .execute("DELETE FROM rooms WHERE room_id = ?1", [room_id])?
            != 0)
    }

    /// Persist the next validated manifest before its transfers start.
    ///
    /// Staged hashes participate in cache retention while the accepted revision remains unchanged.
    /// A newer staged revision replaces an older pending target atomically.
    pub fn stage_manifest(
        &self,
        manifest: &RoomManifest,
        limits: ManifestLimits,
    ) -> Result<ManifestStaging, RoomStateError> {
        manifest.validate(limits)?;
        let revision = to_sql_integer("manifest revision", manifest.revision)?;
        let json = serde_json::to_string(manifest)?;
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let current: i64 = transaction
            .query_row(
                "SELECT last_accepted_revision FROM rooms WHERE room_id = ?1",
                [&manifest.room_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| RoomStateError::RoomNotJoined(manifest.room_id.clone()))?;

        if revision < current {
            return Err(RoomStateError::StaleRevision {
                current: current as u64,
                attempted: manifest.revision,
            });
        }
        if revision == current {
            let accepted_json: Option<String> = transaction
                .query_row(
                    "SELECT manifest_json FROM accepted_manifests WHERE room_id = ?1",
                    [&manifest.room_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(accepted_json) = accepted_json {
                let accepted: RoomManifest = serde_json::from_str(&accepted_json)
                    .map_err(|error| RoomStateError::CorruptData(error.to_string()))?;
                return if accepted == *manifest {
                    Ok(ManifestStaging::AlreadyAccepted)
                } else {
                    Err(RoomStateError::RevisionConflict(manifest.revision))
                };
            }
            let accepted = reference_hashes(&transaction, &manifest.room_id)?;
            let received = manifest_hashes(manifest);
            return if accepted == received {
                Ok(ManifestStaging::AlreadyAccepted)
            } else {
                Err(RoomStateError::RevisionConflict(manifest.revision))
            };
        }

        let staged: Option<(i64, String)> = transaction
            .query_row(
                "SELECT revision, manifest_json FROM staged_manifests WHERE room_id = ?1",
                [&manifest.room_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((staged_revision, staged_json)) = staged {
            if revision < staged_revision {
                return Err(RoomStateError::StaleRevision {
                    current: staged_revision as u64,
                    attempted: manifest.revision,
                });
            }
            if revision == staged_revision {
                let existing: RoomManifest = serde_json::from_str(&staged_json)
                    .map_err(|error| RoomStateError::CorruptData(error.to_string()))?;
                return if existing == *manifest {
                    Ok(ManifestStaging::AlreadyStaged)
                } else {
                    Err(RoomStateError::RevisionConflict(manifest.revision))
                };
            }
        }

        transaction.execute(
            "DELETE FROM staged_cache_references WHERE room_id = ?1",
            [&manifest.room_id],
        )?;
        transaction.execute(
            "INSERT INTO staged_manifests (room_id, revision, manifest_json, staged_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(room_id) DO UPDATE SET
                revision = excluded.revision,
                manifest_json = excluded.manifest_json,
                staged_at_ms = excluded.staged_at_ms",
            params![manifest.room_id, revision, json, now_ms()],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO staged_cache_references (room_id, content_hash) VALUES (?1, ?2)",
            )?;
            for room_mod in &manifest.mods {
                insert.execute(params![manifest.room_id, room_mod.content_hash.as_str()])?;
            }
        }
        transaction.commit()?;
        Ok(ManifestStaging::Staged)
    }

    /// Load a target left staged by a previous process.
    pub fn staged_manifest(&self, room_id: &str) -> Result<Option<RoomManifest>, RoomStateError> {
        validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
        let json: Option<String> = self
            .connection
            .lock()
            .query_row(
                "SELECT manifest_json FROM staged_manifests WHERE room_id = ?1",
                [room_id],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| {
            serde_json::from_str(&json)
                .map_err(|error| RoomStateError::CorruptData(error.to_string()))
        })
        .transpose()
    }

    /// Load the complete manifest for the last atomically accepted revision.
    pub fn accepted_manifest(&self, room_id: &str) -> Result<Option<RoomManifest>, RoomStateError> {
        validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
        let json: Option<String> = self
            .connection
            .lock()
            .query_row(
                "SELECT manifest_json FROM accepted_manifests WHERE room_id = ?1",
                [room_id],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|json| {
            serde_json::from_str(&json)
                .map_err(|error| RoomStateError::CorruptData(error.to_string()))
        })
        .transpose()
    }

    /// Forget a pending target without changing the accepted revision or deleting cache files.
    pub fn discard_staged_manifest(&self, room_id: &str) -> Result<bool, RoomStateError> {
        validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
        Ok(self
            .connection
            .lock()
            .execute("DELETE FROM staged_manifests WHERE room_id = ?1", [room_id])?
            != 0)
    }

    /// Accept a manifest only when every referenced cache blob is present and verified.
    ///
    /// The revision and its cache references change in one SQLite transaction. Replaying the exact
    /// accepted revision is idempotent; reusing its revision number for different content fails.
    pub fn accept_verified_manifest(
        &self,
        cache: &RoomCache,
        manifest: &RoomManifest,
        limits: ManifestLimits,
    ) -> Result<ManifestAcceptance, RoomStateError> {
        manifest.validate(limits)?;

        for room_mod in &manifest.mods {
            let artifact = CanonicalRoomArtifact {
                content_hash: room_mod.content_hash.clone(),
                size_bytes: room_mod.size_bytes,
                format: room_mod.format,
            };
            if !cache.contains(&artifact)? {
                return Err(RoomStateError::MissingCachedBlob(
                    room_mod.content_hash.clone(),
                ));
            }
        }

        // Comparing a manifest stages it before transfers begin. Direct callers may skip that
        // phase, so stage only after verification here: a failed acceptance must not replace or
        // poison an already resumable target.
        self.stage_manifest(manifest, limits)?;
        let manifest_json = serde_json::to_string(manifest)?;

        let revision = to_sql_integer("manifest revision", manifest.revision)?;
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let current: i64 = transaction
            .query_row(
                "SELECT last_accepted_revision FROM rooms WHERE room_id = ?1",
                [&manifest.room_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| RoomStateError::RoomNotJoined(manifest.room_id.clone()))?;

        if revision < current {
            return Err(RoomStateError::StaleRevision {
                current: current as u64,
                attempted: manifest.revision,
            });
        }
        if revision == current {
            let stored = reference_hashes(&transaction, &manifest.room_id)?;
            if stored != manifest_hashes(manifest) {
                return Err(RoomStateError::RevisionConflict(manifest.revision));
            }
            transaction.execute(
                "INSERT INTO accepted_manifests (room_id, revision, manifest_json, accepted_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(room_id) DO NOTHING",
                params![manifest.room_id, revision, manifest_json, now_ms()],
            )?;
            transaction.commit()?;
            return Ok(ManifestAcceptance::AlreadyAccepted);
        }

        let staged_json: String = transaction
            .query_row(
                "SELECT manifest_json FROM staged_manifests WHERE room_id = ?1 AND revision = ?2",
                params![manifest.room_id, revision],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| RoomStateError::StagedManifestChanged(manifest.revision))?;
        let staged: RoomManifest = serde_json::from_str(&staged_json)
            .map_err(|error| RoomStateError::CorruptData(error.to_string()))?;
        if staged != *manifest {
            return Err(RoomStateError::StagedManifestChanged(manifest.revision));
        }

        transaction.execute(
            "DELETE FROM cache_references WHERE room_id = ?1",
            [&manifest.room_id],
        )?;
        {
            let mut insert = transaction
                .prepare("INSERT INTO cache_references (room_id, content_hash) VALUES (?1, ?2)")?;
            for room_mod in &manifest.mods {
                insert.execute(params![manifest.room_id, room_mod.content_hash.as_str()])?;
            }
        }
        transaction.execute(
            "UPDATE rooms SET last_accepted_revision = ?2, updated_at_ms = ?3
             WHERE room_id = ?1",
            params![manifest.room_id, revision, now_ms()],
        )?;
        transaction.execute(
            "INSERT INTO accepted_manifests (room_id, revision, manifest_json, accepted_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(room_id) DO UPDATE SET
                revision = excluded.revision,
                manifest_json = excluded.manifest_json,
                accepted_at_ms = excluded.accepted_at_ms",
            params![manifest.room_id, revision, manifest_json, now_ms()],
        )?;
        transaction.execute(
            "DELETE FROM staged_manifests WHERE room_id = ?1",
            [&manifest.room_id],
        )?;
        transaction.commit()?;
        Ok(ManifestAcceptance::Advanced)
    }

    /// Atomically remember the local UUID assigned to every blob in one accepted revision.
    ///
    /// This records preparation only. It does not edit profiles, enable mods, or invoke the
    /// patcher. The mapping must cover the accepted manifest exactly.
    pub fn record_prepared_revision(
        &self,
        manifest: &RoomManifest,
        mappings: &[PreparedModMapping],
        limits: ManifestLimits,
    ) -> Result<PreparedRevision, RoomStateError> {
        manifest.validate(limits)?;
        validate_prepared_mappings(manifest, mappings)?;
        let mut canonical_mappings = mappings.to_vec();
        canonical_mappings
            .sort_by(|left, right| left.content_hash.as_str().cmp(right.content_hash.as_str()));
        let revision = to_sql_integer("prepared revision", manifest.revision)?;
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let accepted_json: String = transaction
            .query_row(
                "SELECT manifest_json FROM accepted_manifests
                 WHERE room_id = ?1 AND revision = ?2",
                params![manifest.room_id, revision],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| RoomStateError::ManifestNotAccepted(manifest.revision))?;
        let accepted: RoomManifest = serde_json::from_str(&accepted_json)
            .map_err(|error| RoomStateError::CorruptData(error.to_string()))?;
        if accepted != *manifest {
            return Err(RoomStateError::RevisionConflict(manifest.revision));
        }

        transaction.execute(
            "DELETE FROM prepared_mod_mappings WHERE room_id = ?1",
            [&manifest.room_id],
        )?;
        let prepared_at_ms = now_ms();
        transaction.execute(
            "INSERT INTO prepared_revisions (room_id, revision, prepared_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(room_id) DO UPDATE SET
                revision = excluded.revision,
                prepared_at_ms = excluded.prepared_at_ms",
            params![manifest.room_id, revision, prepared_at_ms],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO prepared_mod_mappings (room_id, content_hash, local_mod_id)
                 VALUES (?1, ?2, ?3)",
            )?;
            for mapping in &canonical_mappings {
                insert.execute(params![
                    manifest.room_id,
                    mapping.content_hash.as_str(),
                    mapping.local_mod_id,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(PreparedRevision {
            room_id: manifest.room_id.clone(),
            revision: manifest.revision,
            prepared_at_ms,
            mappings: canonical_mappings,
        })
    }

    pub fn prepared_revision(
        &self,
        room_id: &str,
    ) -> Result<Option<PreparedRevision>, RoomStateError> {
        validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
        let connection = self.connection.lock();
        let prepared: Option<(i64, i64)> = connection
            .query_row(
                "SELECT revision, prepared_at_ms FROM prepared_revisions WHERE room_id = ?1",
                [room_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((revision, prepared_at_ms)) = prepared else {
            return Ok(None);
        };
        let mut statement = connection.prepare(
            "SELECT content_hash, local_mod_id FROM prepared_mod_mappings
             WHERE room_id = ?1 ORDER BY content_hash",
        )?;
        let mappings = statement
            .query_map([room_id], prepared_mapping_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(PreparedRevision {
            room_id: room_id.to_string(),
            revision: u64::try_from(revision)
                .map_err(|_| RoomStateError::CorruptData("negative revision".into()))?,
            prepared_at_ms,
            mappings,
        }))
    }

    /// Record a non-active local profile that represents one fully prepared accepted revision.
    pub fn record_room_profile(
        &self,
        manifest: &RoomManifest,
        local_profile_id: &str,
        limits: ManifestLimits,
    ) -> Result<RoomProfileBinding, RoomStateError> {
        manifest.validate(limits)?;
        validate_local_profile_id(local_profile_id)?;
        let revision = to_sql_integer("room profile revision", manifest.revision)?;
        let mut connection = self.connection.lock();
        let transaction = connection.transaction()?;
        let accepted_json: String = transaction
            .query_row(
                "SELECT manifest_json FROM accepted_manifests
                 WHERE room_id = ?1 AND revision = ?2",
                params![manifest.room_id, revision],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| RoomStateError::ManifestNotAccepted(manifest.revision))?;
        let accepted: RoomManifest = serde_json::from_str(&accepted_json)
            .map_err(|error| RoomStateError::CorruptData(error.to_string()))?;
        if accepted != *manifest {
            return Err(RoomStateError::RevisionConflict(manifest.revision));
        }
        let prepared_revision: Option<i64> = transaction
            .query_row(
                "SELECT revision FROM prepared_revisions WHERE room_id = ?1",
                [&manifest.room_id],
                |row| row.get(0),
            )
            .optional()?;
        match prepared_revision {
            Some(prepared) if prepared == revision => {}
            Some(prepared) => {
                return Err(RoomStateError::PreparedRevisionMismatch {
                    prepared: sql_revision(prepared)?,
                    requested: manifest.revision,
                });
            }
            None => return Err(RoomStateError::PreparedRevisionMissing),
        }

        let expected = manifest_hashes(manifest);
        let prepared_hashes = prepared_mapping_hashes(&transaction, &manifest.room_id)?;
        if prepared_hashes != expected {
            return Err(RoomStateError::InvalidPreparedMappings);
        }

        let updated_at_ms = now_ms();
        transaction.execute(
            "INSERT INTO room_profiles (room_id, revision, local_profile_id, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(room_id) DO UPDATE SET
                revision = excluded.revision,
                local_profile_id = excluded.local_profile_id,
                updated_at_ms = excluded.updated_at_ms",
            params![manifest.room_id, revision, local_profile_id, updated_at_ms],
        )?;
        transaction.commit()?;
        Ok(RoomProfileBinding {
            room_id: manifest.room_id.clone(),
            revision: manifest.revision,
            local_profile_id: local_profile_id.to_string(),
            updated_at_ms,
        })
    }

    pub fn room_profile(
        &self,
        room_id: &str,
    ) -> Result<Option<RoomProfileBinding>, RoomStateError> {
        validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
        self.connection
            .lock()
            .query_row(
                "SELECT revision, local_profile_id, updated_at_ms FROM room_profiles WHERE room_id = ?1",
                [room_id],
                |row| {
                    let revision: i64 = row.get(0)?;
                    Ok(RoomProfileBinding {
                        room_id: room_id.to_string(),
                        revision: sqlite_value(0, sql_revision(revision))?,
                        local_profile_id: row.get(1)?,
                        updated_at_ms: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Rebuild the cache retention view from durable database references.
    pub fn cache_references(&self) -> Result<RoomCacheReferences, RoomStateError> {
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT room_id, content_hash FROM cache_references
             UNION
             SELECT room_id, content_hash FROM staged_cache_references
             ORDER BY room_id, content_hash",
        )?;
        let mut rows = statement.query([])?;
        let mut grouped: std::collections::BTreeMap<String, Vec<ContentHash>> =
            std::collections::BTreeMap::new();
        while let Some(row) = rows.next()? {
            let room_id: String = row.get(0)?;
            let raw_hash: String = row.get(1)?;
            let content_hash = ContentHash::parse(raw_hash)
                .map_err(|error| RoomStateError::CorruptData(error.to_string()))?;
            grouped.entry(room_id).or_default().push(content_hash);
        }

        let mut references = RoomCacheReferences::default();
        for (room_id, hashes) in grouped {
            references.set_room_hashes(room_id, hashes);
        }
        Ok(references)
    }

    /// Insert or update resumable progress. Completed transfers are removed with
    /// [`Self::remove_transfer`] after their blob is committed.
    pub fn save_transfer(
        &self,
        mut transfer: PendingTransfer,
    ) -> Result<PendingTransfer, RoomStateError> {
        validate_transfer(&transfer)?;
        transfer.updated_at_ms = now_ms();
        let expected = to_sql_integer("expected transfer size", transfer.expected_size_bytes)?;
        let transferred = to_sql_integer("transferred byte count", transfer.transferred_bytes)?;

        let changed = self.connection.lock().execute(
            "INSERT INTO pending_transfers (
                room_id, content_hash, direction, state, format,
                expected_size_bytes, transferred_bytes, etag, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(room_id, content_hash, direction) DO UPDATE SET
                state = excluded.state,
                transferred_bytes = excluded.transferred_bytes,
                etag = excluded.etag,
                updated_at_ms = excluded.updated_at_ms
             WHERE pending_transfers.expected_size_bytes = excluded.expected_size_bytes
               AND pending_transfers.format = excluded.format",
            params![
                transfer.room_id,
                transfer.content_hash.as_str(),
                transfer.direction.as_str(),
                transfer.state.as_str(),
                transfer.format.as_str(),
                expected,
                transferred,
                transfer.etag,
                transfer.updated_at_ms,
            ],
        )?;
        if changed == 0 {
            return Err(RoomStateError::TransferIdentityConflict);
        }
        Ok(transfer)
    }

    pub fn pending_transfers(&self, room_id: &str) -> Result<Vec<PendingTransfer>, RoomStateError> {
        validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
        let connection = self.connection.lock();
        let mut statement = connection.prepare(
            "SELECT room_id, content_hash, direction, state, format,
                    expected_size_bytes, transferred_bytes, etag, updated_at_ms
             FROM pending_transfers WHERE room_id = ?1
             ORDER BY updated_at_ms, content_hash, direction",
        )?;
        statement
            .query_map([room_id], pending_transfer_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn remove_transfer(
        &self,
        room_id: &str,
        content_hash: &ContentHash,
        direction: TransferDirection,
    ) -> Result<bool, RoomStateError> {
        validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
        Ok(self.connection.lock().execute(
            "DELETE FROM pending_transfers
             WHERE room_id = ?1 AND content_hash = ?2 AND direction = ?3",
            params![room_id, content_hash.as_str(), direction.as_str()],
        )? != 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct JoinedRoom {
    pub room_id: String,
    pub member_id: String,
    pub last_accepted_revision: u64,
    pub joined_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestAcceptance {
    Advanced,
    AlreadyAccepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestStaging {
    Staged,
    AlreadyStaged,
    AlreadyAccepted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct PreparedModMapping {
    pub content_hash: ContentHash,
    pub local_mod_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct PreparedRevision {
    pub room_id: String,
    pub revision: u64,
    pub prepared_at_ms: i64,
    pub mappings: Vec<PreparedModMapping>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct RoomProfileBinding {
    pub room_id: String,
    pub revision: u64,
    pub local_profile_id: String,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct PendingTransfer {
    pub room_id: String,
    pub content_hash: ContentHash,
    pub direction: TransferDirection,
    pub state: TransferState,
    pub format: RoomModFormat,
    pub expected_size_bytes: u64,
    pub transferred_bytes: u64,
    pub etag: Option<String>,
    pub updated_at_ms: i64,
}

impl PendingTransfer {
    pub fn new(
        room_id: impl Into<String>,
        artifact: &CanonicalRoomArtifact,
        direction: TransferDirection,
    ) -> Self {
        Self {
            room_id: room_id.into(),
            content_hash: artifact.content_hash.clone(),
            direction,
            state: TransferState::Queued,
            format: artifact.format,
            expected_size_bytes: artifact.size_bytes,
            transferred_bytes: 0,
            etag: None,
            updated_at_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub enum TransferDirection {
    Download,
    Upload,
}

impl TransferDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Upload => "upload",
        }
    }

    fn parse(value: &str) -> Result<Self, RoomStateError> {
        match value {
            "download" => Ok(Self::Download),
            "upload" => Ok(Self::Upload),
            _ => Err(RoomStateError::CorruptData(format!(
                "unknown transfer direction: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub enum TransferState {
    Queued,
    Transferring,
    Paused,
}

impl TransferState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Transferring => "transferring",
            Self::Paused => "paused",
        }
    }

    fn parse(value: &str) -> Result<Self, RoomStateError> {
        match value {
            "queued" => Ok(Self::Queued),
            "transferring" => Ok(Self::Transferring),
            "paused" => Ok(Self::Paused),
            _ => Err(RoomStateError::CorruptData(format!(
                "unknown transfer state: {value}"
            ))),
        }
    }
}

impl RoomModFormat {
    fn as_str(self) -> &'static str {
        match self {
            Self::Modpkg => "modpkg",
            Self::Fantome => "fantome",
        }
    }

    fn parse_state(value: &str) -> Result<Self, RoomStateError> {
        match value {
            "modpkg" => Ok(Self::Modpkg),
            "fantome" => Ok(Self::Fantome),
            _ => Err(RoomStateError::CorruptData(format!(
                "unknown room mod format: {value}"
            ))),
        }
    }
}

#[derive(Debug, Error)]
pub enum RoomStateError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("room state database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("room state serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Cache(#[from] RoomCacheError),
    #[error("room state schema {found} is newer than supported schema {supported}")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("room ID is invalid")]
    InvalidRoomId,
    #[error("member ID must contain 1 to {MAX_MEMBER_ID_CHARS} safe ASCII characters")]
    InvalidMemberId,
    #[error("room is not joined locally: {0}")]
    RoomNotJoined(String),
    #[error("manifest revision {attempted} is older than accepted revision {current}")]
    StaleRevision { current: u64, attempted: u64 },
    #[error("manifest revision {0} conflicts with different content already accepted")]
    RevisionConflict(u64),
    #[error("staged manifest revision {0} changed before it could be accepted")]
    StagedManifestChanged(u64),
    #[error("room manifest revision {0} has not been accepted locally")]
    ManifestNotAccepted(u64),
    #[error("prepared mod mappings must cover every manifest hash exactly once")]
    InvalidPreparedMappings,
    #[error("local mod ID must contain 1 to {MAX_LOCAL_MOD_ID_CHARS} safe ASCII characters")]
    InvalidLocalModId,
    #[error("a current prepared revision is required before creating a room profile")]
    PreparedRevisionMissing,
    #[error("prepared revision {prepared} does not match requested revision {requested}")]
    PreparedRevisionMismatch { prepared: u64, requested: u64 },
    #[error(
        "local profile ID must contain 1 to {MAX_LOCAL_PROFILE_ID_CHARS} safe ASCII characters"
    )]
    InvalidLocalProfileId,
    #[error("manifest references a blob that is not verified in cache: {0}")]
    MissingCachedBlob(ContentHash),
    #[error("{field} exceeds SQLite's signed integer range")]
    IntegerOutOfRange { field: &'static str },
    #[error("transfer expected size must be nonzero and progress cannot exceed it")]
    InvalidTransferProgress,
    #[error("transfer ETag exceeds {MAX_ETAG_CHARS} characters or contains control characters")]
    InvalidEtag,
    #[error("an existing transfer for this content has a different size or format")]
    TransferIdentityConflict,
    #[error("room state database contains invalid data: {0}")]
    CorruptData(String),
}

fn configure(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA trusted_schema = OFF;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;",
    )
}

fn migrate(connection: &Connection) -> Result<(), RoomStateError> {
    let mut version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > STATE_SCHEMA_VERSION {
        return Err(RoomStateError::SchemaTooNew {
            found: version,
            supported: STATE_SCHEMA_VERSION,
        });
    }
    if version == 0 {
        apply_migration(connection, SCHEMA_V1)?;
        version = 1;
    }
    if version == 1 {
        apply_migration(connection, SCHEMA_V2)?;
        version = 2;
    }
    if version == 2 {
        apply_migration(connection, SCHEMA_V3)?;
        version = 3;
    }
    if version == 3 {
        apply_migration(connection, SCHEMA_V4)?;
    }
    Ok(())
}

fn apply_migration(connection: &Connection, sql: &str) -> Result<(), RoomStateError> {
    connection.execute_batch("BEGIN IMMEDIATE;")?;
    if let Err(error) = connection.execute_batch(sql) {
        let _ = connection.execute_batch("ROLLBACK;");
        return Err(error.into());
    }
    connection.execute_batch("COMMIT;")?;
    Ok(())
}

fn validate_ids(room_id: &str, member_id: &str) -> Result<(), RoomStateError> {
    validate_room_id(room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
    if member_id.is_empty()
        || member_id.chars().count() > MAX_MEMBER_ID_CHARS
        || !member_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(RoomStateError::InvalidMemberId);
    }
    Ok(())
}

fn validate_transfer(transfer: &PendingTransfer) -> Result<(), RoomStateError> {
    validate_room_id(&transfer.room_id).map_err(|_| RoomStateError::InvalidRoomId)?;
    if transfer.expected_size_bytes == 0
        || transfer.transferred_bytes > transfer.expected_size_bytes
    {
        return Err(RoomStateError::InvalidTransferProgress);
    }
    if transfer.etag.as_ref().is_some_and(|etag| {
        etag.chars().count() > MAX_ETAG_CHARS || etag.chars().any(char::is_control)
    }) {
        return Err(RoomStateError::InvalidEtag);
    }
    Ok(())
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn to_sql_integer(field: &'static str, value: u64) -> Result<i64, RoomStateError> {
    i64::try_from(value).map_err(|_| RoomStateError::IntegerOutOfRange { field })
}

fn manifest_hashes(manifest: &RoomManifest) -> HashSet<ContentHash> {
    manifest
        .mods
        .iter()
        .map(|room_mod| room_mod.content_hash.clone())
        .collect()
}

fn validate_prepared_mappings(
    manifest: &RoomManifest,
    mappings: &[PreparedModMapping],
) -> Result<(), RoomStateError> {
    let expected = manifest_hashes(manifest);
    let received: HashSet<_> = mappings
        .iter()
        .map(|mapping| mapping.content_hash.clone())
        .collect();
    if mappings.len() != expected.len() || received != expected {
        return Err(RoomStateError::InvalidPreparedMappings);
    }
    let mut local_ids = HashSet::with_capacity(mappings.len());
    for mapping in mappings {
        if mapping.local_mod_id.is_empty()
            || mapping.local_mod_id.chars().count() > MAX_LOCAL_MOD_ID_CHARS
            || !mapping.local_mod_id.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
        {
            return Err(RoomStateError::InvalidLocalModId);
        }
        if !local_ids.insert(mapping.local_mod_id.as_str()) {
            return Err(RoomStateError::InvalidPreparedMappings);
        }
    }
    Ok(())
}

fn validate_local_profile_id(local_profile_id: &str) -> Result<(), RoomStateError> {
    if local_profile_id.is_empty()
        || local_profile_id.chars().count() > MAX_LOCAL_PROFILE_ID_CHARS
        || !local_profile_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(RoomStateError::InvalidLocalProfileId);
    }
    Ok(())
}

fn prepared_mapping_hashes(
    transaction: &Transaction<'_>,
    room_id: &str,
) -> Result<HashSet<ContentHash>, RoomStateError> {
    let mut statement = transaction.prepare(
        "SELECT content_hash FROM prepared_mod_mappings WHERE room_id = ?1 ORDER BY content_hash",
    )?;
    let hashes = statement
        .query_map([room_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    hashes
        .into_iter()
        .map(|hash| {
            ContentHash::parse(hash).map_err(|error| RoomStateError::CorruptData(error.to_string()))
        })
        .collect()
}

fn sql_revision(value: i64) -> Result<u64, RoomStateError> {
    u64::try_from(value).map_err(|_| RoomStateError::CorruptData("negative revision".into()))
}

fn prepared_mapping_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PreparedModMapping> {
    let raw_hash: String = row.get(0)?;
    Ok(PreparedModMapping {
        content_hash: sqlite_value(0, ContentHash::parse(raw_hash))?,
        local_mod_id: row.get(1)?,
    })
}

fn joined_room_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<JoinedRoom> {
    let revision: i64 = row.get(2)?;
    Ok(JoinedRoom {
        room_id: row.get(0)?,
        member_id: row.get(1)?,
        last_accepted_revision: sql_u64(2, revision)?,
        joined_at_ms: row.get(3)?,
        updated_at_ms: row.get(4)?,
    })
}

fn pending_transfer_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingTransfer> {
    let raw_hash: String = row.get(1)?;
    let raw_direction: String = row.get(2)?;
    let raw_state: String = row.get(3)?;
    let raw_format: String = row.get(4)?;
    let expected: i64 = row.get(5)?;
    let transferred: i64 = row.get(6)?;

    Ok(PendingTransfer {
        room_id: row.get(0)?,
        content_hash: sqlite_value(1, ContentHash::parse(raw_hash))?,
        direction: sqlite_value(2, TransferDirection::parse(&raw_direction))?,
        state: sqlite_value(3, TransferState::parse(&raw_state))?,
        format: sqlite_value(4, RoomModFormat::parse_state(&raw_format))?,
        expected_size_bytes: sql_u64(5, expected)?,
        transferred_bytes: sql_u64(6, transferred)?,
        etag: row.get(7)?,
        updated_at_ms: row.get(8)?,
    })
}

fn sql_u64(index: usize, value: i64) -> rusqlite::Result<u64> {
    sqlite_value(
        index,
        u64::try_from(value).map_err(|_| RoomStateError::CorruptData("negative integer".into())),
    )
}

fn sqlite_value<T, E: std::error::Error + Send + Sync + 'static>(
    index: usize,
    result: Result<T, E>,
) -> rusqlite::Result<T> {
    result.map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn reference_hashes(
    transaction: &Transaction<'_>,
    room_id: &str,
) -> Result<HashSet<ContentHash>, RoomStateError> {
    let mut statement = transaction.prepare(
        "SELECT content_hash FROM cache_references WHERE room_id = ?1 ORDER BY content_hash",
    )?;
    let hashes = statement
        .query_map([room_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    hashes
        .into_iter()
        .map(|hash| {
            ContentHash::parse(hash).map_err(|error| RoomStateError::CorruptData(error.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_sync::{ROOM_MANIFEST_SCHEMA_VERSION, RoomMod};

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

    fn cached(directory: &Path, artifacts: &[(&CanonicalRoomArtifact, &[u8])]) -> RoomCache {
        let library = directory.join("library");
        fs::create_dir_all(&library).unwrap();
        let cache = RoomCache::open(directory.join("cache"), &[library]).unwrap();
        for (artifact, bytes) in artifacts {
            let partial = cache.prepare_partial(&artifact.content_hash).unwrap();
            fs::write(partial, bytes).unwrap();
            cache.commit_partial(artifact).unwrap();
        }
        cache
    }

    #[test]
    fn joined_rooms_persist_without_any_secret_columns() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rooms.sqlite3");
        let store = RoomStateStore::open(&path).unwrap();
        store.join_room("room_a", "member-1").unwrap();
        drop(store);

        let reopened = RoomStateStore::open(&path).unwrap();
        assert_eq!(reopened.rooms().unwrap()[0].member_id, "member-1");

        let connection = Connection::open(path).unwrap();
        for table in [
            "rooms",
            "cache_references",
            "pending_transfers",
            "staged_manifests",
            "staged_cache_references",
            "accepted_manifests",
            "prepared_revisions",
            "prepared_mod_mappings",
            "room_profiles",
        ] {
            let mut statement = connection
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let columns: Vec<String> = statement
                .query_map([], |row| row.get(1))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert!(columns.iter().all(|column| {
                !column.contains("token")
                    && !column.contains("password")
                    && !column.contains("secret")
            }));
        }
    }

    #[test]
    fn manifest_acceptance_is_verified_transactional_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let first = artifact(b"first");
        let second = artifact(b"second");
        let cache = cached(directory.path(), &[(&first, b"first")]);
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();

        assert!(matches!(
            store.accept_verified_manifest(
                &cache,
                &manifest("room_a", 1, &[first.clone(), second.clone()]),
                ManifestLimits::default()
            ),
            Err(RoomStateError::MissingCachedBlob(_))
        ));
        assert_eq!(
            store
                .room("room_a")
                .unwrap()
                .unwrap()
                .last_accepted_revision,
            0
        );

        let accepted = manifest("room_a", 1, std::slice::from_ref(&first));
        assert_eq!(
            store
                .accept_verified_manifest(&cache, &accepted, ManifestLimits::default())
                .unwrap(),
            ManifestAcceptance::Advanced
        );
        assert_eq!(
            store
                .accept_verified_manifest(&cache, &accepted, ManifestLimits::default())
                .unwrap(),
            ManifestAcceptance::AlreadyAccepted
        );
        assert_eq!(store.accepted_manifest("room_a").unwrap(), Some(accepted));
        let mut conflicting_metadata = store.accepted_manifest("room_a").unwrap().unwrap();
        conflicting_metadata.mods[0].display_name = "Different metadata".to_string();
        assert!(matches!(
            store.accept_verified_manifest(
                &cache,
                &conflicting_metadata,
                ManifestLimits::default()
            ),
            Err(RoomStateError::RevisionConflict(1))
        ));
        assert!(
            store
                .cache_references()
                .unwrap()
                .contains(&first.content_hash)
        );
    }

    #[test]
    fn prepared_mapping_must_exactly_cover_the_accepted_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let first = artifact(b"first");
        let second = artifact(b"second");
        let cache = cached(
            directory.path(),
            &[(&first, b"first"), (&second, b"second")],
        );
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();
        let accepted = manifest("room_a", 1, &[first.clone(), second.clone()]);
        store
            .accept_verified_manifest(&cache, &accepted, ManifestLimits::default())
            .unwrap();

        let incomplete = vec![PreparedModMapping {
            content_hash: first.content_hash.clone(),
            local_mod_id: "local-1".to_string(),
        }];
        assert!(matches!(
            store.record_prepared_revision(&accepted, &incomplete, ManifestLimits::default()),
            Err(RoomStateError::InvalidPreparedMappings)
        ));
        assert!(store.prepared_revision("room_a").unwrap().is_none());

        let complete = vec![
            PreparedModMapping {
                content_hash: first.content_hash,
                local_mod_id: "local-1".to_string(),
            },
            PreparedModMapping {
                content_hash: second.content_hash,
                local_mod_id: "local-2".to_string(),
            },
        ];
        let recorded = store
            .record_prepared_revision(&accepted, &complete, ManifestLimits::default())
            .unwrap();
        assert_eq!(recorded.revision, 1);
        assert_eq!(store.prepared_revision("room_a").unwrap(), Some(recorded));
    }

    #[test]
    fn room_profile_binding_requires_the_matching_accepted_prepared_revision() {
        let directory = tempfile::tempdir().unwrap();
        let content = artifact(b"shared");
        let cache = cached(directory.path(), &[(&content, b"shared")]);
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();
        let revision_one = manifest("room_a", 1, std::slice::from_ref(&content));
        store
            .accept_verified_manifest(&cache, &revision_one, ManifestLimits::default())
            .unwrap();

        assert!(matches!(
            store.record_room_profile(&revision_one, "profile-1", ManifestLimits::default()),
            Err(RoomStateError::PreparedRevisionMissing)
        ));
        assert!(store.room_profile("room_a").unwrap().is_none());

        store
            .record_prepared_revision(
                &revision_one,
                &[PreparedModMapping {
                    content_hash: content.content_hash.clone(),
                    local_mod_id: "local-1".to_string(),
                }],
                ManifestLimits::default(),
            )
            .unwrap();
        let binding = store
            .record_room_profile(&revision_one, "profile-1", ManifestLimits::default())
            .unwrap();
        assert_eq!(store.room_profile("room_a").unwrap(), Some(binding));

        let revision_two = manifest("room_a", 2, &[content]);
        store
            .accept_verified_manifest(&cache, &revision_two, ManifestLimits::default())
            .unwrap();
        assert!(matches!(
            store.record_room_profile(&revision_two, "profile-1", ManifestLimits::default()),
            Err(RoomStateError::PreparedRevisionMismatch {
                prepared: 1,
                requested: 2
            })
        ));
        assert_eq!(store.room_profile("room_a").unwrap().unwrap().revision, 1);
    }

    #[test]
    fn revisions_cannot_regress_or_reuse_a_number_for_different_content() {
        let directory = tempfile::tempdir().unwrap();
        let first = artifact(b"first");
        let second = artifact(b"second");
        let cache = cached(
            directory.path(),
            &[(&first, b"first"), (&second, b"second")],
        );
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();
        store
            .accept_verified_manifest(
                &cache,
                &manifest("room_a", 2, std::slice::from_ref(&first)),
                ManifestLimits::default(),
            )
            .unwrap();

        assert!(matches!(
            store.accept_verified_manifest(
                &cache,
                &manifest("room_a", 1, std::slice::from_ref(&second)),
                ManifestLimits::default()
            ),
            Err(RoomStateError::StaleRevision { .. })
        ));
        assert!(matches!(
            store.accept_verified_manifest(
                &cache,
                &manifest("room_a", 2, std::slice::from_ref(&second)),
                ManifestLimits::default()
            ),
            Err(RoomStateError::RevisionConflict(2))
        ));
        assert!(
            store
                .cache_references()
                .unwrap()
                .contains(&first.content_hash)
        );
        assert!(
            !store
                .cache_references()
                .unwrap()
                .contains(&second.content_hash)
        );
    }

    #[test]
    fn leaving_cascades_state_but_does_not_delete_cached_files() {
        let directory = tempfile::tempdir().unwrap();
        let content = artifact(b"cached");
        let cache = cached(directory.path(), &[(&content, b"cached")]);
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();
        store
            .accept_verified_manifest(
                &cache,
                &manifest("room_a", 1, std::slice::from_ref(&content)),
                ManifestLimits::default(),
            )
            .unwrap();
        store
            .save_transfer(PendingTransfer::new(
                "room_a",
                &content,
                TransferDirection::Upload,
            ))
            .unwrap();

        assert!(store.leave_room("room_a").unwrap());
        assert!(store.rooms().unwrap().is_empty());
        assert!(store.pending_transfers("room_a").unwrap().is_empty());
        assert_eq!(store.cache_references().unwrap().room_count(), 0);
        assert!(cache.blob_path(&content.content_hash).exists());
    }

    #[test]
    fn interrupted_transfers_resume_as_queued_after_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rooms.sqlite3");
        let content = artifact(b"transfer");
        let store = RoomStateStore::open(&path).unwrap();
        store.join_room("room_a", "member-1").unwrap();
        let mut transfer = PendingTransfer::new("room_a", &content, TransferDirection::Download);
        transfer.state = TransferState::Transferring;
        transfer.transferred_bytes = 3;
        transfer.etag = Some("\"version-1\"".to_string());
        store.save_transfer(transfer).unwrap();
        drop(store);

        let reopened = RoomStateStore::open(&path).unwrap();
        let transfers = reopened.pending_transfers("room_a").unwrap();
        assert_eq!(transfers.len(), 1);
        assert_eq!(transfers[0].state, TransferState::Queued);
        assert_eq!(transfers[0].transferred_bytes, 3);
        assert_eq!(transfers[0].etag.as_deref(), Some("\"version-1\""));
    }

    #[test]
    fn invalid_transfer_progress_and_identity_changes_are_rejected() {
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();
        let content = artifact(b"transfer");
        let mut invalid = PendingTransfer::new("room_a", &content, TransferDirection::Download);
        invalid.transferred_bytes = invalid.expected_size_bytes + 1;
        assert!(matches!(
            store.save_transfer(invalid),
            Err(RoomStateError::InvalidTransferProgress)
        ));

        store
            .save_transfer(PendingTransfer::new(
                "room_a",
                &content,
                TransferDirection::Download,
            ))
            .unwrap();
        let mut changed = content.clone();
        changed.size_bytes += 1;
        assert!(matches!(
            store.save_transfer(PendingTransfer::new(
                "room_a",
                &changed,
                TransferDirection::Download
            )),
            Err(RoomStateError::TransferIdentityConflict)
        ));
    }

    #[test]
    fn newer_database_schema_is_refused() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA user_version = 999;")
            .unwrap();
        assert!(matches!(
            RoomStateStore::from_connection(PathBuf::from(":memory:"), connection),
            Err(RoomStateError::SchemaTooNew { found: 999, .. })
        ));
    }

    #[test]
    fn version_two_state_migrates_without_inventing_an_accepted_manifest() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(SCHEMA_V1).unwrap();
        connection.execute_batch(SCHEMA_V2).unwrap();
        connection
            .execute(
                "INSERT INTO rooms (
                    room_id, member_id, last_accepted_revision, joined_at_ms, updated_at_ms
                 ) VALUES ('room_a', 'member-1', 1, 0, 0)",
                [],
            )
            .unwrap();

        let store = RoomStateStore::from_connection(PathBuf::from(":memory:"), connection).unwrap();
        let version: i64 = store
            .connection
            .lock()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, STATE_SCHEMA_VERSION);
        assert!(store.accepted_manifest("room_a").unwrap().is_none());
        assert!(store.prepared_revision("room_a").unwrap().is_none());
    }
}
