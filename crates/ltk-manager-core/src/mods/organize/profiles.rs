use crate::config::Config;
use crate::error::{AppError, AppResult};
use chrono::Utc;
use fs_err as fs;
use std::collections::HashMap;
use uuid::Uuid;

use crate::mods::ModLibrary;
use crate::mods::archive::metadata::load_mod_project;
use crate::mods::index::{
    LibraryIndex, get_active_profile, get_profile_by_id, resolve_profile_dirs,
};
use crate::mods::types::{Profile, ProfileOrderMode, ProfileSlug};

const ROOM_PROFILE_MARKER: &str = ".room-sync-room-id";

/// A prepared local mod and its exact room-profile state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoomProfileModSpec {
    pub(crate) local_mod_id: String,
    pub(crate) enabled: bool,
    pub(crate) enabled_layers: Vec<String>,
}

impl ModLibrary {
    /// Create or update a dedicated room profile without activating it.
    ///
    /// The profile keeps the room manifest's exact local UUID order, whole-mod enabled state, and
    /// every layer selection. It is marked on disk with the room ID so interrupted state writes
    /// can recover the same local profile instead of creating another one.
    pub(crate) fn upsert_room_profile(
        &self,
        config: &Config,
        room_id: &str,
        mods: &[RoomProfileModSpec],
    ) -> AppResult<Profile> {
        if room_id.is_empty() || room_id.len() > 128 {
            return Err(AppError::ValidationFailed(
                "Room ID must contain 1 to 128 characters".to_string(),
            ));
        }

        self.mutate_index(config, |storage_dir, index| {
            let layer_states = room_layer_states(index, storage_dir, mods)?;
            let mod_order: Vec<String> = mods
                .iter()
                .map(|room_mod| room_mod.local_mod_id.clone())
                .collect();
            let enabled_mods: Vec<String> = mods
                .iter()
                .filter(|room_mod| room_mod.enabled)
                .map(|room_mod| room_mod.local_mod_id.clone())
                .collect();

            let profile = if let Some(position) = find_room_profile(index, storage_dir, room_id) {
                let profile = &mut index.profiles[position];
                profile.order_mode = ProfileOrderMode::RoomPinned;
                profile.mod_order = mod_order.clone();
                profile.enabled_mods = enabled_mods.clone();
                profile.layer_states = layer_states;
                profile.clone()
            } else {
                let (name, slug) = next_room_profile_identity(index, room_id)?;
                let profile = Profile {
                    id: Uuid::new_v4().to_string(),
                    name,
                    slug,
                    enabled_mods,
                    mod_order,
                    layer_states,
                    order_mode: ProfileOrderMode::RoomPinned,
                    created_at: Utc::now(),
                    last_used: Utc::now(),
                };
                index.profiles.push(profile.clone());
                profile
            };

            ensure_room_profile_marker(storage_dir, &profile, room_id)?;
            tracing::info!(
                room_id,
                profile_id = %profile.id,
                "Prepared non-active room profile"
            );
            Ok(profile)
        })
    }

