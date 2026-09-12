//! Explicit creation and update of a dedicated, non-active local room profile.
//!
//! This workflow consumes a fully prepared revision and its local UUID mapping. It never selects
//! the profile, starts the patcher, builds an overlay, or launches the game; those remain existing
//! user-driven actions after the user chooses the profile.

use super::{ManifestLimits, RoomStateError, RoomStateStore};
use crate::config::Config;
use crate::error::AppError;
use crate::mods::{ModLibrary, Profile, RoomProfileModSpec};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomProfileWorkflowResult {
    pub room_id: String,
    pub revision: u64,
    pub profile: Profile,
}

/// Create or update a profile for the current accepted and prepared room revision.
///
/// Calling this function is the explicit local confirmation boundary. The result's profile remains
/// non-active. Its enabled-mod order exactly follows the manifest and its complete per-mod layer
/// map exactly reflects each manifest suggestion.
pub fn create_or_update_room_profile(
    store: &RoomStateStore,
    library: &ModLibrary,
    config: &Config,
    room_id: &str,
    limits: ManifestLimits,
) -> Result<RoomProfileWorkflowResult, RoomProfileWorkflowError> {
    let manifest = store
        .accepted_manifest(room_id)?
        .ok_or_else(|| RoomProfileWorkflowError::NoAcceptedManifest(room_id.to_string()))?;
    manifest.validate(limits).map_err(RoomStateError::from)?;
    let prepared = store
        .prepared_revision(room_id)?
        .ok_or(RoomProfileWorkflowError::RevisionNotPrepared)?;
    if prepared.revision != manifest.revision {
        return Err(RoomProfileWorkflowError::PreparedRevisionMismatch {
            prepared: prepared.revision,
            accepted: manifest.revision,
        });
    }

    let mappings: HashMap<_, _> = prepared
        .mappings
        .into_iter()
        .map(|mapping| (mapping.content_hash, mapping.local_mod_id))
        .collect();
    let mut specs = Vec::with_capacity(manifest.mods.len());
    for room_mod in &manifest.mods {
        let Some(local_mod_id) = mappings.get(&room_mod.content_hash) else {
            return Err(RoomProfileWorkflowError::PreparedMappingMissing);
        };
        specs.push(RoomProfileModSpec {
            local_mod_id: local_mod_id.clone(),
            enabled: room_mod.enabled,
            enabled_layers: room_mod.suggested_layers.clone(),
        });
    }

    let profile = library.upsert_room_profile(config, room_id, &specs)?;
    store.record_room_profile(&manifest, &profile.id, limits)?;
    Ok(RoomProfileWorkflowResult {
        room_id: manifest.room_id,
        revision: manifest.revision,
        profile,
    })
}

