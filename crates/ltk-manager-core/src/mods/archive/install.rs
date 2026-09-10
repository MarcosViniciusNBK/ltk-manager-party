//! Getting mods into and out of the library.
//!
//! Installing happens in two halves. Staging copies the archive to
//! `mods/.staging-<uuid>.<ext>` and extracts its metadata into
//! `mods/.staging-<uuid>/`, which is the slow part and holds no lock.
//! Registering assigns the slug, renames both into place, and does the index
//! bookkeeping under the index lock. Splitting them is what lets a bulk install
//! stage every file before taking the lock once.
//!
//! Uninstalling reverses both and scrubs the mod from every profile and folder.

use crate::config::Config;
use crate::error::{AppError, AppResult, Utf8PathExt};
use crate::events::{BackendEvent, InstallProgress};
use crate::mods::ModLibrary;
use crate::mods::archive::metadata::{
    extract_fantome_metadata, extract_modpkg_metadata, load_mod_project, read_installed_mod,
};
use crate::mods::index::document::archive_path;
use crate::mods::index::{HarvestSummary, LibraryIndex, LibraryModEntry, ModArchiveFormat};
use crate::mods::slug::{ModSlug, TakenSlugs};
use crate::mods::types::{BulkInstallError, BulkInstallResult, InstalledMod, ROOT_FOLDER_ID};
use chrono::{DateTime, Utc};
use fs_err as fs;
use ltk_wad::PathResolver;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

/// Prefix an in-flight install's directory and archive copy share under `mods/`.
///
/// The leading dot keeps them out of directory discovery, which walks `mods/`
/// looking for mod projects to re-register, and it is what the startup sweep
/// matches on.
pub(crate) const STAGING_PREFIX: &str = ".staging-";
const ROOM_SOURCE_HASH_FILE: &str = ".room-source-sha256";

/// What an install needs that the index does not hold.
pub(crate) struct InstallContext<'a> {
    /// Names for a packed WAD's chunks. Best-effort — with no tables the
    /// chunks land under their hex names, which the overlay reads either way.
    pub(crate) resolver: &'a dyn PathResolver,
}

/// A mod staged beside the index, not yet in it.
#[derive(Debug)]
pub(crate) struct StagedMod {
    id: String,
    installed_at: DateTime<Utc>,
    format: ModArchiveFormat,
    staging_dir: PathBuf,
    /// The copy of the source archive, which is where the content stays.
    staged_archive: PathBuf,
    /// The project's `name`, which the slug is derived from. Not
    /// `display_name`: the directory should not move when a user renames a mod.
    project_name: String,
    /// The file this came from, so a register that fails names something the
    /// user recognizes rather than an empty row.
    source_path: String,
    /// What preserving the mod's names found. `None` for a modpkg.
    harvest: Option<HarvestSummary>,
}

impl StagedMod {
    /// Delete what staging left behind, for a register that could not finish.
    fn discard(&self) {
        if self.staging_dir.exists()
            && let Err(e) = fs::remove_dir_all(&self.staging_dir)
        {
            tracing::warn!(
                "Failed to clean up staging directory {}: {}",
                self.staging_dir.display(),
                e
            );
        }

        if self.staged_archive.exists()
            && let Err(e) = fs::remove_file(&self.staged_archive)
        {
            tracing::warn!(
                "Failed to clean up staged archive {}: {}",
                self.staged_archive.display(),
                e
            );
        }
    }
}

/// What materializing one archive into staging produced.
#[derive(Debug)]
struct StagedContent {
    /// The project's `name`, which the slug is derived from.
    project_name: String,
    /// What preserving the mod's names found. `None` for a modpkg.
    harvest: Option<HarvestSummary>,
}

