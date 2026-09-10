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

/// A prepared local mod and the exact layer names the room profile enables for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RoomProfileModSpec {
    pub(crate) local_mod_id: String,
    pub(crate) enabled_layers: Vec<String>,
}

impl ModLibrary {
    /// Create or update a dedicated room profile without activating it.
    ///
    /// The profile keeps the room manifest's exact local UUID order, enables only those mods, and
    /// records every layer explicitly. It is marked on disk with the room ID so interrupted state
    /// writes can recover the same local profile instead of creating another one.
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

            let profile = if let Some(position) = find_room_profile(index, storage_dir, room_id) {
                if index.profiles[position].id == index.active_profile_id {
                    return Err(AppError::ValidationFailed(
                        "Select another profile before updating this room profile".to_string(),
                    ));
                }
                let profile = &mut index.profiles[position];
                profile.order_mode = ProfileOrderMode::RoomPinned;
                profile.mod_order = mod_order.clone();
                profile.enabled_mods = mod_order;
                profile.layer_states = layer_states;
                profile.clone()
            } else {
                let (name, slug) = next_room_profile_identity(index, room_id)?;
                let profile = Profile {
                    id: Uuid::new_v4().to_string(),
                    name,
                    slug,
                    enabled_mods: mod_order.clone(),
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