#[derive(Debug, Error)]
pub enum RoomProfileWorkflowError {
    #[error(transparent)]
    State(#[from] RoomStateError),
    #[error("room has no complete accepted manifest available for profile creation: {0}")]
    NoAcceptedManifest(String),
    #[error("the accepted room revision has not been prepared in the local library")]
    RevisionNotPrepared,
    #[error("prepared revision {prepared} does not match accepted revision {accepted}")]
    PreparedRevisionMismatch { prepared: u64, accepted: u64 },
    #[error("prepared mappings do not cover the accepted room manifest")]
    PreparedMappingMissing,
    #[error("local room profile operation failed: {0}")]
    Library(#[from] AppError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mods::ProfileOrderMode;
    use crate::mods::test_support::{make_modpkg, make_test_library, mod_project_named};
    use crate::room_sync::{
        CanonicalRoomArtifact, ROOM_MANIFEST_SCHEMA_VERSION, RoomCache, RoomManifest, RoomMod,
        RoomModFormat, prepare_accepted_revision,
    };
    use fs_err as fs;
    use std::io::BufWriter;
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

    fn manifest(revision: u64, artifacts: &[CanonicalRoomArtifact]) -> RoomManifest {
        RoomManifest {
            schema_version: ROOM_MANIFEST_SCHEMA_VERSION,
            room_id: "room_a".to_string(),
            revision,
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

    fn make_layered_modpkg(path: &Path) {
        let source = tempfile::tempdir().unwrap();
        let mut project = mod_project_named("Layered Room Mod");
        project.layers.push(ltk_mod_project::ModProjectLayer {
            name: "accent".to_string(),
            display_name: Some("Accent".to_string()),
            priority: 1,
            ..Default::default()
        });
        for layer in ["base", "accent"] {
            let wad_dir = source
                .path()
                .join("content")
                .join(layer)
                .join("Aatrox.wad.client")
                .join("data");
            fs::create_dir_all(&wad_dir).unwrap();
            fs::write(wad_dir.join("skin0.bin"), layer.as_bytes()).unwrap();
        }
        fs::write(
            source.path().join("mod.config.json"),
            serde_json::to_string_pretty(&project).unwrap(),
        )
        .unwrap();

        let project_dir = camino::Utf8PathBuf::from_path_buf(source.path().to_path_buf()).unwrap();
        let writer = BufWriter::new(fs::File::create(path).unwrap());
        ltk_mod_project::ProjectPacker::new(project, project_dir)
            .pack(ltk_mod_project::modpkg::ModpkgFormat::new(writer))
            .unwrap();
    }

    #[test]
    fn room_profile_is_non_active_pinned_and_updates_from_the_prepared_revision() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("library");
        let source = directory.path().join("source");
        fs::create_dir_all(&source).unwrap();
        let (library, config) = make_test_library(&storage);
        let cache =
            RoomCache::open(directory.path().join("cache"), &[storage.join("mods")]).unwrap();
        let store = RoomStateStore::open(directory.path().join("rooms.sqlite3")).unwrap();
        store.join_room("room_a", "member-1").unwrap();

        let first_archive = source.join("first.modpkg");
        let layered_archive = source.join("layered.modpkg");
        make_modpkg(&first_archive, "First Room Mod");
        make_layered_modpkg(&layered_archive);
        let first = cache_archive(&cache, &first_archive, RoomModFormat::Modpkg);
        let layered = cache_archive(&cache, &layered_archive, RoomModFormat::Modpkg);

        let mut revision_one = manifest(1, &[first.clone(), layered.clone()]);
        revision_one.mods[1].suggested_layers = vec!["accent".to_string()];
        store
            .accept_verified_manifest(&cache, &revision_one, ManifestLimits::default())
            .unwrap();
        let prepared_one = prepare_accepted_revision(
            &store,
            &cache,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        let active_before = library.get_active_profile_info(&config).unwrap();

        let first_profile = create_or_update_room_profile(
            &store,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        let expected_one: Vec<_> = prepared_one
            .mods
            .iter()
            .map(|prepared| prepared.local_mod_id.clone())
            .collect();
        assert_eq!(
            first_profile.profile.order_mode,
            ProfileOrderMode::RoomPinned
        );
        assert_eq!(first_profile.profile.mod_order, expected_one);
        assert_eq!(first_profile.profile.enabled_mods, expected_one);
        assert_eq!(
            library.get_active_profile_info(&config).unwrap().id,
            active_before.id
        );
        let first_id = &prepared_one.mods[0].local_mod_id;
        let layered_id = &prepared_one.mods[1].local_mod_id;
        assert_eq!(
            first_profile.profile.layer_states.get(first_id).unwrap(),
            &std::collections::HashMap::from([("base".to_string(), false)])
        );
        assert_eq!(
            first_profile.profile.layer_states.get(layered_id).unwrap(),
            &std::collections::HashMap::from([
                ("base".to_string(), false),
                ("accent".to_string(), true),
            ])
        );
        assert_eq!(
            store
                .room_profile("room_a")
                .unwrap()
                .unwrap()
                .local_profile_id,
            first_profile.profile.id
        );

        let replay = create_or_update_room_profile(
            &store,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        assert_eq!(replay.profile.id, first_profile.profile.id);

        let mut revision_two = manifest(2, &[layered, first]);
        revision_two.mods[0].suggested_layers = Vec::new();
        revision_two.mods[1].suggested_layers = vec!["base".to_string()];
        revision_two.mods[1].enabled = false;
        store
            .accept_verified_manifest(&cache, &revision_two, ManifestLimits::default())
            .unwrap();
        let prepared_two = prepare_accepted_revision(
            &store,
            &cache,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        let updated = create_or_update_room_profile(
            &store,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        let expected_two: Vec<_> = prepared_two
            .mods
            .iter()
            .map(|prepared| prepared.local_mod_id.clone())
            .collect();
        assert_eq!(updated.profile.id, first_profile.profile.id);
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.profile.mod_order, expected_two);
        assert_eq!(updated.profile.enabled_mods, vec![expected_two[0].clone()]);
        assert_eq!(
            updated.profile.layer_states.get(layered_id).unwrap(),
            &std::collections::HashMap::from([
                ("base".to_string(), false),
                ("accent".to_string(), false),
            ])
        );
        assert_eq!(
            updated.profile.layer_states.get(first_id).unwrap(),
            &std::collections::HashMap::from([("base".to_string(), true)])
        );
        assert_eq!(store.room_profile("room_a").unwrap().unwrap().revision, 2);
        assert_eq!(
            library.get_active_profile_info(&config).unwrap().id,
            active_before.id
        );

        library
            .mutate_index(&config, |_storage, index| {
                index.sync_profile_orders();
                Ok(())
            })
            .unwrap();
        let persisted = library
            .get_profiles(&config)
            .unwrap()
            .into_iter()
            .find(|profile| profile.id == updated.profile.id)
            .unwrap();
        assert_eq!(persisted.mod_order, expected_two);
        assert_eq!(persisted.enabled_mods, vec![expected_two[0].clone()]);
    }

    #[test]
    fn room_profile_requires_preparation_and_never_creates_a_partial_profile() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("library");
        let source = directory.path().join("source.modpkg");
        let (library, config) = make_test_library(&storage);
        let cache =
            RoomCache::open(directory.path().join("cache"), &[storage.join("mods")]).unwrap();
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();

        make_modpkg(&source, "Unprepared Room Mod");
        let artifact = cache_archive(&cache, &source, RoomModFormat::Modpkg);
        let accepted = manifest(1, &[artifact]);
        store
            .accept_verified_manifest(&cache, &accepted, ManifestLimits::default())
            .unwrap();

        assert!(matches!(
            create_or_update_room_profile(
                &store,
                &library,
                &config,
                "room_a",
                ManifestLimits::default()
            ),
            Err(RoomProfileWorkflowError::RevisionNotPrepared)
        ));
        assert!(store.room_profile("room_a").unwrap().is_none());
        assert!(
            library
                .get_profiles(&config)
                .unwrap()
                .iter()
                .all(|profile| profile.enabled_mods.is_empty())
        );
    }

    #[test]
    fn updating_an_active_room_profile_changes_only_library_state() {
        let directory = tempfile::tempdir().unwrap();
        let storage = directory.path().join("library");
        let source = directory.path().join("source.modpkg");
        let (library, config) = make_test_library(&storage);
        let cache =
            RoomCache::open(directory.path().join("cache"), &[storage.join("mods")]).unwrap();
        let store = RoomStateStore::open_in_memory().unwrap();
        store.join_room("room_a", "member-1").unwrap();

        make_modpkg(&source, "Active Room Mod");
        let artifact = cache_archive(&cache, &source, RoomModFormat::Modpkg);
        let revision_one = manifest(1, std::slice::from_ref(&artifact));
        store
            .accept_verified_manifest(&cache, &revision_one, ManifestLimits::default())
            .unwrap();
        prepare_accepted_revision(
            &store,
            &cache,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        let profile = create_or_update_room_profile(
            &store,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap()
        .profile;
        library.switch_profile(&config, profile.id.clone()).unwrap();

        let mut revision_two = manifest(2, &[artifact]);
        revision_two.mods[0].suggested_layers = vec!["base".to_string()];
        store
            .accept_verified_manifest(&cache, &revision_two, ManifestLimits::default())
            .unwrap();
        prepare_accepted_revision(
            &store,
            &cache,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();

        let updated = create_or_update_room_profile(
            &store,
            &library,
            &config,
            "room_a",
            ManifestLimits::default(),
        )
        .unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.profile.id, profile.id);
        assert_eq!(store.room_profile("room_a").unwrap().unwrap().revision, 2);
        assert_eq!(
            library.get_active_profile_info(&config).unwrap().id,
            profile.id
        );
    }
}
