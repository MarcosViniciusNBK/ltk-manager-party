//! Domain types and integrity checks for room-synchronized mod files.
//!
//! A room manifest is deliberately detached from the mod library and patcher. It describes immutable
//! blobs plus a suggested order/layer selection; accepting one does not install, enable, or apply a
//! mod. The Tauri shell and any future server can share this contract without either gaining a path
//! to game-changing operations.

mod cache;
mod client_state;
mod credentials;
mod preparation;
mod profile;
mod state;
mod transfer;

pub use cache::{CacheCommit, CachePruneReport, RoomCache, RoomCacheError, RoomCacheReferences};
pub use client_state::{
    RoomClientError, RoomSyncBlockReason, RoomSyncPhase, RoomSyncSession, RoomSyncSnapshot,
};
pub use credentials::{CredentialError, RoomCredentialVault, RoomSecret, RoomSecretKind};
pub use preparation::{
    PreparedModOutcome, PreparedRoomMod, RoomPreparationError, RoomPreparationResult,
    prepare_accepted_revision,
};
pub use profile::{
    RoomProfileWorkflowError, RoomProfileWorkflowResult, create_or_update_room_profile,
};
pub use state::{
    JoinedRoom, ManifestAcceptance, ManifestStaging, PendingTransfer, PreparedModMapping,
    PreparedRevision, RoomProfileBinding, RoomStateError, RoomStateStore, TransferDirection,
    TransferState,
};
pub use transfer::{
    HttpTransferEngine, RetryPolicy, TransferCancellation, TransferContext, TransferEndpoint,
    TransferEngineError, TransferOutcome, TransferProgress, TransferProgressCallback,
};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::io::{self, Read};
use std::path::Path;
use thiserror::Error;

use fs_err as fs;

/// The only manifest schema this build understands.
pub const ROOM_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// SHA-256 rendered as exactly 64 lowercase hexadecimal characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct ContentHash(String);

impl ContentHash {
    /// Hash all bytes read from `reader`.
    pub fn from_reader(mut reader: impl Read) -> io::Result<Self> {
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];

        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }

        Ok(Self::from_digest(hasher.finalize().into()))
    }

    /// Hash a file without loading it all into memory.
    pub fn from_file(path: &Path) -> io::Result<Self> {
        Self::from_reader(fs::File::open(path)?)
    }

    /// Parse a canonical SHA-256 string.
    pub fn parse(value: impl Into<String>) -> Result<Self, ContentHashError> {
        let value = value.into();
        if value.len() != 64 {
            return Err(ContentHashError::Length {
                actual: value.len(),
            });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ContentHashError::Encoding);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_digest(digest: [u8; 32]) -> Self {
        let mut value = String::with_capacity(64);
        for byte in digest {
            use fmt::Write as _;
            write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
        }
        Self(value)
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl TryFrom<String> for ContentHash {
    type Error = ContentHashError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContentHashError {
    #[error("a SHA-256 hash must contain 64 characters, got {actual}")]
    Length { actual: usize },
    #[error("a SHA-256 hash must contain only lowercase hexadecimal characters")]
    Encoding,
}

/// Formats accepted as immutable room blobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub enum RoomModFormat {
    Modpkg,
    Fantome,
}

impl RoomModFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Modpkg => "modpkg",
            Self::Fantome => "fantome",
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "modpkg" => Some(Self::Modpkg),
            "fantome" => Some(Self::Fantome),
            _ => None,
        }
    }
}

/// Fingerprint of the exact installed archive bytes a publisher offers to a room.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct CanonicalRoomArtifact {
    pub content_hash: ContentHash,
    pub size_bytes: u64,
    pub format: RoomModFormat,
}

impl CanonicalRoomArtifact {
    /// Inspect an installed archive without rewriting, normalizing, or importing it.
    pub fn from_file(
        path: &Path,
        declared_format: RoomModFormat,
    ) -> Result<Self, CanonicalArtifactError> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(CanonicalArtifactError::NotAFile);
        }
        if metadata.len() == 0 {
            return Err(CanonicalArtifactError::Empty);
        }

        let extension_format =
            RoomModFormat::from_path(path).ok_or(CanonicalArtifactError::UnsupportedExtension)?;
        if extension_format != declared_format {
            return Err(CanonicalArtifactError::FormatMismatch {
                declared: declared_format,
                extension: extension_format,
            });
        }

        Ok(Self {
            content_hash: ContentHash::from_file(path)?,
            size_bytes: metadata.len(),
            format: declared_format,
        })
    }

    /// Re-verify the immutable cached blob represented by this fingerprint.
    pub fn verify(&self, path: &Path) -> Result<(), BlobVerificationError> {
        verify_blob(path, self.size_bytes, &self.content_hash)
    }
}