impl ModLibrary {
    pub fn install_mod_from_package(
        &self,
        config: &Config,
        file_path: &str,
    ) -> AppResult<InstalledMod> {
        let storage_dir = self.storage_dir(config)?;
        let resolver = self.wad_resolver();
        let staged = stage_mod_package(
            &storage_dir,
            file_path,
            &InstallContext {
                resolver: resolver.as_ref(),
            },
        )?;

        self.mutate_index(config, |storage_dir, index| {
            let mut taken = TakenSlugs::collect(index, &storage_dir.join("mods"));
            let (_entry, installed_mod) =
                register_staged_mod(storage_dir, index, staged, &mut taken)?;
            Ok(installed_mod)
        })
    }

    /// Install multiple mods in a single batch operation.
    ///
    /// Stages every archive first, then takes the index lock once to register
    /// them all. Emits `"install-progress"` events per file.
    pub fn install_mods_from_packages(
        &self,
        config: &Config,
        file_paths: &[String],
    ) -> AppResult<BulkInstallResult> {
        if file_paths.is_empty() {
            return Ok(BulkInstallResult {
                installed: Vec::new(),
                failed: Vec::new(),
            });
        }

        let events = Arc::clone(self.events());
        let storage_dir = self.storage_dir(config)?;
        let resolver = self.wad_resolver();
        let context = InstallContext {
            resolver: resolver.as_ref(),
        };

        let total = file_paths.len();
        let mut staged = Vec::new();
        let mut failed = Vec::new();

        for (i, file_path) in file_paths.iter().enumerate() {
            let file_name = file_name_of(file_path);
            events.emit(BackendEvent::InstallProgress(InstallProgress {
                current: i + 1,
                total,
                current_file: file_name.clone(),
            }));

            match stage_mod_package(&storage_dir, file_path, &context) {
                Ok(mod_package) => staged.push(mod_package),
                Err(e) => {
                    tracing::warn!("Failed to install {}: {}", file_path, e);
                    failed.push(BulkInstallError {
                        file_path: file_path.clone(),
                        file_name,
                        message: e.to_string(),
                    });
                }
            }
        }

        let mut installed = Vec::new();
        self.mutate_index(config, |storage_dir, index| {
            let mut taken = TakenSlugs::collect(index, &storage_dir.join("mods"));
            for mod_package in staged {
                let source_path = mod_package.source_path.clone();
                match register_staged_mod(storage_dir, index, mod_package, &mut taken) {
                    Ok((_entry, mod_info)) => installed.push(mod_info),
                    Err(e) => {
                        tracing::warn!("Failed to register {}: {}", source_path, e);
                        failed.push(BulkInstallError {
                            file_name: file_name_of(&source_path),
                            file_path: source_path,
                            message: e.to_string(),
                        });
                    }
                }
            }
            Ok(())
        })?;

        Ok(BulkInstallResult { installed, failed })
    }

    /// Prepare one verified room-cache archive in the library without enabling it anywhere.
    ///
    /// The content hash is stored beside the imported metadata so a retry (including one after a
    /// crash between the library and room-state writes) reuses the local UUID instead of creating a
    /// duplicate. The caller remains responsible for verifying the cache blob before this method.
    pub(crate) fn prepare_cached_room_mod(
        &self,
        config: &Config,
        source_path: &Path,
        format: ModArchiveFormat,
        content_hash: &str,
    ) -> AppResult<PreparedCachedMod> {
        if let Some(local_mod_id) = self.with_index(config, |storage_dir, index| {
            Ok(find_room_source(index, storage_dir, format, content_hash))
        })? {
            return Ok(PreparedCachedMod {
                local_mod_id,
                imported: false,
            });
        }

        let storage_dir = self.storage_dir(config)?;
        let resolver = self.wad_resolver();
        let staged = stage_mod_package_as(
            &storage_dir,
            source_path,
            format,
            &InstallContext {
                resolver: resolver.as_ref(),
            },
        )?;
        let marker_result = (|| -> AppResult<()> {
            let marker = RoomSourceMarker {
                schema_version: 1,
                source_sha256: content_hash.to_string(),
                library_sha256: sha256_file(&staged.staged_archive)?,
                format,
            };
            fs::write(
                staged.staging_dir.join(ROOM_SOURCE_HASH_FILE),
                serde_json::to_vec(&marker)?,
            )?;
            Ok(())
        })();
        if let Err(error) = marker_result {
            staged.discard();
            return Err(error);
        }

        let mut staged = Some(staged);
        let result = self.mutate_index(config, |storage_dir, index| {
            // Staging deliberately happens outside the index lock. Check again under the lock so
            // two simultaneous explicit preparation requests cannot install the same hash twice.
            if let Some(local_mod_id) = find_room_source(index, storage_dir, format, content_hash) {
                staged.take().expect("staged room mod available").discard();
                return Ok(PreparedCachedMod {
                    local_mod_id,
                    imported: false,
                });
            }

            let mut taken = TakenSlugs::collect(index, &storage_dir.join("mods"));
            let (entry, _) = register_staged_mod_with_policy(
                storage_dir,
                index,
                staged.take().expect("staged room mod available"),
                &mut taken,
                RegistrationPolicy::PreparedDisabled,
            )?;
            Ok(PreparedCachedMod {
                local_mod_id: entry.id,
                imported: true,
            })
        });
        if let Some(staged) = staged {
            staged.discard();
        }
        result
    }