    /// Create a new profile.
    pub fn create_profile(&self, config: &Config, name: String) -> AppResult<Profile> {
        self.mutate_index(config, |storage_dir, index| {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(AppError::Other("Profile name cannot be empty".to_string()));
            }

            if index.profiles.iter().any(|p| p.name == name) {
                return Err(AppError::Other(format!(
                    "Profile '{}' already exists",
                    name
                )));
            }

            let slug = ProfileSlug::from_name(&name).ok_or_else(|| {
                AppError::Other(
                    "Profile name must contain at least one alphanumeric character".to_string(),
                )
            })?;
            if !slug.is_unique_in(index, None) {
                return Err(AppError::Other(format!(
                    "Profile '{}' already exists",
                    name
                )));
            }

            let mod_order: Vec<String> = index.mods.iter().map(|m| m.id.clone()).collect();

            let profile = Profile {
                id: Uuid::new_v4().to_string(),
                name,
                slug,
                enabled_mods: Vec::new(),
                mod_order,
                layer_states: HashMap::new(),
                order_mode: ProfileOrderMode::Library,
                created_at: Utc::now(),
                last_used: Utc::now(),
            };

            let (overlay_dir, cache_dir) = resolve_profile_dirs(storage_dir, &profile.slug);
            fs::create_dir_all(&overlay_dir)?;
            fs::create_dir_all(&cache_dir)?;

            index.profiles.push(profile.clone());

            tracing::info!("Created profile: {} (id={})", profile.name, profile.id);
            Ok(profile)
        })
    }

    /// Delete a profile by ID.
    pub fn delete_profile(&self, config: &Config, profile_id: String) -> AppResult<()> {
        self.mutate_index(config, |storage_dir, index| {
            let profile = get_profile_by_id(index, &profile_id)?;

            if profile.name == "Default" {
                return Err(AppError::Other("Cannot delete Default profile".to_string()));
            }

            if profile_id == index.active_profile_id {
                return Err(AppError::Other(
                    "Cannot delete active profile. Switch to another profile first.".to_string(),
                ));
            }

            let profile_slug = profile.slug.clone();
            index.profiles.retain(|p| p.id != profile_id);

            let profile_dir = storage_dir.join("profiles").join(profile_slug.as_str());
            if profile_dir.exists() {
                fs::remove_dir_all(&profile_dir)?;
                tracing::info!("Deleted profile directory: {}", profile_dir.display());
            }

            tracing::info!("Deleted profile: {}", profile_id);
            Ok(())
        })
    }

    /// Switch to a different profile.
    pub fn switch_profile(&self, config: &Config, profile_id: String) -> AppResult<Profile> {
        self.mutate_index(config, |_storage_dir, index| {
            get_profile_by_id(index, &profile_id)?;
            index.active_profile_id = profile_id.clone();

            let profile = index
                .profiles
                .iter_mut()
                .find(|p| p.id == profile_id)
                .ok_or_else(|| AppError::Other("Profile not found after validation".to_string()))?;

            profile.last_used = Utc::now();
            let result = profile.clone();

            tracing::info!("Switched to profile: {} (id={})", result.name, result.id);
            Ok(result)
        })
    }

    /// Get all profiles.
    pub fn get_profiles(&self, config: &Config) -> AppResult<Vec<Profile>> {
        self.with_index(config, |_storage_dir, index| Ok(index.profiles.clone()))
    }

    /// Rename a profile.
    pub fn rename_profile(
        &self,
        config: &Config,
        profile_id: String,
        new_name: String,
    ) -> AppResult<Profile> {
        self.mutate_index(config, |storage_dir, index| {
            let new_name = new_name.trim().to_string();
            if new_name.is_empty() {
                return Err(AppError::Other("Profile name cannot be empty".to_string()));
            }

            let new_slug = ProfileSlug::from_name(&new_name).ok_or_else(|| {
                AppError::Other(
                    "Profile name must contain at least one alphanumeric character".to_string(),
                )
            })?;

            if index
                .profiles
                .iter()
                .any(|p| p.id != profile_id && p.name == new_name)
            {
                return Err(AppError::Other(format!(
                    "Profile '{}' already exists",
                    new_name
                )));
            }

            if !new_slug.is_unique_in(index, Some(&profile_id)) {
                return Err(AppError::Other(format!(
                    "Profile directory name '{}' conflicts with another profile",
                    new_slug
                )));
            }

            let profile = index
                .profiles
                .iter_mut()
                .find(|p| p.id == profile_id)
                .ok_or_else(|| AppError::Other("Profile not found".to_string()))?;

            if profile.name == "Default" {
                return Err(AppError::Other("Cannot rename Default profile".to_string()));
            }

            // Rename directory on disk if slug changed — done before index update
            // so that if rename fails, the closure returns Err and the index is NOT saved.
            if profile.slug != new_slug {
                let old_dir = storage_dir.join("profiles").join(profile.slug.as_str());
                let new_dir = storage_dir.join("profiles").join(new_slug.as_str());
                if old_dir.exists() {
                    fs::rename(&old_dir, &new_dir)?;
                    tracing::info!(
                        "Renamed profile dir: {} -> {}",
                        old_dir.display(),
                        new_dir.display()
                    );
                }
            }

            profile.name = new_name;
            profile.slug = new_slug;
            let result = profile.clone();

            tracing::info!("Renamed profile {} to: {}", profile_id, result.name);
            Ok(result)
        })
    }

    /// Get the active profile.
    pub fn get_active_profile_info(&self, config: &Config) -> AppResult<Profile> {
        self.with_index(config, |_storage_dir, index| {
            let profile = get_active_profile(index)?;
            Ok(profile.clone())
        })
    }

    /// Collect the installed archive paths and RoomMod representations for a profile.
    ///
    /// Ordinary profiles preserve the historical behavior of sharing only enabled mods. Dedicated
    /// room profiles retain their complete pinned list so a disabled room mod remains shared as a
    /// disabled state rather than being mistaken for a removal.
    pub fn collect_profile_room_artifacts(
        &self,
        config: &Config,
        profile_id: Option<&str>,
    ) -> AppResult<(
        Profile,
        Vec<(std::path::PathBuf, crate::room_sync::RoomMod)>,
    )> {
        self.with_index(config, |storage_dir, index| {
            let profile = match profile_id {
                Some(id) => get_profile_by_id(index, id)?,
                None => get_active_profile(index)?,
            };

            let shared_mod_ids = if profile.order_mode == ProfileOrderMode::RoomPinned {
                &profile.mod_order
            } else {
                &profile.enabled_mods
            };
            let enabled_ids: std::collections::HashSet<&str> =
                profile.enabled_mods.iter().map(String::as_str).collect();

            let mut artifacts = Vec::new();
            for mod_id in shared_mod_ids {
                let entry = index
                    .mods
                    .iter()
                    .find(|entry| entry.id == *mod_id)
                    .ok_or_else(|| AppError::ModNotFound(mod_id.clone()))?;

                let archive_path = entry.archive_path(storage_dir);
                if !archive_path.is_file() {
                    return Err(AppError::InvalidPath(format!(
                        "Shared mod archive is missing and cannot be shared: {mod_id}"
                    )));
                }

                let room_format = match entry.format {
                    crate::mods::ModArchiveFormat::Modpkg => {
                        crate::room_sync::RoomModFormat::Modpkg
                    }
                    crate::mods::ModArchiveFormat::Fantome => {
                        crate::room_sync::RoomModFormat::Fantome
                    }
                    crate::mods::ModArchiveFormat::Unknown => {
                        return Err(AppError::ValidationFailed(format!(
                            "Shared mod has no shareable archive format: {mod_id}"
                        )));
                    }
                };

                let artifact =
                    crate::room_sync::CanonicalRoomArtifact::from_file(&archive_path, room_format)
                        .map_err(|error| {
                            AppError::ValidationFailed(format!(
                                "Shared mod cannot be shared ({mod_id}): {error}"
                            ))
                        })?;

                let installed = crate::mods::archive::metadata::read_installed_mod(
                    entry,
                    false,
                    storage_dir,
                    None,
                );
                let display_name = installed
                    .as_ref()
                    .map(|m| m.display_name.clone())
                    .unwrap_or_else(|_| entry.id.clone());
                let version = installed
                    .as_ref()
                    .map(|m| m.version.clone())
                    .unwrap_or_else(|_| "1.0".to_string());

                let suggested_layers = profile
                    .layer_states
                    .get(&entry.id)
                    .map(|layers| {
                        layers
                            .iter()
                            .filter_map(
                                |(name, enabled)| if *enabled { Some(name.clone()) } else { None },
                            )
                            .collect()
                    })
                    .unwrap_or_default();

                artifacts.push((
                    archive_path,
                    crate::room_sync::RoomMod {
                        content_hash: artifact.content_hash,
                        size_bytes: artifact.size_bytes,
                        format: room_format,
                        display_name,
                        version,
                        enabled: enabled_ids.contains(mod_id.as_str()),
                        suggested_layers,
                    },
                ));
            }

            Ok((profile.clone(), artifacts))
        })
    }
}