#[derive(Debug, Error)]
pub enum CanonicalArtifactError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("room artifact path is not a regular file")]
    NotAFile,
    #[error("room artifact is empty")]
    Empty,
    #[error("room artifacts must use the .modpkg or .fantome extension")]
    UnsupportedExtension,
    #[error(
        "declared room artifact format {declared:?} does not match extension format {extension:?}"
    )]
    FormatMismatch {
        declared: RoomModFormat,
        extension: RoomModFormat,
    },
}

/// One immutable file referenced by a room revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct RoomMod {
    pub content_hash: ContentHash,
    pub size_bytes: u64,
    pub format: RoomModFormat,
    /// Display-only metadata; it is never used as a filesystem path.
    pub display_name: String,
    /// Display-only source version.
    pub version: String,
    /// A suggestion for an explicit future local import action, not an instruction to apply it.
    #[serde(default)]
    pub suggested_layers: Vec<String>,
}

/// Immutable description of the exact files in one room revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct RoomManifest {
    pub schema_version: u32,
    pub room_id: String,
    pub revision: u64,
    /// Informational compatibility value. Synchronization itself does not inspect the game.
    pub game_build: Option<String>,
    /// Manifest order is the suggested future profile priority order.
    pub mods: Vec<RoomMod>,
}

impl RoomManifest {
    /// Validate data received across a trust boundary without touching disk or application state.
    pub fn validate(&self, limits: ManifestLimits) -> Result<ManifestSummary, ManifestError> {
        if self.schema_version != ROOM_MANIFEST_SCHEMA_VERSION {
            return Err(ManifestError::UnsupportedSchema {
                found: self.schema_version,
                supported: ROOM_MANIFEST_SCHEMA_VERSION,
            });
        }
        if self.revision == 0 {
            return Err(ManifestError::ZeroRevision);
        }
        validate_room_id(&self.room_id)?;
        validate_text(
            "game build",
            self.game_build.as_deref().unwrap_or_default(),
            128,
            true,
        )?;

        if self.mods.len() > limits.max_mods {
            return Err(ManifestError::TooManyMods {
                actual: self.mods.len(),
                maximum: limits.max_mods,
            });
        }

        let mut hashes = HashSet::with_capacity(self.mods.len());
        let mut total_size_bytes = 0_u64;
        for (index, room_mod) in self.mods.iter().enumerate() {
            if !hashes.insert(room_mod.content_hash.clone()) {
                return Err(ManifestError::DuplicateContent {
                    hash: room_mod.content_hash.clone(),
                });
            }
            if room_mod.size_bytes == 0 {
                return Err(ManifestError::EmptyMod { index });
            }
            if room_mod.size_bytes > limits.max_mod_size_bytes {
                return Err(ManifestError::ModTooLarge {
                    index,
                    actual: room_mod.size_bytes,
                    maximum: limits.max_mod_size_bytes,
                });
            }
            total_size_bytes = total_size_bytes
                .checked_add(room_mod.size_bytes)
                .ok_or(ManifestError::TotalSizeOverflow)?;
            if total_size_bytes > limits.max_total_size_bytes {
                return Err(ManifestError::RoomTooLarge {
                    actual: total_size_bytes,
                    maximum: limits.max_total_size_bytes,
                });
            }

            validate_text("display name", &room_mod.display_name, 256, false)?;
            validate_text("version", &room_mod.version, 128, true)?;
            if room_mod.suggested_layers.len() > limits.max_layers_per_mod {
                return Err(ManifestError::TooManyLayers {
                    index,
                    actual: room_mod.suggested_layers.len(),
                    maximum: limits.max_layers_per_mod,
                });
            }

            let mut layers = HashSet::with_capacity(room_mod.suggested_layers.len());
            for layer in &room_mod.suggested_layers {
                validate_text("layer", layer, 128, false)?;
                if !layers.insert(layer) {
                    return Err(ManifestError::DuplicateLayer {
                        index,
                        layer: layer.clone(),
                    });
                }
            }
        }

        Ok(ManifestSummary {
            mod_count: self.mods.len(),
            total_size_bytes,
        })
    }
}

/// Resource limits are supplied by the caller so private and public deployments can differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestLimits {
    pub max_mods: usize,
    pub max_mod_size_bytes: u64,
    pub max_total_size_bytes: u64,
    pub max_layers_per_mod: usize,
}