    pub fn uninstall_mod_by_id(&self, config: &Config, mod_id: &str) -> AppResult<()> {
        self.mutate_index(config, |storage_dir, index| {
            let Some(pos) = index.mods.iter().position(|m| m.id == mod_id) else {
                return Err(AppError::ModNotFound(mod_id.to_string()));
            };

            let entry = index.mods.remove(pos);

            for folder in &mut index.folders {
                folder.mod_ids.retain(|id| id != mod_id);
            }

            for profile in &mut index.profiles {
                profile.mod_order.retain(|id| id != mod_id);
                profile.enabled_mods.retain(|id| id != mod_id);
                profile.layer_states.remove(mod_id);
            }

            entry.remove_files(storage_dir)
        })
    }
}

/// Stage `file_path` as `mods/.staging-<uuid>.<ext>`, with its metadata beside it.
///
/// Holds no lock, so a bulk install does this per file before taking one.
///
/// # Errors
///
/// Fails when the file is missing or the archive is malformed.
pub(crate) fn stage_mod_package(
    storage_dir: &Path,
    file_path: &str,
    context: &InstallContext<'_>,
) -> AppResult<StagedMod> {
    let file_path = PathBuf::from(file_path);
    if !file_path.exists() {
        return Err(AppError::InvalidPath(file_path.display().to_string()));
    }

    // A fantome is a zip, which is what an archive arriving under a name
    // nothing recognizes most often turns out to be. Guessing modpkg instead
    // would hand the file to the modpkg provider, and a modpkg is not
    // convertible, so nothing afterwards could undo the guess.
    let format = file_path
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(ModArchiveFormat::from_extension)
        .unwrap_or(ModArchiveFormat::Fantome);

    stage_mod_package_as(storage_dir, &file_path, format, context)
}

/// Stage a package whose trustworthy format comes from an already validated room manifest.
///
/// Room-cache paths are content-addressed and intentionally have no user-controlled extension, so
/// their format must not be guessed from the cache filename.
fn stage_mod_package_as(
    storage_dir: &Path,
    file_path: &Path,
    format: ModArchiveFormat,
    context: &InstallContext<'_>,
) -> AppResult<StagedMod> {
    if !file_path.is_file() {
        return Err(AppError::InvalidPath(file_path.display().to_string()));
    }
    if matches!(format, ModArchiveFormat::Unknown) {
        return Err(AppError::ValidationFailed(
            "A room archive must be modpkg or fantome".to_string(),
        ));
    }

    let id = Uuid::new_v4().to_string();
    let mods_dir = storage_dir.join("mods");
    let staging_dir = mods_dir.join(format!("{STAGING_PREFIX}{id}"));
    let staged_archive = mods_dir.join(format!("{STAGING_PREFIX}{id}.{}", format.extension()));
    fs::create_dir_all(&staging_dir)?;

    let staged = stage_into(&staging_dir, &staged_archive, file_path, format, context)
        .inspect_err(|_| {
            let _ = fs::remove_dir_all(&staging_dir);
            let _ = fs::remove_file(&staged_archive);
        })?;

    Ok(StagedMod {
        id,
        installed_at: Utc::now(),
        format,
        staging_dir,
        staged_archive,
        project_name: staged.project_name,
        source_path: file_path.display().to_string(),
        harvest: staged.harvest,
    })
}

