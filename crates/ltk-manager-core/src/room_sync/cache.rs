//! Isolated, content-addressed storage for room downloads.
//!
//! This cache owns only synchronized copies. It never points at, moves, or removes files in the
//! installed mod library. Room membership is represented separately by [`RoomCacheReferences`], so
//! releasing one room can make a blob collectible without treating a user's personal library copy
//! as cache data.

use super::{BlobVerificationError, CanonicalRoomArtifact, ContentHash, RoomManifest};
use fs_err as fs;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

const BLOBS_DIRECTORY: &str = "blobs";
const HASH_ALGORITHM_DIRECTORY: &str = "sha256";
const PARTIALS_DIRECTORY: &str = "partials";

/// A cache rooted outside every protected application directory.
///
/// Final blobs use `blobs/sha256/<first two hex characters>/<full hash>`. Their format and display
/// name remain manifest metadata and therefore cannot influence a filesystem path.
#[derive(Debug, Clone)]
pub struct RoomCache {
    root: PathBuf,
    commit_lock: Arc<Mutex<()>>,
}

impl RoomCache {
    /// Create or open an isolated cache and reject overlap with protected roots such as the mod
    /// library.
    ///
    /// Both nesting directions are rejected. This ensures future cache cleanup cannot accidentally
    /// operate inside the library and the library cannot be created inside cache-owned storage.
    pub fn open(
        root: impl AsRef<Path>,
        protected_roots: &[impl AsRef<Path>],
    ) -> Result<Self, RoomCacheError> {
        fs::create_dir_all(root.as_ref())?;
        let root = fs::canonicalize(root.as_ref())?;

        for protected_root in protected_roots {
            let protected_root = resolve_location(protected_root.as_ref())?;
            if root.starts_with(&protected_root) || protected_root.starts_with(&root) {
                return Err(RoomCacheError::ProtectedRootOverlap {
                    cache_root: root,
                    protected_root,
                });
            }
        }

        let cache = Self {
            root,
            commit_lock: Arc::new(Mutex::new(())),
        };
        cache.ensure_layout()?;
        Ok(cache)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stable location where a transfer engine can create or resume a download.
    pub fn partial_path(&self, content_hash: &ContentHash) -> PathBuf {
        self.root
            .join(PARTIALS_DIRECTORY)
            .join(format!("{content_hash}.part"))
    }

    /// Final content-addressed location. No remote filename is ever included in it.
    pub fn blob_path(&self, content_hash: &ContentHash) -> PathBuf {
        self.root
            .join(BLOBS_DIRECTORY)
            .join(HASH_ALGORITHM_DIRECTORY)
            .join(&content_hash.as_str()[..2])
            .join(content_hash.as_str())
    }

    /// Ensure the parent exists and return the deterministic `.part` path for a transfer.
    pub fn prepare_partial(&self, content_hash: &ContentHash) -> Result<PathBuf, RoomCacheError> {
        self.ensure_layout()?;
        Ok(self.partial_path(content_hash))
    }

    /// Verify and atomically promote a completed `.part` file into the blob store.
    ///
    /// An already cached valid blob wins and the redundant partial is discarded. An existing
    /// corrupt blob is never overwritten implicitly.
    pub fn commit_partial(
        &self,
        artifact: &CanonicalRoomArtifact,
    ) -> Result<CacheCommit, RoomCacheError> {
        let partial = self.partial_path(&artifact.content_hash);
        ensure_regular_file(&partial, RoomCacheError::PartialNotRegular)?;
        artifact.verify(&partial)?;

        let _guard = self
            .commit_lock
            .lock()
            .map_err(|_| RoomCacheError::CommitLockPoisoned)?;
        let destination = self.blob_path(&artifact.content_hash);
        let parent = destination
            .parent()
            .expect("content-addressed blob paths always have a parent");
        fs::create_dir_all(parent)?;
        self.ensure_cache_directory(parent)?;

        match fs::symlink_metadata(&destination) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    return Err(RoomCacheError::CachedBlobNotRegular(destination));
                }
                artifact.verify(&destination).map_err(|source| {
                    RoomCacheError::ExistingBlobInvalid {
                        path: destination.clone(),
                        source,
                    }
                })?;
                fs::remove_file(&partial)?;
                Ok(CacheCommit::AlreadyPresent(destination))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // Both paths live under the canonical cache root, so rename stays on one volume.
                // The process-local lock prevents two cache clients from replacing the same hash.
                fs::rename(&partial, &destination)?;
                Ok(CacheCommit::Stored(destination))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Return whether the exact artifact is already present and valid.
    pub fn contains(&self, artifact: &CanonicalRoomArtifact) -> Result<bool, RoomCacheError> {
        let path = self.blob_path(&artifact.content_hash);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                artifact
                    .verify(&path)
                    .map_err(|source| RoomCacheError::ExistingBlobInvalid { path, source })?;
                Ok(true)
            }
            Ok(_) => Err(RoomCacheError::CachedBlobNotRegular(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Remove one interrupted transfer without touching a finalized blob.
    pub fn discard_partial(&self, content_hash: &ContentHash) -> Result<bool, RoomCacheError> {
        let path = self.partial_path(content_hash);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => {
                fs::remove_file(path)?;
                Ok(true)
            }
            Ok(_) => Err(RoomCacheError::PartialNotRegular(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    /// Delete finalized cache blobs that no joined room references.
    ///
    /// Only canonical hash filenames immediately below the cache's shard directories are eligible;
    /// unknown entries and links are ignored rather than followed.
    pub fn prune_unreferenced(
        &self,
        references: &RoomCacheReferences,
    ) -> Result<CachePruneReport, RoomCacheError> {
        let blobs_root = self
            .root
            .join(BLOBS_DIRECTORY)
            .join(HASH_ALGORITHM_DIRECTORY);
        self.ensure_cache_directory(&blobs_root)?;

        let mut report = CachePruneReport::default();
        for shard in fs::read_dir(&blobs_root)? {
            let shard = shard?;
            let shard_type = shard.file_type()?;
            let shard_name = shard.file_name();
            let shard_name = shard_name.to_string_lossy();
            if !shard_type.is_dir() || !is_hash_prefix(&shard_name) {
                continue;
            }

            for entry in fs::read_dir(shard.path())? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let Ok(content_hash) = ContentHash::parse(name) else {
                    continue;
                };
                if &content_hash.as_str()[..2] != shard_name.as_ref()
                    || references.contains(&content_hash)
                {
                    continue;
                }

                let size = entry.metadata()?.len();
                fs::remove_file(entry.path())?;
                report.removed_blobs += 1;
                report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(size);
            }

            // Empty shard removal is housekeeping only; another transfer may have populated it.
            let _ = fs::remove_dir(shard.path());
        }

        Ok(report)
    }

    fn ensure_layout(&self) -> Result<(), RoomCacheError> {
        let blobs = self
            .root
            .join(BLOBS_DIRECTORY)
            .join(HASH_ALGORITHM_DIRECTORY);
        let partials = self.root.join(PARTIALS_DIRECTORY);
        fs::create_dir_all(&blobs)?;
        fs::create_dir_all(&partials)?;
        self.ensure_cache_directory(&blobs)?;
        self.ensure_cache_directory(&partials)?;
        Ok(())
    }

    fn ensure_cache_directory(&self, path: &Path) -> Result<(), RoomCacheError> {
        ensure_directory(path)?;
        let resolved = fs::canonicalize(path)?;
        if resolved.starts_with(&self.root) {
            Ok(())
        } else {
            Err(RoomCacheError::PathEscapesRoot {
                path: path.to_path_buf(),
                resolved,
            })
        }
    }
}

/// Result of publishing a verified partial file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheCommit {
    Stored(PathBuf),
    AlreadyPresent(PathBuf),
}

impl CacheCommit {
    pub fn path(&self) -> &Path {
        match self {
            Self::Stored(path) | Self::AlreadyPresent(path) => path,
        }
    }
}

/// In-memory reference view. Item 5 will persist this same relationship in SQLite.
#[derive(Debug, Clone, Default)]
pub struct RoomCacheReferences {
    by_room: HashMap<String, HashSet<ContentHash>>,
}

impl RoomCacheReferences {
    /// Replace one room's reference set with an already accepted manifest revision.
    pub fn set_manifest(&mut self, manifest: &RoomManifest) {
        self.set_room_hashes(
            manifest.room_id.clone(),
            manifest
                .mods
                .iter()
                .map(|room_mod| room_mod.content_hash.clone()),
        );
    }

    pub(super) fn set_room_hashes(
        &mut self,
        room_id: String,
        hashes: impl IntoIterator<Item = ContentHash>,
    ) {
        self.by_room.insert(room_id, hashes.into_iter().collect());
    }

    /// Stop retaining blobs solely for this room. No file is removed until an explicit prune.
    pub fn release_room(&mut self, room_id: &str) -> bool {
        self.by_room.remove(room_id).is_some()
    }

    pub fn contains(&self, content_hash: &ContentHash) -> bool {
        self.by_room
            .values()
            .any(|hashes| hashes.contains(content_hash))
    }

    pub fn room_count(&self) -> usize {
        self.by_room.len()
    }

    /// Number of distinct immutable blobs retained across every room.
    pub fn blob_count(&self) -> usize {
        self.by_room
            .values()
            .flat_map(HashSet::iter)
            .collect::<HashSet<_>>()
            .len()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct CachePruneReport {
    pub removed_blobs: usize,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Error)]
pub enum RoomCacheError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("room cache root {cache_root} overlaps protected root {protected_root}")]
    ProtectedRootOverlap {
        cache_root: PathBuf,
        protected_root: PathBuf,
    },
    #[error("room cache partial is not a regular file: {0}")]
    PartialNotRegular(PathBuf),
    #[error("room cache blob is not a regular file: {0}")]
    CachedBlobNotRegular(PathBuf),
    #[error("room cache path {path} resolves outside its root to {resolved}")]
    PathEscapesRoot { path: PathBuf, resolved: PathBuf },
    #[error("existing room cache blob is invalid at {path}: {source}")]
    ExistingBlobInvalid {
        path: PathBuf,
        #[source]
        source: BlobVerificationError,
    },
    #[error(transparent)]
    Verification(#[from] BlobVerificationError),
    #[error("room cache commit lock is poisoned")]
    CommitLockPoisoned,
}

fn resolve_location(path: &Path) -> io::Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::path::absolute(path),
        Err(error) => Err(error),
    }
}

fn ensure_regular_file(
    path: &Path,
    error: fn(PathBuf) -> RoomCacheError,
) -> Result<(), RoomCacheError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(error(path.to_path_buf())),
        Err(source) => Err(source.into()),
    }
}

