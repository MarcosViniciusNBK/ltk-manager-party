//! Room tokens in the operating-system credential vault.
//!
//! The SQLite state store never accepts secret bytes. On Windows these methods use Generic
//! Credentials scoped to the current user and local machine. Callers receive [`RoomSecret`], whose
//! memory is zeroed when dropped and whose debug output is always redacted.

use super::validate_room_id;
use std::fmt;
use std::io;
use thiserror::Error;
use zeroize::Zeroize;

const TARGET_PREFIX: &str = "LTK Manager/RoomSync";
const MAX_SECRET_BYTES: usize = 2_048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomSecretKind {
    MemberToken,
    OwnerToken,
}

impl RoomSecretKind {
    fn target_suffix(self) -> &'static str {
        match self {
            Self::MemberToken => "member",
            Self::OwnerToken => "owner",
        }
    }
}

/// Secret bytes with redacted formatting and best-effort memory clearing on drop.
pub struct RoomSecret(Vec<u8>);

impl RoomSecret {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for RoomSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RoomSecret([REDACTED])")
    }
}

impl Drop for RoomSecret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Access to credentials owned by the room synchronization feature.
#[derive(Debug, Clone, Copy, Default)]
pub struct RoomCredentialVault;

impl RoomCredentialVault {
    /// Deterministic, non-secret target name. No server URL, password, or token appears in it.
    pub fn target(room_id: &str, kind: RoomSecretKind) -> Result<String, CredentialError> {
        validate_room_id(room_id).map_err(|_| CredentialError::InvalidRoomId)?;
        Ok(format!(
            "{TARGET_PREFIX}/{room_id}/{}",
            kind.target_suffix()
        ))
    }