impl Default for ManifestLimits {
    fn default() -> Self {
        Self {
            max_mods: 256,
            max_mod_size_bytes: 1024 * 1024 * 1024,
            max_total_size_bytes: 4 * 1024 * 1024 * 1024,
            max_layers_per_mod: 64,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestSummary {
    pub mod_count: usize,
    pub total_size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ManifestError {
    #[error("unsupported room manifest schema {found}; this build supports {supported}")]
    UnsupportedSchema { found: u32, supported: u32 },
    #[error("room manifest revision must be greater than zero")]
    ZeroRevision,
    #[error("room ID must contain 1 to 64 ASCII letters, digits, '-' or '_'")]
    InvalidRoomId,
    #[error("manifest contains {actual} mods; maximum is {maximum}")]
    TooManyMods { actual: usize, maximum: usize },
    #[error("mod at index {index} is empty")]
    EmptyMod { index: usize },
    #[error("mod at index {index} is {actual} bytes; maximum is {maximum}")]
    ModTooLarge {
        index: usize,
        actual: u64,
        maximum: u64,
    },
    #[error("room content is {actual} bytes; maximum is {maximum}")]
    RoomTooLarge { actual: u64, maximum: u64 },
    #[error("room content size overflowed u64")]
    TotalSizeOverflow,
    #[error("content hash appears more than once: {hash}")]
    DuplicateContent { hash: ContentHash },
    #[error("{field} must be at most {maximum} characters and contain no control characters")]
    InvalidText { field: &'static str, maximum: usize },
    #[error("mod at index {index} contains {actual} layers; maximum is {maximum}")]
    TooManyLayers {
        index: usize,
        actual: usize,
        maximum: usize,
    },
    #[error("mod at index {index} repeats layer '{layer}'")]
    DuplicateLayer { index: usize, layer: String },
}

/// Verify a completed blob before an atomic move into the room cache.
pub fn verify_blob(
    path: &Path,
    expected_size_bytes: u64,
    expected_hash: &ContentHash,
) -> Result<(), BlobVerificationError> {
    let actual_size_bytes = fs::metadata(path)?.len();
    if actual_size_bytes != expected_size_bytes {
        return Err(BlobVerificationError::SizeMismatch {
            expected: expected_size_bytes,
            actual: actual_size_bytes,
        });
    }

    let actual_hash = ContentHash::from_file(path)?;
    if &actual_hash != expected_hash {
        return Err(BlobVerificationError::HashMismatch {
            expected: expected_hash.clone(),
            actual: actual_hash,
        });
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum BlobVerificationError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("blob size mismatch: expected {expected} bytes, got {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("blob hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        expected: ContentHash,
        actual: ContentHash,
    },
}

fn validate_room_id(room_id: &str) -> Result<(), ManifestError> {
    if room_id.is_empty()
        || room_id.len() > 64
        || !room_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ManifestError::InvalidRoomId);
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    maximum: usize,
    allow_empty: bool,
) -> Result<(), ManifestError> {
    let length = value.chars().count();
    if (!allow_empty && length == 0) || length > maximum || value.chars().any(char::is_control) {
        return Err(ManifestError::InvalidText { field, maximum });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn hash(byte: u8) -> ContentHash {
        ContentHash::parse(format!("{byte:02x}").repeat(32)).unwrap()
    }

    fn room_mod(byte: u8, size_bytes: u64) -> RoomMod {
        RoomMod {
            content_hash: hash(byte),
            size_bytes,
            format: RoomModFormat::Modpkg,
            display_name: format!("Mod {byte}"),
            version: "1.0.0".to_string(),
            suggested_layers: vec!["base".to_string()],
        }
    }

    fn manifest(mods: Vec<RoomMod>) -> RoomManifest {
        RoomManifest {
            schema_version: ROOM_MANIFEST_SCHEMA_VERSION,
            room_id: "room_123".to_string(),
            revision: 1,
            game_build: Some("16.18".to_string()),
            mods,
        }
    }

    #[test]
    fn hashes_bytes_as_canonical_lowercase_sha256() {
        let content_hash = ContentHash::from_reader(Cursor::new(b"hello world")).unwrap();
        assert_eq!(
            content_hash.as_str(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn content_hash_deserialization_rejects_noncanonical_values() {
        let uppercase = format!("\"{}\"", "AB".repeat(32));
        assert!(serde_json::from_str::<ContentHash>(&uppercase).is_err());
        assert!(serde_json::from_str::<ContentHash>("\"abcd\"").is_err());
    }

    #[test]
    fn valid_manifest_reports_count_and_total_size() {
        let summary = manifest(vec![room_mod(1, 10), room_mod(2, 20)])
            .validate(ManifestLimits::default())
            .unwrap();
        assert_eq!(summary.mod_count, 2);
        assert_eq!(summary.total_size_bytes, 30);
    }

    #[test]
    fn manifest_rejects_unsupported_schema_and_zero_revision() {
        let mut value = manifest(Vec::new());
        value.schema_version = ROOM_MANIFEST_SCHEMA_VERSION + 1;
        assert!(matches!(
            value.validate(ManifestLimits::default()),
            Err(ManifestError::UnsupportedSchema { .. })
        ));

        value.schema_version = ROOM_MANIFEST_SCHEMA_VERSION;
        value.revision = 0;
        assert_eq!(
            value.validate(ManifestLimits::default()),
            Err(ManifestError::ZeroRevision)
        );
    }

    #[test]
    fn manifest_rejects_duplicate_content_and_layers() {
        let first = room_mod(1, 10);
        let duplicate = room_mod(1, 20);
        assert!(matches!(
            manifest(vec![first, duplicate]).validate(ManifestLimits::default()),
            Err(ManifestError::DuplicateContent { .. })
        ));

        let mut repeated_layer = room_mod(1, 10);
        repeated_layer.suggested_layers.push("base".to_string());
        assert!(matches!(
            manifest(vec![repeated_layer]).validate(ManifestLimits::default()),
            Err(ManifestError::DuplicateLayer { .. })
        ));
    }

    #[test]
    fn manifest_enforces_individual_and_total_size_limits() {
        let limits = ManifestLimits {
            max_mod_size_bytes: 10,
            max_total_size_bytes: 15,
            ..ManifestLimits::default()
        };
        assert!(matches!(
            manifest(vec![room_mod(1, 11)]).validate(limits),
            Err(ManifestError::ModTooLarge { .. })
        ));
        assert!(matches!(
            manifest(vec![room_mod(1, 8), room_mod(2, 8)]).validate(limits),
            Err(ManifestError::RoomTooLarge { .. })
        ));
    }

    #[test]
    fn manifest_rejects_path_like_control_characters_in_display_metadata() {
        let mut value = room_mod(1, 10);
        value.display_name = "bad\nname".to_string();
        assert!(matches!(
            manifest(vec![value]).validate(ManifestLimits::default()),
            Err(ManifestError::InvalidText {
                field: "display name",
                ..
            })
        ));
    }

    #[test]
    fn verify_blob_checks_size_before_hash() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("blob.modpkg");
        fs::write(&path, b"hello world").unwrap();
        let expected = ContentHash::from_reader(Cursor::new(b"hello world")).unwrap();

        verify_blob(&path, 11, &expected).unwrap();
        assert!(matches!(
            verify_blob(&path, 12, &expected),
            Err(BlobVerificationError::SizeMismatch { .. })
        ));
        assert!(matches!(
            verify_blob(&path, 11, &hash(1)),
            Err(BlobVerificationError::HashMismatch { .. })
        ));
    }

    #[test]
    fn manifest_round_trip_preserves_validated_content_hashes() {
        let original = manifest(vec![room_mod(1, 10)]);
        let json = serde_json::to_string(&original).unwrap();
        let decoded: RoomManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, original);
        decoded.validate(ManifestLimits::default()).unwrap();
    }

    #[test]
    fn canonical_artifact_fingerprints_exact_archive_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.modpkg");
        fs::write(&path, b"canonical bytes").unwrap();

        let artifact = CanonicalRoomArtifact::from_file(&path, RoomModFormat::Modpkg).unwrap();

        assert_eq!(artifact.size_bytes, 15);
        assert_eq!(artifact.format, RoomModFormat::Modpkg);
        artifact.verify(&path).unwrap();
    }

    #[test]
    fn canonical_artifact_rejects_extension_mismatch_and_empty_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.fantome");
        fs::write(&path, b"bytes").unwrap();

        assert!(matches!(
            CanonicalRoomArtifact::from_file(&path, RoomModFormat::Modpkg),
            Err(CanonicalArtifactError::FormatMismatch { .. })
        ));

        let empty = directory.path().join("empty.modpkg");
        fs::write(&empty, []).unwrap();
        assert!(matches!(
            CanonicalRoomArtifact::from_file(&empty, RoomModFormat::Modpkg),
            Err(CanonicalArtifactError::Empty)
        ));
    }

    #[test]
    fn changing_one_byte_creates_a_different_canonical_artifact() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.modpkg");
        let second = directory.path().join("second.modpkg");
        fs::write(&first, b"archive-a").unwrap();
        fs::write(&second, b"archive-b").unwrap();

        let first = CanonicalRoomArtifact::from_file(&first, RoomModFormat::Modpkg).unwrap();
        let second = CanonicalRoomArtifact::from_file(&second, RoomModFormat::Modpkg).unwrap();

        assert_ne!(first.content_hash, second.content_hash);
        assert_eq!(first.size_bytes, second.size_bytes);
    }
}