fn room_layer_states(
    index: &LibraryIndex,
    storage_dir: &std::path::Path,
    mods: &[RoomProfileModSpec],
) -> AppResult<HashMap<String, HashMap<String, bool>>> {
    let mut states = HashMap::with_capacity(mods.len());
    for room_mod in mods {
        if states.contains_key(&room_mod.local_mod_id) {
            return Err(AppError::ValidationFailed(
                "Room profile cannot contain the same local mod twice".to_string(),
            ));
        }
        let entry = index
            .mods
            .iter()
            .find(|entry| entry.id == room_mod.local_mod_id)
            .ok_or_else(|| AppError::ModNotFound(room_mod.local_mod_id.clone()))?;
        if !entry.is_present(storage_dir) {
            return Err(AppError::InvalidPath(format!(
                "Prepared room mod is missing from the local library: {}",
                room_mod.local_mod_id
            )));
        }

        let project = load_mod_project(&entry.mod_dir(storage_dir))?;
        let available: std::collections::HashSet<&str> = project
            .layers
            .iter()
            .map(|layer| layer.name.as_str())
            .collect();
        let requested: std::collections::HashSet<&str> =
            room_mod.enabled_layers.iter().map(String::as_str).collect();
        if requested.len() != room_mod.enabled_layers.len()
            || !requested.iter().all(|layer| available.contains(layer))
        {
            return Err(AppError::ValidationFailed(format!(
                "Room profile requests an unknown or repeated layer for mod {}",
                room_mod.local_mod_id
            )));
        }

        states.insert(
            room_mod.local_mod_id.clone(),
            project
                .layers
                .iter()
                .map(|layer| (layer.name.clone(), requested.contains(layer.name.as_str())))
                .collect(),
        );
    }
    Ok(states)
}