/// Copy one archive to `staged_archive` and write its metadata into
/// `staging_dir`.
///
/// The archive is the mod: every install keeps it, and the content provider
/// reads out of it. A fantome's copy is made through the name preserve, so the
/// names its packed WADs carry survive into the library with it.
fn stage_into(
    staging_dir: &Path,
    staged_archive: &Path,
    file_path: &Path,
    format: ModArchiveFormat,
    context: &InstallContext<'_>,
) -> AppResult<StagedContent> {
    match format {
        // `Unknown` never reaches here — it is what a discovered directory
        // records, and nothing installs one.
        ModArchiveFormat::Fantome | ModArchiveFormat::Unknown => {
            let source = file_path.to_path_buf().try_into_utf8("archive path")?;
            let dest = staged_archive
                .to_path_buf()
                .try_into_utf8("staged archive")?;
            let report =
                ltk_mod_project::preserve_archive_names(&source, &dest, Some(context.resolver))
                    .map_err(|e| {
                        AppError::Other(format!("Failed to preserve the mod's names: {e}"))
                    })?;
            tracing::info!(
                archive = %source,
                outcome = ?report.outcome,
                unharvestable = report.unharvestable,
                "Preserved the mod's names on import"
            );

            let outcome = ltk_fantome::normalize_archive(&dest, &dest)
                .map_err(|e| AppError::Other(format!("Failed to normalize the archive: {e}")))?;
            tracing::info!(archive = %dest, outcome = ?outcome, "Normalized the mod's archive");

            extract_fantome_metadata(staged_archive, staging_dir)?;

            Ok(StagedContent {
                project_name: load_mod_project(staging_dir)?.name,
                harvest: Some(report.into()),
            })
        }
        ModArchiveFormat::Modpkg => {
            fs::copy(file_path, staged_archive)?;
            extract_modpkg_metadata(staged_archive, staging_dir)?;

            Ok(StagedContent {
                project_name: load_mod_project(staging_dir)?.name,
                harvest: None,
            })
        }
    }
}

