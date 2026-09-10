//! Content-Addressed Storage (CAS) engine and cryptographic grant generation.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

type HmacSha256 = Hmac<Sha256>;

pub const DEFAULT_ROOM_QUOTA_BYTES: u64 = 5 * 1024 * 1024 * 1024; // 5 GB
pub const MAX_BLOB_SIZE_BYTES: u64 = 500 * 1024 * 1024; // 500 MB
pub const UPLOAD_GRANT_TTL_SECS: u64 = 30 * 60; // 30 minutes
pub const DOWNLOAD_GRANT_TTL_SECS: u64 = 60 * 60; // 60 minutes

#[derive(Clone)]
pub struct StorageManager {
    base_dir: PathBuf,
    secret_key: Vec<u8>,
    public_url: String,
    room_quota_bytes: u64,
}

impl StorageManager {
    pub fn new(base_dir: impl AsRef<Path>, secret_key: Vec<u8>, public_url: String) -> io::Result<Self> {
        let base_dir = base_dir.as_ref().to_path_buf();
        let objects_dir = base_dir.join("objects");
        let partial_dir = base_dir.join("partial");

        fs::create_dir_all(&objects_dir)?;
        fs::create_dir_all(&partial_dir)?;

        Ok(Self {
            base_dir,
            secret_key,
            public_url: public_url.trim_end_matches('/').to_string(),
            room_quota_bytes: DEFAULT_ROOM_QUOTA_BYTES,
        })
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.base_dir.join("objects")
    }

    pub fn partial_dir(&self) -> PathBuf {
        self.base_dir.join("partial")
    }

    /// Validates that a hash is strictly 64 lowercase hexadecimal ASCII characters.
    pub fn is_safe_hash(content_hash: &str) -> bool {
        let clean = content_hash.trim();
        clean.len() == 64 && clean.chars().all(|c| c.is_ascii_hexdigit())
    }

    pub fn blob_path(&self, content_hash: &str) -> PathBuf {
        let clean = content_hash.trim().to_lowercase();
        let prefix = if clean.len() >= 2 {
            &clean[..2]
        } else {
            "default"
        };
        let target = self.objects_dir().join(prefix).join(&clean);
        // Traversal guard
        if !target.starts_with(self.objects_dir()) {
            panic!("Path traversal attempt detected in blob_path");
        }
        target
    }

    pub fn partial_path(&self, content_hash: &str) -> PathBuf {
        let clean = content_hash.trim().to_lowercase();
        let target = self.partial_dir().join(format!("{clean}.part"));
        // Traversal guard
        if !target.starts_with(self.partial_dir()) {
            panic!("Path traversal attempt detected in partial_path");
        }
        target
    }

    pub fn blob_exists(&self, content_hash: &str) -> bool {
        if !Self::is_safe_hash(content_hash) {
            return false;
        }
        let path = self.blob_path(content_hash);
        path.is_file()
    }

    pub fn blob_size(&self, content_hash: &str) -> Option<u64> {
        if !Self::is_safe_hash(content_hash) {
            return None;
        }
        let path = self.blob_path(content_hash);
        fs::metadata(path).ok().map(|m| m.len())
    }

    pub fn partial_offset(&self, content_hash: &str) -> u64 {
        if !Self::is_safe_hash(content_hash) {
            return 0;
        }
        let path = self.partial_path(content_hash);
        fs::metadata(path).ok().map(|m| m.len()).unwrap_or(0)
    }

    pub fn discard_partial(&self, content_hash: &str) {
        if !Self::is_safe_hash(content_hash) {
            return;
        }
        let path = self.partial_path(content_hash);
        let _ = fs::remove_file(path);
    }

    /// Appends or writes bytes to the partial upload file at the specified offset.
    pub fn write_partial_chunk(
        &self,
        content_hash: &str,
        offset: u64,
        data: &[u8],
    ) -> io::Result<u64> {
        if !Self::is_safe_hash(content_hash) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid content hash"));
        }
        let path = self.partial_path(content_hash);
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(&path)?;

        file.seek(SeekFrom::Start(offset))?;
        file.write_all(data)?;
        file.sync_data()?;

