//! Explicitly initiated preparation of a synchronized revision in the local mod library.
//!
//! Preparation imports verified cache blobs through the existing archive staging pipeline and
//! records the local UUID assigned to each shared hash. It never enables a mod, edits a room
//! profile, builds an overlay, starts the patcher, launches the game, or changes the active profile.

use super::{
    CanonicalRoomArtifact, ContentHash, ManifestLimits, PreparedModMapping, RoomCache,
    RoomCacheError, RoomStateError, RoomStateStore,
};
use crate::config::Config;
use crate::error::AppError;
use crate::mods::{ModArchiveFormat, ModLibrary};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreparedModOutcome {
    Imported,
    Reused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedRoomMod {
    pub content_hash: ContentHash,
    /// UUID generated and owned by this machine's library.
    pub local_mod_id: String,
    pub outcome: PreparedModOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomPreparationResult {
    pub room_id: String,
    pub revision: u64,
    pub mods: Vec<PreparedRoomMod>,
    pub imported_count: usize,
    pub reused_count: usize,
}

/// Prepare the currently accepted room revision after an explicit user action.
///
/// All cache blobs are verified before the first library mutation. Each archive then follows the
/// normal library staging, metadata extraction, normalization, and validation path, but registers
/// disabled. Successful partial work is safe to retry because its content hash marker recovers the
/// same local UUID. The revision is recorded as prepared only after every mapping succeeds.
pub fn prepare_accepted_revision(
    store: &RoomStateStore,
    cache: &RoomCache,
    library: &ModLibrary,
    config: &Config,
    room_id: &str,
    limits: ManifestLimits,
) -> Result<RoomPreparationResult, RoomPreparationError> {
    let manifest = store
        .accepted_manifest(room_id)?
        .ok_or_else(|| RoomPreparationError::NoAcceptedManifest(room_id.to_string()))?;
    manifest.validate(limits).map_err(RoomStateError::from)?;

    // Fail before importing anything if the synchronized revision is incomplete or was corrupted
    // after acceptance.
    for room_mod in &manifest.mods {
        let artifact = CanonicalRoomArtifact {
            content_hash: room_mod.content_hash.clone(),
            size_bytes: room_mod.size_bytes,
            format: room_mod.format,
        };
        if !cache.contains(&artifact)? {
            return Err(RoomPreparationError::MissingVerifiedBlob(
                room_mod.content_hash.clone(),
            ));
        }
    }

    let mut prepared = Vec::with_capacity(manifest.mods.len());
    for room_mod in &manifest.mods {
        let source = cache.blob_path(&room_mod.content_hash);
        let local = library.prepare_cached_room_mod(
            config,
            &source,
            match room_mod.format {
                super::RoomModFormat::Modpkg => ModArchiveFormat::Modpkg,
                super::RoomModFormat::Fantome => ModArchiveFormat::Fantome,
            },
            room_mod.content_hash.as_str(),
        )?;
        prepared.push(PreparedRoomMod {
            content_hash: room_mod.content_hash.clone(),
            local_mod_id: local.local_mod_id,
            outcome: if local.imported {
                PreparedModOutcome::Imported
            } else {
                PreparedModOutcome::Reused
            },
        });
    }

    let mappings: Vec<_> = prepared
        .iter()
        .map(|prepared| PreparedModMapping {
            content_hash: prepared.content_hash.clone(),
            local_mod_id: prepared.local_mod_id.clone(),
        })
        .collect();
    store.record_prepared_revision(&manifest, &mappings, limits)?;

    let imported_count = prepared
        .iter()
        .filter(|prepared| prepared.outcome == PreparedModOutcome::Imported)
        .count();
    Ok(RoomPreparationResult {
        room_id: manifest.room_id,
        revision: manifest.revision,
        reused_count: prepared.len() - imported_count,
        imported_count,
        mods: prepared,
    })
}

#[derive(Debug, Error)]
pub enum RoomPreparationError {
    #[error(transparent)]
    State(#[from] RoomStateError),
    #[error(transparent)]
    Cache(#[from] RoomCacheError),
    #[error("room has no complete accepted manifest available for preparation: {0}")]
    NoAcceptedManifest(String),
    #[error("accepted room revision is missing a verified cache blob: {0}")]
    MissingVerifiedBlob(ContentHash),
    #[error("local library preparation failed: {0}")]
    Library(#[from] AppError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mods::test_support::{make_full_fantome_zip, make_modpkg, make_test_library};
    use crate::room_sync::{ROOM_MANIFEST_SCHEMA_VERSION, RoomManifest, RoomMod, RoomModFormat};
    use fs_err as fs;
    use std::path::Path;

    fn cache_archive(
        cache: &RoomCache,
        source: &Path,
        format: RoomModFormat,
    ) -> CanonicalRoomArtifact {
        let artifact = CanonicalRoomArtifact::from_file(source, format).unwrap();
        let partial = cache.prepare_partial(&artifact.content_hash).unwrap();
        fs::copy(source, partial).unwrap();
        cache.commit_partial(&artifact).unwrap();
        artifact
    }

    fn manifest(artifacts: &[CanonicalRoomArtifact]) -> RoomManifest {
        RoomManifest {
            schema_version: ROOM_MANIFEST_SCHEMA_VERSION,
            room_id: "room_a".to_string(),
            revision: 1,
            game_build: None,
            mods: artifacts
                .iter()
                .enumerate()
                .map(|(index, artifact)| RoomMod {
                    content_hash: artifact.content_hash.clone(),
                    size_bytes: artifact.size_bytes,
                    format: artifact.format,
                    display_name: format!("Room Mod {index}"),
                    version: String::new(),
                    enabled: true,
                    suggested_layers: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn explicit_preparation_imports_real_archives_disabled_and_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("library");
        let source = directory.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let (library, config) = make_test_library(&storage);
        let cache =
            RoomCache::open(directory.path().join("cache"), &[storage.join("mods")]).unwrap();
        let store = RoomStateStore::open(directory.path().join("rooms.sqlite3")).unwrap();
        store.join_room("room_a", "member-1").unwrap();

        let modpkg = source.join("shared.modpkg");
        make_modpkg(&modpkg, "Shared Modpkg");
        let fantome = source.join("shared.fantome");
        make_full_fantome_zip(&fantome);
        let modpkg = cache_archive(&cache, &modpkg, RoomModFormat::Modpkg);
        let fantome = cache_archive(&cache, &fantome, RoomModFormat::Fantome);
        let accepted = manifest(&[modpkg.clone(), fantome.clone()]);
        store
            .accept_verified_manifest(&cache, &accepted, ManifestLimits::default())
            .unwrap();
        let cached_fantome_before = fs::read(cache.blob_path(&fantome.content_hash)).unwrap();

        let first = prepare_accepted_revision(
            &store,
            &cache,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        assert_eq!(first.imported_count, 2);
        assert_eq!(first.reused_count, 0);
        assert!(first.mods.iter().all(|prepared| {
            prepared.local_mod_id != prepared.content_hash.as_str()
                && uuid::Uuid::parse_str(&prepared.local_mod_id).is_ok()
        }));

        let installed = library.get_installed_mods(&config).unwrap();
        assert_eq!(installed.len(), 2);
        assert!(installed.iter().all(|installed| !installed.enabled));
        let profiles = library.get_profiles(&config).unwrap();
        assert!(
            profiles
                .iter()
                .all(|profile| profile.enabled_mods.is_empty())
        );
        assert!(profiles.iter().all(|profile| profile.mod_order.len() == 2));
        assert_eq!(
            fs::read(cache.blob_path(&fantome.content_hash)).unwrap(),
            cached_fantome_before
        );

        let recorded = store.prepared_revision("room_a").unwrap().unwrap();
        assert_eq!(recorded.revision, 1);
        assert_eq!(recorded.mappings.len(), 2);

        let second = prepare_accepted_revision(
            &store,
            &cache,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        assert_eq!(second.imported_count, 0);
        assert_eq!(second.reused_count, 2);
        assert_eq!(
            first
                .mods
                .iter()
                .map(|prepared| &prepared.local_mod_id)
                .collect::<Vec<_>>(),
            second
                .mods
                .iter()
                .map(|prepared| &prepared.local_mod_id)
                .collect::<Vec<_>>()
        );
        assert_eq!(library.get_installed_mods(&config).unwrap().len(), 2);

        // If a prepared library archive was changed later, its marker no longer proves equality.
        // Preparing again installs a fresh disabled copy and updates only this machine's mapping.
        let old_modpkg_id = second
            .mods
            .iter()
            .find(|prepared| prepared.content_hash == modpkg.content_hash)
            .unwrap()
            .local_mod_id
            .clone();
        let installed_modpkg = library
            .get_installed_mods(&config)
            .unwrap()
            .into_iter()
            .find(|installed| installed.id == old_modpkg_id)
            .unwrap();
        fs::write(
            Path::new(&installed_modpkg.mod_dir).with_extension("modpkg"),
            b"locally changed",
        )
        .unwrap();

        let repaired_mapping = prepare_accepted_revision(
            &store,
            &cache,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        assert_eq!(repaired_mapping.imported_count, 1);
        assert_eq!(repaired_mapping.reused_count, 1);
        assert_ne!(
            repaired_mapping
                .mods
                .iter()
                .find(|prepared| prepared.content_hash == modpkg.content_hash)
                .unwrap()
                .local_mod_id,
            old_modpkg_id
        );
        assert!(
            library
                .get_installed_mods(&config)
                .unwrap()
                .iter()
                .all(|installed| !installed.enabled)
        );
    }

    #[test]
    fn preparation_requires_an_accepted_complete_revision_before_library_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("library");
        let (library, config) = make_test_library(&storage);
        let cache =
            RoomCache::open(directory.path().join("cache"), &[storage.join("mods")]).unwrap();
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();

        assert!(matches!(
            prepare_accepted_revision(
                &store,
                &cache,
                &library,
                &config,
                "room_a",
                ManifestLimits::default()
            ),
            Err(RoomPreparationError::NoAcceptedManifest(_))
        ));
        assert!(library.get_installed_mods(&config).unwrap().is_empty());
    }

    #[test]
    fn malformed_archive_never_marks_the_revision_prepared_or_enables_a_mod() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("library");
        let source = directory.path().join("broken.fantome");
        fs::write(&source, b"not a fantome archive").unwrap();
        let (library, config) = make_test_library(&storage);
        let cache =
            RoomCache::open(directory.path().join("cache"), &[storage.join("mods")]).unwrap();
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();
        let artifact = cache_archive(&cache, &source, RoomModFormat::Fantome);
        let accepted = manifest(&[artifact]);
        store
            .accept_verified_manifest(&cache, &accepted, ManifestLimits::default())
            .unwrap();

        assert!(matches!(
            prepare_accepted_revision(
                &store,
                &cache,
                &library,
                &config,
                "room_a",
                ManifestLimits::default()
            ),
            Err(RoomPreparationError::Library(_))
        ));
        assert!(store.prepared_revision("room_a").unwrap().is_none());
        assert!(library.get_installed_mods(&config).unwrap().is_empty());
        assert!(
            library
                .get_profiles(&config)
                .unwrap()
                .iter()
                .all(|profile| profile.enabled_mods.is_empty())
        );
    }
}