/// Assign a slug, move the staged files into place, and record the mod.
///
/// Runs under the index lock. On any failure everything staging wrote is
/// removed, so a half-registered mod never outlives the call.
///
/// # Errors
///
/// Fails when the staged directory or its archive cannot be moved into place,
/// or when the config cannot be read back.
pub(crate) fn register_staged_mod(
    storage_dir: &Path,
    index: &mut LibraryIndex,
    staged: StagedMod,
    taken: &mut TakenSlugs,
) -> AppResult<(LibraryModEntry, InstalledMod)> {
    register_staged_mod_with_policy(
        storage_dir,
        index,
        staged,
        taken,
        RegistrationPolicy::EnableInActiveProfile,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationPolicy {
    EnableInActiveProfile,
    PreparedDisabled,
}

fn register_staged_mod_with_policy(
    storage_dir: &Path,
    index: &mut LibraryIndex,
    staged: StagedMod,
    taken: &mut TakenSlugs,
    policy: RegistrationPolicy,
) -> AppResult<(LibraryModEntry, InstalledMod)> {
    let slug = ModSlug::assign(&staged.project_name, taken);
    let mod_dir = storage_dir.join("mods").join(slug.as_str());

    if let Err(e) = fs::rename(&staged.staging_dir, &mod_dir) {
        staged.discard();
        return Err(AppError::Io(std::io::Error::new(
            e.kind(),
            format!("Failed to move staged mod into {}: {e}", mod_dir.display()),
        )));
    }

    let destination = archive_path(storage_dir, &slug, staged.format);
    if let Err(e) = fs::rename(&staged.staged_archive, &destination) {
        // A mod without its archive is not a mod: the content provider reads
        // out of it.
        let _ = fs::remove_dir_all(&mod_dir);
        staged.discard();
        return Err(AppError::Io(std::io::Error::new(
            e.kind(),
            format!(
                "Failed to move staged archive into {}: {e}",
                destination.display()
            ),
        )));
    }

    taken.insert(&slug);

    let entry = LibraryModEntry {
        id: staged.id,
        installed_at: staged.installed_at,
        format: staged.format,
        storage: staged.format.installed_storage(),
        slug: Some(slug),
        harvest: staged.harvest,
    };
    let id = entry.id.clone();
    index.mods.push(entry.clone());

    if let Some(root) = index.folders.iter_mut().find(|f| f.id == ROOT_FOLDER_ID) {
        root.mod_ids.insert(0, id.clone());
    }

    let enabled = matches!(policy, RegistrationPolicy::EnableInActiveProfile);
    if enabled {
        let active_profile_id = index.active_profile_id.clone();
        if let Some(profile) = index
            .profiles
            .iter_mut()
            .find(|p| p.id == active_profile_id)
        {
            profile.enabled_mods.insert(0, id.clone());
            profile.mod_order.insert(0, id.clone());
        }
    } else {
        // A prepared mod is visible in every profile's library order, but remains disabled in all
        // of them. The dedicated room-profile workflow decides layer state and enablement later.
        index.sync_profile_orders();
    }

    // A re-installed mod can carry a different layer set, which would otherwise
    // leave states for layers it no longer has.
    if let Ok(project) = load_mod_project(&mod_dir) {
        let new_layer_names: std::collections::HashSet<&str> =
            project.layers.iter().map(|l| l.name.as_str()).collect();
        for profile in &mut index.profiles {
            if let Some(states) = profile.layer_states.get_mut(&id) {
                states.retain(|name, _| new_layer_names.contains(name.as_str()));
            }
        }
    }

    let installed_mod = read_installed_mod(&entry, enabled, storage_dir, None)?;
    Ok((entry, installed_mod))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedCachedMod {
    pub(crate) local_mod_id: String,
    pub(crate) imported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomSourceMarker {
    schema_version: u32,
    source_sha256: String,
    library_sha256: String,
    format: ModArchiveFormat,
}

fn find_room_source(
    index: &LibraryIndex,
    storage_dir: &Path,
    format: ModArchiveFormat,
    content_hash: &str,
) -> Option<String> {
    index.mods.iter().find_map(|entry| {
        if entry.format != format || !entry.is_packed() {
            return None;
        }
        let marker = entry.mod_dir(storage_dir).join(ROOM_SOURCE_HASH_FILE);
        let marker: RoomSourceMarker = fs::read(marker)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())?;
        (marker.schema_version == 1
            && marker.source_sha256 == content_hash
            && marker.format == format
            && sha256_file(&entry.archive_path(storage_dir))
                .is_ok_and(|actual| actual == marker.library_sha256))
        .then(|| entry.id.clone())
    })
}

fn sha256_file(path: &Path) -> AppResult<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Stage and register one mod in a single step, for callers already holding
/// the index lock — reconciliation's drop-folder discovery.
///
/// # Errors
///
/// Whatever [`stage_mod_package`] or [`register_staged_mod`] report.
pub(crate) fn install_single_mod_to_index(
    storage_dir: &Path,
    index: &mut LibraryIndex,
    file_path: &str,
    context: &InstallContext<'_>,
    taken: &mut TakenSlugs,
) -> AppResult<(LibraryModEntry, InstalledMod)> {
    let staged = stage_mod_package(storage_dir, file_path, context)?;
    register_staged_mod(storage_dir, index, staged, taken)
}

fn file_name_of(file_path: &str) -> String {
    Path::new(file_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(file_path)
        .to_string()
}

#[cfg(test)]
mod tests;