        let new_len = file.metadata()?.len();
        Ok(new_len)
    }

    /// Verifies the full SHA-256 hash of the partial upload file and promotes it to permanent object.
    pub fn finalize_upload(
        &self,
        content_hash: &str,
        expected_size: u64,
    ) -> Result<PathBuf, String> {
        if !Self::is_safe_hash(content_hash) {
            return Err("Invalid content hash".to_string());
        }
        let part_path = self.partial_path(content_hash);
        let mut file = File::open(&part_path).map_err(|e| format!("Failed to open partial file: {e}"))?;

        let meta = file.metadata().map_err(|e| format!("Failed to read metadata: {e}"))?;
        if meta.len() != expected_size {
            return Err(format!(
                "Uploaded size mismatch: partial file is {} bytes, but expected {}",
                meta.len(),
                expected_size
            ));
        }

        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buffer).map_err(|e| format!("Read error during verification: {e}"))?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        let calculated_hash = hex::encode(hasher.finalize());
        if !calculated_hash.eq_ignore_ascii_case(content_hash) {
            self.discard_partial(content_hash);
            return Err(format!(
                "Integrity mismatch: computed SHA-256 {} does not match expected hash {}",
                calculated_hash, content_hash
            ));
        }

        let target_path = self.blob_path(content_hash);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Failed to create object dir: {e}"))?;
        }

        fs::rename(&part_path, &target_path).map_err(|e| format!("Failed to promote blob: {e}"))?;
        info!(content_hash = %content_hash, size = expected_size, "Blob promoted to permanent storage");

        Ok(target_path)
    }

    /// Cryptographic grant signature bound to operation, room_id, content_hash, and expiration.
    pub fn generate_grant(&self, room_id: &str, content_hash: &str, operation: &str, expires_at: u64) -> String {
        let mut mac = HmacSha256::new_from_slice(&self.secret_key)
            .expect("HMAC can take key of any size");
        let message = format!("{operation}:{room_id}:{content_hash}:{expires_at}");
        mac.update(message.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// Verifies the cryptographic grant signature and expiration timestamp.
    pub fn verify_grant(
        &self,
        room_id: &str,
        content_hash: &str,
        operation: &str,
        expires_at: u64,
        provided_grant: &str,
    ) -> bool {
        if !Self::is_safe_hash(content_hash) {
            return false;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        if now > expires_at {
            warn!(content_hash = %content_hash, room_id = %room_id, "Grant expired");
            return false;
        }

        let expected_grant = self.generate_grant(room_id, content_hash, operation, expires_at);
        if expected_grant.len() != provided_grant.len() {
            return false;
        }

        let mut diff = 0u8;
        for (a, b) in expected_grant.bytes().zip(provided_grant.bytes()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    pub fn build_upload_url(&self, room_id: &str, content_hash: &str) -> (String, u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expires_at = now + UPLOAD_GRANT_TTL_SECS;
        let grant = self.generate_grant(room_id, content_hash, "upload", expires_at);
        let url = format!(
            "{}/v1/blobs/upload/{}?room_id={}&grant={}&expires={}",
            self.public_url, content_hash, room_id, grant, expires_at
        );
        (url, expires_at)
    }

    pub fn build_download_url(&self, room_id: &str, content_hash: &str) -> (String, u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let expires_at = now + DOWNLOAD_GRANT_TTL_SECS;
        let grant = self.generate_grant(room_id, content_hash, "download", expires_at);
        let url = format!(
            "{}/v1/blobs/download/{}?room_id={}&grant={}&expires={}",
            self.public_url, content_hash, room_id, grant, expires_at
        );
        (url, expires_at)
    }

    pub fn room_quota_bytes(&self) -> u64 {
        self.room_quota_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_grant_generation_and_verification() {
        let temp_dir = std::env::temp_dir().join("ltk_storage_test_e16");
        let manager = StorageManager::new(&temp_dir, b"test_secret_key".to_vec(), "http://localhost:3000".to_string()).unwrap();

        let hash = "a".repeat(64);
        let (upload_url, expires) = manager.build_upload_url("room-1", &hash);
        assert!(upload_url.contains(&hash));
        assert!(upload_url.contains("room_id=room-1"));

        let grant = manager.generate_grant("room-1", &hash, "upload", expires);
        assert!(manager.verify_grant("room-1", &hash, "upload", expires, &grant));
        // Cross-room isolation: grant for room-1 must fail for room-2!
        assert!(!manager.verify_grant("room-2", &hash, "upload", expires, &grant));
        // Cross-operation isolation: upload grant must fail for download
        assert!(!manager.verify_grant("room-1", &hash, "download", expires, &grant));
        // Tampered grant fails
        assert!(!manager.verify_grant("room-1", &hash, "upload", expires, "invalid_grant"));
    }

    #[test]
    fn test_safe_hash_validation() {
        assert!(StorageManager::is_safe_hash(&"f".repeat(64)));
        assert!(!StorageManager::is_safe_hash("../etc/passwd"));
        assert!(!StorageManager::is_safe_hash("f".repeat(63).as_str()));
        assert!(!StorageManager::is_safe_hash("f".repeat(65).as_str()));
        assert!(!StorageManager::is_safe_hash(""));
    }
}
