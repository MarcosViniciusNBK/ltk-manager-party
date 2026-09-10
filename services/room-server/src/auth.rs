//! Cryptographic authentication, password hashing with Argon2id, and token generation.

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::RngCore;
use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum AuthError {
    #[error("Password hashing error: {0}")]
    Hashing(String),
    #[error("Invalid credentials provided")]
    InvalidCredentials,
    #[error("Missing authorization header")]
    MissingToken,
}

/// Hash a password using Argon2id with a cryptographically secure random salt.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();

    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| AuthError::Hashing(e.to_string()))
}

/// Verify a plaintext password against an Argon2id password hash using constant-time comparison.
pub fn verify_password(password: &str, password_hash: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(password_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
}

/// Generate a cryptographically secure random token (256 bits of entropy, 64-character lowercase hex).
pub fn generate_high_entropy_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashing_and_verification_roundtrip() {
        let password = "super_secure_room_password_123";
        let hash = hash_password(password).expect("hashing should succeed");

        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(password, &hash));
        assert!(!verify_password("wrong_password", &hash));
        assert!(!verify_password("", &hash));
    }

    #[test]
    fn high_entropy_tokens_are_unique_and_sufficiently_long() {
        let t1 = generate_high_entropy_token();
        let t2 = generate_high_entropy_token();

        assert_eq!(t1.len(), 64);
        assert_eq!(t2.len(), 64);
        assert_ne!(t1, t2);
    }
}