fn ensure_directory(path: &Path) -> Result<(), RoomCacheError> {
    if fs::symlink_metadata(path)?.file_type().is_dir() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "room cache path is not a directory: {}",
            path.display()
        ))
        .into())
    }
}

fn is_hash_prefix(value: &str) -> bool {
    value.len() == 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_sync::{ROOM_MANIFEST_SCHEMA_VERSION, RoomMod, RoomModFormat};

    fn artifact(bytes: &[u8]) -> CanonicalRoomArtifact {
        CanonicalRoomArtifact {
            content_hash: ContentHash::from_reader(bytes).unwrap(),
            size_bytes: bytes.len() as u64,
            format: RoomModFormat::Modpkg,
        }
    }

    fn cache_in(directory: &Path) -> RoomCache {
        let library = directory.join("library");
        fs::create_dir_all(&library).unwrap();
        RoomCache::open(directory.join("room-cache"), &[library]).unwrap()
    }

    fn write_partial(cache: &RoomCache, artifact: &CanonicalRoomArtifact, bytes: &[u8]) {
        let partial = cache.prepare_partial(&artifact.content_hash).unwrap();
        fs::write(partial, bytes).unwrap();
    }

    fn manifest(room_id: &str, artifacts: &[CanonicalRoomArtifact]) -> RoomManifest {
        RoomManifest {
            schema_version: ROOM_MANIFEST_SCHEMA_VERSION,
            room_id: room_id.to_string(),
            revision: 1,
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
                    suggested_layers: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn paths_are_content_addressed_and_ignore_remote_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache_in(directory.path());
        let artifact = artifact(b"room bytes");
        let hash = artifact.content_hash.as_str();

        assert_eq!(
            cache.blob_path(&artifact.content_hash),
            cache
                .root()
                .join("blobs")
                .join("sha256")
                .join(&hash[..2])
                .join(hash)
        );
        assert_eq!(
            cache.partial_path(&artifact.content_hash),
            cache.root().join("partials").join(format!("{hash}.part"))
        );
    }

    #[test]
    fn verified_partial_is_atomically_promoted() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache_in(directory.path());
        let artifact = artifact(b"verified room bytes");
        write_partial(&cache, &artifact, b"verified room bytes");

        let committed = cache.commit_partial(&artifact).unwrap();
        assert!(matches!(committed, CacheCommit::Stored(_)));
        assert!(!cache.partial_path(&artifact.content_hash).exists());
        assert_eq!(fs::read(committed.path()).unwrap(), b"verified room bytes");
        assert!(cache.contains(&artifact).unwrap());
    }

    #[test]
    fn corrupt_partial_is_not_promoted() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache_in(directory.path());
        let artifact = artifact(b"expected bytes");
        write_partial(&cache, &artifact, b"wrong bytes!!");

        assert!(matches!(
            cache.commit_partial(&artifact),
            Err(RoomCacheError::Verification(_))
        ));
        assert!(cache.partial_path(&artifact.content_hash).exists());
        assert!(!cache.blob_path(&artifact.content_hash).exists());
    }

    #[test]
    fn valid_existing_blob_wins_over_a_redundant_partial() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache_in(directory.path());
        let artifact = artifact(b"shared bytes");
        write_partial(&cache, &artifact, b"shared bytes");
        cache.commit_partial(&artifact).unwrap();
        write_partial(&cache, &artifact, b"shared bytes");

        assert!(matches!(
            cache.commit_partial(&artifact).unwrap(),
            CacheCommit::AlreadyPresent(_)
        ));
        assert!(!cache.partial_path(&artifact.content_hash).exists());
    }

    #[test]
    fn invalid_existing_blob_is_never_replaced_implicitly() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache_in(directory.path());
        let artifact = artifact(b"correct bytes");
        let destination = cache.blob_path(&artifact.content_hash);
        fs::create_dir_all(destination.parent().unwrap()).unwrap();
        fs::write(&destination, b"broken bytes!").unwrap();
        write_partial(&cache, &artifact, b"correct bytes");

        assert!(matches!(
            cache.commit_partial(&artifact),
            Err(RoomCacheError::ExistingBlobInvalid { .. })
        ));
        assert_eq!(fs::read(destination).unwrap(), b"broken bytes!");
        assert!(cache.partial_path(&artifact.content_hash).exists());
    }

    #[test]
    fn references_keep_shared_blobs_until_the_last_room_leaves() {
        let directory = tempfile::tempdir().unwrap();
        let cache = cache_in(directory.path());
        let shared = artifact(b"shared");
        let only_first = artifact(b"only first");
        for (artifact, bytes) in [
            (&shared, b"shared".as_slice()),
            (&only_first, b"only first"),
        ] {
            write_partial(&cache, artifact, bytes);
            cache.commit_partial(artifact).unwrap();
        }

        let mut references = RoomCacheReferences::default();
        references.set_manifest(&manifest("first", &[shared.clone(), only_first.clone()]));
        references.set_manifest(&manifest("second", std::slice::from_ref(&shared)));
        references.release_room("first");

        let first_prune = cache.prune_unreferenced(&references).unwrap();
        assert_eq!(first_prune.removed_blobs, 1);
        assert!(cache.blob_path(&shared.content_hash).exists());
        assert!(!cache.blob_path(&only_first.content_hash).exists());

        references.release_room("second");
        let second_prune = cache.prune_unreferenced(&references).unwrap();
        assert_eq!(second_prune.removed_blobs, 1);
        assert!(!cache.blob_path(&shared.content_hash).exists());
    }

    #[test]
    fn pruning_cache_never_touches_the_personal_library() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("library");
        fs::create_dir_all(&library).unwrap();
        let personal_mod = library.join("personal.modpkg");
        fs::write(&personal_mod, b"my personal copy").unwrap();
        let cache = RoomCache::open(directory.path().join("room-cache"), &[&library]).unwrap();
        let artifact = artifact(b"my personal copy");
        write_partial(&cache, &artifact, b"my personal copy");
        cache.commit_partial(&artifact).unwrap();

        let report = cache
            .prune_unreferenced(&RoomCacheReferences::default())
            .unwrap();
        assert_eq!(report.removed_blobs, 1);
        assert_eq!(fs::read(personal_mod).unwrap(), b"my personal copy");
    }

    #[test]
    fn cache_and_protected_roots_must_not_overlap() {
        let directory = tempfile::tempdir().unwrap();
        let library = directory.path().join("library");
        fs::create_dir_all(&library).unwrap();

        assert!(matches!(
            RoomCache::open(library.join("room-cache"), &[&library]),
            Err(RoomCacheError::ProtectedRootOverlap { .. })
        ));
        assert!(matches!(
            RoomCache::open(directory.path().join("cache-parent"), &[directory.path()]),
            Err(RoomCacheError::ProtectedRootOverlap { .. })
        ));
    }
}
