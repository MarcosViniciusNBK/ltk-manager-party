//! Signed desktop update distribution.
//!
//! The server never signs a build and never receives the private signing key. Deployment places
//! one atomically-written `latest.json` plus immutable signed artifacts in `UPDATES_DIR`; this
//! route validates the metadata and adapts it to Tauri's dynamic updater protocol.

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderValue, Response, StatusCode};
use semver::Version;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;

use crate::state::AppState;

#[derive(Debug, Deserialize)]
struct ReleaseManifest {
    version: String,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    pub_date: Option<String>,
    platforms: HashMap<String, ReleaseArtifact>,
}

#[derive(Debug, Deserialize)]
struct ReleaseArtifact {
    artifact: String,
    signature: String,
}

#[derive(Debug, Serialize)]
struct UpdateResponse {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub_date: Option<String>,
    url: String,
    signature: String,
}

/// Return 204 when this client is current, or Tauri's signed dynamic-update response otherwise.
pub async fn check_update(
    State(state): State<AppState>,
    Path((target, arch, current_version)): Path<(String, String, String)>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let Some(manifest) = read_manifest(&state.updates_dir).await? else {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(CACHE_CONTROL, "no-store")
            .body(Body::empty())
            .map_err(internal_response_error);
    };
    let latest = parse_version(&manifest.version).ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "The published update version is invalid".to_string(),
    ))?;
    let current = parse_version(&current_version).ok_or((
        StatusCode::BAD_REQUEST,
        "The current application version is invalid".to_string(),
    ))?;

    if current >= latest {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(CACHE_CONTROL, "no-store")
            .body(Body::empty())
            .map_err(internal_response_error);
    }

    let platform = format!("{target}-{arch}");
    let artifact = manifest.platforms.get(&platform).ok_or((
        StatusCode::NOT_FOUND,
        format!("No update artifact is published for {platform}"),
    ))?;
    validate_artifact_name(&artifact.artifact)?;
    if artifact.signature.trim().is_empty() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "The published update signature is empty".to_string(),
        ));
    }

    let base = state.public_server_url.trim_end_matches('/');
    let payload = UpdateResponse {
        version: manifest.version,
        notes: manifest.notes,
        pub_date: manifest.pub_date,
        url: format!("{base}/v1/updates/files/{}", artifact.artifact),
        signature: artifact.signature.clone(),
    };
    let bytes = serde_json::to_vec(&payload).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Could not serialize update metadata: {error}"),
        )
    })?;
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/json")
        .header(CACHE_CONTROL, "no-store")
        .body(Body::from(bytes))
        .map_err(internal_response_error)
}

/// Stream an immutable, signed updater artifact without ever buffering it in process memory.
pub async fn download_update(
    State(state): State<AppState>,
    Path(file_name): Path<String>,
) -> Result<Response<Body>, (StatusCode, String)> {
    validate_artifact_name(&file_name)?;
    let path = state.updates_dir.join(&file_name);
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|error| file_error(&path, error))?;
    let length = file
        .metadata()
        .await
        .map_err(|error| file_error(&path, error))?
        .len();
    let stream = ReaderStream::new(file);

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/octet-stream")
        .header(CONTENT_LENGTH, length)
        .header(CACHE_CONTROL, "public, max-age=31536000, immutable")
        .header(
            CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("attachment; filename=\"{file_name}\""))
                .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?,
        )
        .body(Body::from_stream(stream))
        .map_err(internal_response_error)
}

async fn read_manifest(
    directory: &FsPath,
) -> Result<Option<ReleaseManifest>, (StatusCode, String)> {
    let path = directory.join("latest.json");
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(file_error(&path, error)),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Published update metadata is invalid: {error}"),
        )
    })
}

fn parse_version(value: &str) -> Option<Version> {
    Version::parse(value.trim().trim_start_matches('v')).ok()
}

fn validate_artifact_name(file_name: &str) -> Result<(), (StatusCode, String)> {
    let valid = !file_name.is_empty()
        && file_name.len() <= 180
        && FsPath::new(file_name)
            .file_name()
            .is_some_and(|name| name == file_name)
        && file_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    if valid {
        Ok(())
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            "Invalid update artifact name".to_string(),
        ))
    }
}

fn file_error(path: &PathBuf, error: std::io::Error) -> (StatusCode, String) {
    if error.kind() == std::io::ErrorKind::NotFound {
        (StatusCode::NOT_FOUND, "No update is published".to_string())
    } else {
        tracing::error!(?path, %error, "Could not read published update file");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not read the published update".to_string(),
        )
    }
}

fn internal_response_error(error: axum::http::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_semver_with_optional_v_prefix() {
        assert_eq!(parse_version("v1.20.3"), Some(Version::new(1, 20, 3)));
        assert_eq!(parse_version("1.20.3"), Some(Version::new(1, 20, 3)));
        assert_eq!(parse_version("latest"), None);
    }

    #[test]
    fn artifact_names_cannot_escape_the_update_directory() {
        assert!(validate_artifact_name("ltk-manager_1.20.0_x64-setup.exe").is_ok());
        assert!(validate_artifact_name("../secret").is_err());
        assert!(validate_artifact_name("nested/file.exe").is_err());
        assert!(validate_artifact_name("file name.exe").is_err());
    }
}
