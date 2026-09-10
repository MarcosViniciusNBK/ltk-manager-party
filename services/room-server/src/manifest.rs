//! Room manifest structures and strict integrity validation.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;
pub const MAX_MODS_PER_ROOM: usize = 256;
pub const MAX_MOD_SIZE_BYTES: u64 = 500 * 1024 * 1024; // 500 MB
pub const MAX_TOTAL_SIZE_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB
pub const MAX_LAYERS_PER_MOD: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoomModFormat {
    Modpkg,
    Fantome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomMod {
    pub content_hash: String,
    pub size_bytes: u64,
    pub format: RoomModFormat,
    pub display_name: String,
    pub version: String,
    #[serde(default)]
    pub suggested_layers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomManifest {
    pub schema_version: u32,
    pub room_id: String,
    pub revision: u64,
    pub game_build: Option<String>,
    pub mods: Vec<RoomMod>,
}

impl RoomManifest {
    /// Validates the manifest against size, count, hash and string limits.
    pub fn validate(&self, expected_room_id: &str, expected_revision: u64) -> Result<(), String> {
        if self.schema_version != CURRENT_SCHEMA_VERSION {
            return Err(format!(
                "Unsupported schemaVersion {}: only schemaVersion {} is supported",
                self.schema_version, CURRENT_SCHEMA_VERSION
            ));
        }

        if self.room_id != expected_room_id {
            return Err(format!(
                "Manifest roomId '{}' does not match target room '{}'",
                self.room_id, expected_room_id
            ));
        }

        if self.revision != expected_revision {
            return Err(format!(
                "Manifest revision {} does not match expected revision {}",
                self.revision, expected_revision
            ));
        }

        if self.revision == 0 {
            return Err("Manifest revision must be greater than zero".to_string());
        }

        if let Some(ref gb) = self.game_build {
            if gb.len() > 128 {
                return Err("gameBuild string exceeds 128 characters".to_string());
            }
        }

        if self.mods.len() > MAX_MODS_PER_ROOM {
            return Err(format!(
                "Manifest contains {} mods, exceeding maximum limit of {}",
                self.mods.len(),
                MAX_MODS_PER_ROOM
            ));
        }

        let mut seen_hashes = HashSet::with_capacity(self.mods.len());
        let mut total_size: u64 = 0;

        for (idx, m) in self.mods.iter().enumerate() {
            // Hash check
            let hash = m.content_hash.trim().to_lowercase();
            if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(format!(
                    "Mod at index {} has invalid contentHash: must be 64-char lowercase hex sha256",
                    idx
                ));
            }

            if !seen_hashes.insert(hash) {
                return Err(format!(
                    "Duplicate contentHash '{}' detected at mod index {}",
                    m.content_hash, idx
                ));
            }

            // Size checks
            if m.size_bytes == 0 {
                return Err(format!("Mod at index {} has zero size_bytes", idx));
            }

            if m.size_bytes > MAX_MOD_SIZE_BYTES {
                return Err(format!(
                    "Mod at index {} exceeds single-mod limit ({} > {} bytes)",
                    idx, m.size_bytes, MAX_MOD_SIZE_BYTES
                ));
            }

            total_size = total_size
                .checked_add(m.size_bytes)
                .ok_or_else(|| "Total manifest size exceeded maximum integer range".to_string())?;

            if total_size > MAX_TOTAL_SIZE_BYTES {
                return Err(format!(
                    "Total manifest size exceeds maximum room limit ({} > {} bytes)",
                    total_size, MAX_TOTAL_SIZE_BYTES
                ));
            }

            // Text fields
            let display_name = m.display_name.trim();
            if display_name.is_empty() || display_name.len() > 256 {
                return Err(format!(
                    "Mod at index {} has invalid displayName: must be between 1 and 256 characters",
                    idx
                ));
            }

            if m.version.len() > 128 {
                return Err(format!(
                    "Mod at index {} has version exceeding 128 characters",
                    idx
                ));
            }

            // Suggested layers
            if m.suggested_layers.len() > MAX_LAYERS_PER_MOD {
                return Err(format!(
                    "Mod at index {} has {} layers, exceeding limit of {}",
                    idx,
                    m.suggested_layers.len(),
                    MAX_LAYERS_PER_MOD
                ));
            }

            let mut seen_layers = HashSet::with_capacity(m.suggested_layers.len());
            for layer in &m.suggested_layers {
                let layer_str = layer.trim();
                if layer_str.is_empty() || layer_str.len() > 128 {
                    return Err(format!(
                        "Mod at index {} has layer with invalid length: must be between 1 and 128 characters",
                        idx
                    ));
                }
                if !seen_layers.insert(layer_str) {
                    return Err(format!(
                        "Mod at index {} contains duplicate layer '{}'",
                        idx, layer
                    ));
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_manifest() {
        let manifest = RoomManifest {
            schema_version: 1,
            room_id: "test-room".to_string(),
            revision: 1,
            game_build: Some("14.1.1".to_string()),
            mods: vec![RoomMod {
                content_hash: "a".repeat(64),
                size_bytes: 1024,
                format: RoomModFormat::Modpkg,
                display_name: "Test Mod".to_string(),
                version: "1.0.0".to_string(),
                suggested_layers: vec!["Default".to_string()],
            }],
        };

        assert!(manifest.validate("test-room", 1).is_ok());
    }

    #[test]
    fn test_invalid_hash() {
        let manifest = RoomManifest {
            schema_version: 1,
            room_id: "test-room".to_string(),
            revision: 1,
            game_build: None,
            mods: vec![RoomMod {
                content_hash: "invalid-hash".to_string(),
                size_bytes: 1024,
                format: RoomModFormat::Fantome,
                display_name: "Test Mod".to_string(),
                version: "1.0.0".to_string(),
                suggested_layers: vec![],
            }],
        };

        assert!(manifest.validate("test-room", 1).is_err());
    }

    #[test]
    fn test_duplicate_content_hashes_rejected() {
        let manifest = RoomManifest {
            schema_version: 1,
            room_id: "test-room".to_string(),
            revision: 1,
            game_build: None,
            mods: vec![
                RoomMod {
                    content_hash: "b".repeat(64),
                    size_bytes: 100,
                    format: RoomModFormat::Modpkg,
                    display_name: "Mod 1".to_string(),
                    version: "1.0".to_string(),
                    suggested_layers: vec![],
                },
                RoomMod {
                    content_hash: "b".repeat(64),
                    size_bytes: 200,
                    format: RoomModFormat::Fantome,
                    display_name: "Mod 2".to_string(),
                    version: "1.0".to_string(),
                    suggested_layers: vec![],
                },
            ],
        };

        let res = manifest.validate("test-room", 1);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("Duplicate contentHash"));
    }

    #[test]
    fn test_duplicate_layers_rejected() {
        let manifest = RoomManifest {
            schema_version: 1,
            room_id: "test-room".to_string(),
            revision: 1,
            game_build: None,
            mods: vec![RoomMod {
                content_hash: "c".repeat(64),
                size_bytes: 100,
                format: RoomModFormat::Modpkg,
                display_name: "Mod 1".to_string(),
                version: "1.0".to_string(),
                suggested_layers: vec!["LayerA".to_string(), "LayerA".to_string()],
            }],
        };

        let res = manifest.validate("test-room", 1);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("duplicate layer"));
    }

    #[test]
    fn test_revision_and_room_mismatch() {
        let manifest = RoomManifest {
            schema_version: 1,
            room_id: "room-a".to_string(),
            revision: 2,
            game_build: None,
            mods: vec![],
        };

        assert!(manifest.validate("room-b", 2).is_err());
        assert!(manifest.validate("room-a", 3).is_err());
        assert!(manifest.validate("room-a", 2).is_ok());
    }
}