    /// Create or replace one room token in Windows Credential Manager.
    #[cfg(windows)]
    pub fn write(
        &self,
        room_id: &str,
        kind: RoomSecretKind,
        secret: &[u8],
    ) -> Result<(), CredentialError> {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr;
        use windows_sys::Win32::Security::Credentials::{
            CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
        };

        validate_secret(secret)?;
        let target = Self::target(room_id, kind)?;
        let mut target_wide: Vec<u16> = std::ffi::OsStr::new(&target)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: CREDENTIALW is a plain Win32 C structure. Every non-null pointer below remains
        // valid for the duration of CredWriteW, and all omitted optional fields stay null/zero.
        let mut credential: CREDENTIALW = unsafe { std::mem::zeroed() };
        credential.Type = CRED_TYPE_GENERIC;
        credential.TargetName = target_wide.as_mut_ptr();
        credential.CredentialBlobSize =
            u32::try_from(secret.len()).map_err(|_| CredentialError::SecretTooLarge)?;
        credential.CredentialBlob = secret.as_ptr().cast_mut();
        credential.Persist = CRED_PERSIST_LOCAL_MACHINE;
        credential.UserName = ptr::null_mut();

        // SAFETY: credential and every referenced buffer are valid through this synchronous call.
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            return Err(CredentialError::Windows(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Read one token, returning `None` when that token kind is not stored for the room.
    #[cfg(windows)]
    pub fn read(
        &self,
        room_id: &str,
        kind: RoomSecretKind,
    ) -> Result<Option<RoomSecret>, CredentialError> {
        use std::os::windows::ffi::OsStrExt;
        use std::ptr;
        use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
        use windows_sys::Win32::Security::Credentials::{
            CRED_TYPE_GENERIC, CREDENTIALW, CredReadW,
        };

        let target = Self::target(room_id, kind)?;
        let target_wide: Vec<u16> = std::ffi::OsStr::new(&target)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut raw: *mut CREDENTIALW = ptr::null_mut();

        // SAFETY: target_wide is null-terminated and raw points to writable pointer storage.
        if unsafe { CredReadW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0, &mut raw) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
                return Ok(None);
            }
            return Err(CredentialError::Windows(error));
        }

        let owned = CredentialBuffer(raw);
        // SAFETY: a successful CredReadW returned a non-null CREDENTIALW owned by `owned`.
        let credential = unsafe { raw.as_ref() }.ok_or(CredentialError::InvalidCredentialBuffer)?;
        let size = credential.CredentialBlobSize as usize;
        if size == 0 || size > MAX_SECRET_BYTES || credential.CredentialBlob.is_null() {
            return Err(CredentialError::InvalidCredentialBuffer);
        }
        // SAFETY: CredentialBlob is valid for CredentialBlobSize bytes until `owned` is dropped.
        let secret =
            unsafe { std::slice::from_raw_parts(credential.CredentialBlob, size) }.to_vec();
        drop(owned);
        Ok(Some(RoomSecret(secret)))
    }

    /// Delete a token. Returns false when it was already absent.
    #[cfg(windows)]
    pub fn delete(&self, room_id: &str, kind: RoomSecretKind) -> Result<bool, CredentialError> {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Foundation::ERROR_NOT_FOUND;
        use windows_sys::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CredDeleteW};

        let target = Self::target(room_id, kind)?;
        let target_wide: Vec<u16> = std::ffi::OsStr::new(&target)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // SAFETY: target_wide is a valid null-terminated target name for this synchronous call.
        if unsafe { CredDeleteW(target_wide.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NOT_FOUND as i32) {
            Ok(false)
        } else {
            Err(CredentialError::Windows(error))
        }
    }

    #[cfg(not(windows))]
    pub fn write(
        &self,
        room_id: &str,
        kind: RoomSecretKind,
        secret: &[u8],
    ) -> Result<(), CredentialError> {
        Self::target(room_id, kind)?;
        validate_secret(secret)?;
        Err(CredentialError::UnsupportedPlatform)
    }

    #[cfg(not(windows))]
    pub fn read(
        &self,
        room_id: &str,
        kind: RoomSecretKind,
    ) -> Result<Option<RoomSecret>, CredentialError> {
        Self::target(room_id, kind)?;
        Err(CredentialError::UnsupportedPlatform)
    }

    #[cfg(not(windows))]
    pub fn delete(&self, room_id: &str, kind: RoomSecretKind) -> Result<bool, CredentialError> {
        Self::target(room_id, kind)?;
        Err(CredentialError::UnsupportedPlatform)
    }
}

#[cfg(windows)]
struct CredentialBuffer(*mut windows_sys::Win32::Security::Credentials::CREDENTIALW);

#[cfg(windows)]
impl Drop for CredentialBuffer {
    fn drop(&mut self) {
        use windows_sys::Win32::Security::Credentials::CredFree;

        if !self.0.is_null() {
            // SAFETY: CredReadW allocated this buffer, and this guard frees it exactly once.
            unsafe { CredFree(self.0.cast()) };
        }
    }
}

fn validate_secret(secret: &[u8]) -> Result<(), CredentialError> {
    if secret.is_empty() {
        Err(CredentialError::EmptySecret)
    } else if secret.len() > MAX_SECRET_BYTES {
        Err(CredentialError::SecretTooLarge)
    } else {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("room ID is invalid for a credential target")]
    InvalidRoomId,
    #[error("room tokens cannot be empty")]
    EmptySecret,
    #[error("room token exceeds {MAX_SECRET_BYTES} bytes")]
    SecretTooLarge,
    #[error("Windows Credential Manager returned an invalid credential buffer")]
    InvalidCredentialBuffer,
    #[error("Windows Credential Manager error: {0}")]
    Windows(#[source] io::Error),
    #[error("room credential storage is only available on Windows")]
    UnsupportedPlatform,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_names_are_scoped_and_contain_no_secret() {
        assert_eq!(
            RoomCredentialVault::target("room_123", RoomSecretKind::MemberToken).unwrap(),
            "LTK Manager/RoomSync/room_123/member"
        );
        assert_eq!(
            RoomCredentialVault::target("room_123", RoomSecretKind::OwnerToken).unwrap(),
            "LTK Manager/RoomSync/room_123/owner"
        );
        assert!(RoomCredentialVault::target("../escape", RoomSecretKind::MemberToken).is_err());
    }

    #[test]
    fn room_secrets_are_always_redacted() {
        let secret = RoomSecret(b"do-not-print-me".to_vec());
        assert_eq!(format!("{secret:?}"), "RoomSecret([REDACTED])");
        assert_eq!(secret.as_bytes(), b"do-not-print-me");
    }

    #[test]
    fn empty_and_oversized_secrets_are_rejected_before_platform_access() {
        assert!(matches!(
            validate_secret(&[]),
            Err(CredentialError::EmptySecret)
        ));
        assert!(matches!(
            validate_secret(&vec![0; MAX_SECRET_BYTES + 1]),
            Err(CredentialError::SecretTooLarge)
        ));
    }
}