fn find_room_profile(
    index: &LibraryIndex,
    storage_dir: &std::path::Path,
    room_id: &str,
) -> Option<usize> {
    index.profiles.iter().position(|profile| {
        fs::read_to_string(
            storage_dir
                .join("profiles")
                .join(profile.slug.as_str())
                .join(ROOM_PROFILE_MARKER),
        )
        .is_ok_and(|stored| stored.trim() == room_id)
    })
}

fn next_room_profile_identity(
    index: &LibraryIndex,
    room_id: &str,
) -> AppResult<(String, ProfileSlug)> {
    for suffix in 1..=999_u16 {
        let name = if suffix == 1 {
            format!("Room {room_id}")
        } else {
            format!("Room {room_id} {suffix}")
        };
        let slug = ProfileSlug::from_name(&name).ok_or_else(|| {
            AppError::ValidationFailed("Room profile name is not usable".to_string())
        })?;
        if !index.profiles.iter().any(|profile| profile.name == name)
            && slug.is_unique_in(index, None)
        {
            return Ok((name, slug));
        }
    }
    Err(AppError::ValidationFailed(
        "Unable to allocate a unique room profile name".to_string(),
    ))
}

fn ensure_room_profile_marker(
    storage_dir: &std::path::Path,
    profile: &Profile,
    room_id: &str,
) -> AppResult<()> {
    let (overlay_dir, cache_dir) = resolve_profile_dirs(storage_dir, &profile.slug);
    fs::create_dir_all(&overlay_dir)?;
    fs::create_dir_all(&cache_dir)?;
    let profile_dir = overlay_dir.parent().ok_or_else(|| {
        AppError::Other("Room profile overlay directory has no parent".to_string())
    })?;
    fs::write(profile_dir.join(ROOM_PROFILE_MARKER), room_id)?;
    Ok(())
}
