//! Bounded, resumable HTTP transfers for immutable room blobs.
//!
//! Downloads use `Range` plus `If-Range`/`ETag`, stream through a fixed-size buffer into the
//! cache's `.part` file, and rely on the cache's size/SHA-256 verification before publication.
//! Uploads use a small resumable contract: `HEAD` reports `Upload-Offset`, `PUT` carries the
//! remaining `Content-Range`, and successful completion must echo `X-Content-SHA256`.

use super::{
    CacheCommit, CanonicalRoomArtifact, ContentHash, PendingTransfer, RoomCache, RoomCacheError,
    RoomStateError, RoomStateStore, TransferDirection, TransferState, validate_room_id,
};
use fs_err as fs;
use reqwest::StatusCode;
use reqwest::blocking::{Body, Client, Response};
use reqwest::header::{
    ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, ETAG, HeaderName, HeaderValue, IF_MATCH,
    IF_RANGE, RANGE,
};
use serde::Serialize;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;

const UPLOAD_OFFSET: HeaderName = HeaderName::from_static("upload-offset");
const CONTENT_SHA256: HeaderName = HeaderName::from_static("x-content-sha256");
const PERSIST_EVERY_BYTES: u64 = 1024 * 1024;
const PERSIST_EVERY: Duration = Duration::from_millis(500);
const REPORT_EVERY: Duration = Duration::from_millis(100);

pub type TransferProgressCallback = Arc<dyn Fn(TransferProgress) + Send + Sync + 'static>;

#[derive(Clone, Copy)]
pub struct TransferContext<'a> {
    pub store: &'a RoomStateStore,
    pub cache: &'a RoomCache,
    pub room_id: &'a str,
    pub cancellation: &'a TransferCancellation,
    pub progress: Option<&'a TransferProgressCallback>,
}

impl<'a> TransferContext<'a> {
    pub fn new(
        store: &'a RoomStateStore,
        cache: &'a RoomCache,
        room_id: &'a str,
        cancellation: &'a TransferCancellation,
    ) -> Self {
        Self {
            store,
            cache,
            room_id,
            cancellation,
            progress: None,
        }
    }

    pub fn with_progress(mut self, progress: &'a TransferProgressCallback) -> Self {
        self.progress = Some(progress);
        self
    }
}

#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_concurrent: usize,
    pub max_attempts: u32,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub buffer_bytes: usize,
    pub disk_reserve_bytes: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            max_attempts: 5,
            base_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(8),
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(30 * 60),
            buffer_bytes: 64 * 1024,
            disk_reserve_bytes: 64 * 1024 * 1024,
        }
    }
}

/// An endpoint whose debug representation cannot leak a signed query string.
#[derive(Clone)]
pub struct TransferEndpoint(reqwest::Url);

impl TransferEndpoint {
    /// Accept HTTPS endpoints and loopback HTTP endpoints used by local development/tests.
    pub fn parse(value: &str) -> Result<Self, TransferEngineError> {
        let url = reqwest::Url::parse(value).map_err(|_| TransferEngineError::InvalidEndpoint)?;
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(TransferEngineError::InvalidEndpoint);
        }
        let is_loopback = url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback) {
            return Err(TransferEngineError::InsecureEndpoint);
        }
        Ok(Self(url))
    }
}

impl fmt::Debug for TransferEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TransferEndpoint([REDACTED])")
    }
}

#[derive(Debug, Clone, Default)]
pub struct TransferCancellation(Arc<AtomicBool>);

impl TransferCancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS, specta::Type))]
#[cfg_attr(feature = "ts", ts(export))]
pub struct TransferProgress {
    pub room_id: String,
    pub content_hash: ContentHash,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub direction: TransferDirection,
    pub transferred_bytes: u64,
    pub total_bytes: u64,
    pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferOutcome {
    AlreadyCached(PathBuf),
    Downloaded(PathBuf),
    Uploaded { size_bytes: u64 },
}

#[derive(Debug, Clone)]
pub struct HttpTransferEngine {
    client: Client,
    policy: RetryPolicy,
    limiter: Arc<TransferLimiter>,
}

impl HttpTransferEngine {
    pub fn new(policy: RetryPolicy) -> Result<Self, TransferEngineError> {
        if policy.max_concurrent == 0
            || policy.max_attempts == 0
            || policy.buffer_bytes == 0
            || policy.base_backoff > policy.max_backoff
        {
            return Err(TransferEngineError::InvalidPolicy);
        }
        let client = Client::builder()
            .connect_timeout(policy.connect_timeout)
            .timeout(policy.request_timeout)
            .build()
            .map_err(|_| TransferEngineError::Network("failed to build HTTP client"))?;
        Ok(Self {
            limiter: Arc::new(TransferLimiter::new(policy.max_concurrent)),
            client,
            policy,
        })
    }

    /// Download or resume an artifact, then verify and publish it in the room cache.
    pub fn download(
        &self,
        context: TransferContext<'_>,
        artifact: &CanonicalRoomArtifact,
        endpoint: &TransferEndpoint,
    ) -> Result<TransferOutcome, TransferEngineError> {
        let TransferContext {
            store,
            cache,
            room_id,
            cancellation,
            progress,
        } = context;
        validate_room_id(room_id).map_err(|_| TransferEngineError::InvalidRoomId)?;
        if artifact.size_bytes == 0 {
            return Err(TransferEngineError::InvalidArtifact);
        }
        let _permit = self.limiter.acquire(cancellation)?;
        if cache.contains(artifact)? {
            store.remove_transfer(room_id, &artifact.content_hash, TransferDirection::Download)?;
            return Ok(TransferOutcome::AlreadyCached(
                cache.blob_path(&artifact.content_hash),
            ));
        }

        let partial = cache.prepare_partial(&artifact.content_hash)?;
        let mut offset = partial_length(&partial)?;
        if offset > artifact.size_bytes {
            cache.discard_partial(&artifact.content_hash)?;
            offset = 0;
        }

        let existing = existing_transfer(
            store,
            room_id,
            &artifact.content_hash,
            TransferDirection::Download,
        )?;
        let mut etag = existing.and_then(|transfer| transfer.etag);
        if offset > 0 && etag.is_none() {
            truncate(&partial)?;
            offset = 0;
        }
        ensure_disk_space(
            cache.root(),
            artifact.size_bytes - offset,
            self.policy.disk_reserve_bytes,
        )?;

        persist_transfer(
            store,
            room_id,
            artifact,
            TransferDirection::Download,
            TransferState::Queued,
            offset,
            etag.clone(),
        )?;

        if offset == artifact.size_bytes {
            return self.finish_download(store, cache, room_id, artifact);
        }

        for attempt in 1..=self.policy.max_attempts {
            check_cancelled(cancellation)?;
            persist_transfer(
                store,
                room_id,
                artifact,
                TransferDirection::Download,
                TransferState::Transferring,
                offset,
                etag.clone(),
            )?;
            emit_progress(
                progress,
                room_id,
                artifact,
                TransferDirection::Download,
                offset,
                attempt,
            );

            match self.download_attempt(
                store,
                &partial,
                room_id,
                artifact,
                endpoint,
                cancellation,
                progress,
                attempt,
                offset,
                etag.as_deref(),
            ) {
                Ok(result) => {
                    offset = result.offset;
                    etag = result.etag;
                    if offset == artifact.size_bytes {
                        return self.finish_download(store, cache, room_id, artifact);
                    }
                }
                Err(AttemptFailure::Restart) => {
                    truncate(&partial)?;
                    offset = 0;
                    etag = None;
                }
                Err(AttemptFailure::Fatal(TransferEngineError::Cancelled)) => {
                    offset = partial_length(&partial)?.min(artifact.size_bytes);
                    let persisted = existing_transfer(
                        store,
                        room_id,
                        &artifact.content_hash,
                        TransferDirection::Download,
                    )?;
                    etag = persisted.and_then(|transfer| transfer.etag);
                    persist_transfer(
                        store,
                        room_id,
                        artifact,
                        TransferDirection::Download,
                        TransferState::Paused,
                        offset,
                        etag,
                    )?;
                    return Err(TransferEngineError::Cancelled);
                }
                Err(AttemptFailure::Fatal(error)) => return Err(error),
                Err(AttemptFailure::Retryable) => {
                    offset = partial_length(&partial)?.min(artifact.size_bytes);
                    let persisted = existing_transfer(
                        store,
                        room_id,
                        &artifact.content_hash,
                        TransferDirection::Download,
                    )?;
                    etag = persisted.and_then(|transfer| transfer.etag);
                    if offset > 0 && etag.is_none() {
                        truncate(&partial)?;
                        offset = 0;
                    }
                }
            }

            persist_transfer(
                store,
                room_id,
                artifact,
                TransferDirection::Download,
                TransferState::Paused,
                offset,
                etag.clone(),
            )?;
            if attempt == self.policy.max_attempts {
                return Err(TransferEngineError::AttemptsExhausted {
                    operation: "download",
                    attempts: attempt,
                });
            }
            wait_for_retry(&self.policy, artifact, attempt, cancellation)?;
        }
        unreachable!("max_attempts is validated as nonzero")
    }

    /// Resume an upload from the server-confirmed offset and require a SHA-256 completion receipt.
    pub fn upload(
        &self,
        context: TransferContext<'_>,
        artifact: &CanonicalRoomArtifact,
        endpoint: &TransferEndpoint,
    ) -> Result<TransferOutcome, TransferEngineError> {
        let TransferContext {
            store,
            cache,
            room_id,
            cancellation,
            progress,
        } = context;
        validate_room_id(room_id).map_err(|_| TransferEngineError::InvalidRoomId)?;
        if artifact.size_bytes == 0 {
            return Err(TransferEngineError::InvalidArtifact);
        }
        let _permit = self.limiter.acquire(cancellation)?;
        if !cache.contains(artifact)? {
            return Err(TransferEngineError::MissingCachedSource(
                artifact.content_hash.clone(),
            ));
        }
        let source = cache.blob_path(&artifact.content_hash);

        for attempt in 1..=self.policy.max_attempts {
            check_cancelled(cancellation)?;
            let probe = match self.probe_upload(endpoint, artifact) {
                Ok(probe) => probe,
                Err(AttemptFailure::Fatal(error)) => return Err(error),
                Err(AttemptFailure::Restart | AttemptFailure::Retryable) => {
                    if attempt == self.policy.max_attempts {
                        return Err(TransferEngineError::AttemptsExhausted {
                            operation: "upload",
                            attempts: attempt,
                        });
                    }
                    wait_for_retry(&self.policy, artifact, attempt, cancellation)?;
                    continue;
                }
            };
            let confirmed = probe.offset;
            if confirmed > artifact.size_bytes {
                return Err(TransferEngineError::Protocol(
                    "server upload offset exceeds artifact size",
                ));
            }
            if confirmed == artifact.size_bytes {
                verify_receipt(probe.receipt.as_deref(), &artifact.content_hash)?;
                store.remove_transfer(
                    room_id,
                    &artifact.content_hash,
                    TransferDirection::Upload,
                )?;
                return Ok(TransferOutcome::Uploaded {
                    size_bytes: artifact.size_bytes,
                });
            }

            persist_transfer(
                store,
                room_id,
                artifact,
                TransferDirection::Upload,
                TransferState::Transferring,
                confirmed,
                probe.etag.clone(),
            )?;
            emit_progress(
                progress,
                room_id,
                artifact,
                TransferDirection::Upload,
                confirmed,
                attempt,
            );

            match self.upload_attempt(
                &source,
                room_id,
                artifact,
                endpoint,
                cancellation,
                progress,
                attempt,
                &probe,
            ) {
                Ok(()) => {
                    store.remove_transfer(
                        room_id,
                        &artifact.content_hash,
                        TransferDirection::Upload,
                    )?;
                    return Ok(TransferOutcome::Uploaded {
                        size_bytes: artifact.size_bytes,
                    });
                }
                Err(AttemptFailure::Fatal(TransferEngineError::Cancelled)) => {
                    persist_transfer(
                        store,
                        room_id,
                        artifact,
                        TransferDirection::Upload,
                        TransferState::Paused,
                        confirmed,
                        probe.etag,
                    )?;
                    return Err(TransferEngineError::Cancelled);
                }
                Err(AttemptFailure::Fatal(error)) => return Err(error),
                Err(AttemptFailure::Restart | AttemptFailure::Retryable) => {}
            }

            persist_transfer(
                store,
                room_id,
                artifact,
                TransferDirection::Upload,
                TransferState::Paused,
                confirmed,
                probe.etag,
            )?;
            if attempt == self.policy.max_attempts {
                return Err(TransferEngineError::AttemptsExhausted {
                    operation: "upload",
                    attempts: attempt,
                });
            }
            wait_for_retry(&self.policy, artifact, attempt, cancellation)?;
        }
        unreachable!("max_attempts is validated as nonzero")
    }

    #[allow(clippy::too_many_arguments)]
    fn download_attempt(
        &self,
        store: &RoomStateStore,
        partial: &Path,
        room_id: &str,
        artifact: &CanonicalRoomArtifact,
        endpoint: &TransferEndpoint,
        cancellation: &TransferCancellation,
        progress: Option<&TransferProgressCallback>,
        attempt: u32,
        offset: u64,
        saved_etag: Option<&str>,
    ) -> Result<DownloadAttempt, AttemptFailure> {
        let mut request = self
            .client
            .get(endpoint.0.clone())
            .header(ACCEPT_ENCODING, "identity");
        if offset > 0 {
            request = request.header(RANGE, format!("bytes={offset}-"));
            let etag = HeaderValue::from_str(saved_etag.ok_or(AttemptFailure::Restart)?)
                .map_err(|_| AttemptFailure::Restart)?;
            request = request.header(IF_RANGE, etag);
        }
        let mut response = request.send().map_err(|_| AttemptFailure::Retryable)?;
        let status = response.status();
        if offset > 0 && status == StatusCode::OK {
            return Err(AttemptFailure::Restart);
        }
        if !matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT) {
            return Err(status_failure(status));
        }

        validate_download_response(&response, offset, artifact.size_bytes)?;
        let response_etag = header_text(&response, ETAG)?;
        if offset > 0 && response_etag.as_deref() != saved_etag {
            return Err(AttemptFailure::Restart);
        }
        let etag = response_etag.or_else(|| saved_etag.map(ToOwned::to_owned));
        persist_transfer(
            store,
            room_id,
            artifact,
            TransferDirection::Download,
            TransferState::Transferring,
            offset,
            etag.clone(),
        )
        .map_err(AttemptFailure::Fatal)?;

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(partial)
            .map_err(|error| AttemptFailure::Fatal(error.into()))?;
        let mut buffer = vec![0_u8; self.policy.buffer_bytes];
        let mut transferred = offset;
        let mut last_persisted = offset;
        let mut last_persisted_at = Instant::now();
        let mut last_reported_at = Instant::now();
        loop {
            if cancellation.is_cancelled() {
                return Err(AttemptFailure::Fatal(TransferEngineError::Cancelled));
            }
            let read = response
                .read(&mut buffer)
                .map_err(|_| AttemptFailure::Retryable)?;
            if read == 0 {
                break;
            }
            let read = read as u64;
            if transferred.saturating_add(read) > artifact.size_bytes {
                return Err(AttemptFailure::Fatal(TransferEngineError::Protocol(
                    "download body exceeds declared artifact size",
                )));
            }
            file.write_all(&buffer[..read as usize])
                .map_err(|error| AttemptFailure::Fatal(error.into()))?;
            transferred += read;

            if transferred - last_persisted >= PERSIST_EVERY_BYTES
                || last_persisted_at.elapsed() >= PERSIST_EVERY
                || transferred == artifact.size_bytes
            {
                persist_transfer(
                    store,
                    room_id,
                    artifact,
                    TransferDirection::Download,
                    TransferState::Transferring,
                    transferred,
                    etag.clone(),
                )
                .map_err(AttemptFailure::Fatal)?;
                last_persisted = transferred;
                last_persisted_at = Instant::now();
            }
            if last_reported_at.elapsed() >= REPORT_EVERY || transferred == artifact.size_bytes {
                emit_progress(
                    progress,
                    room_id,
                    artifact,
                    TransferDirection::Download,
                    transferred,
                    attempt,
                );
                last_reported_at = Instant::now();
            }
        }
        file.sync_data()
            .map_err(|error| AttemptFailure::Fatal(error.into()))?;
        if transferred != artifact.size_bytes {
            return Err(AttemptFailure::Retryable);
        }
        Ok(DownloadAttempt {
            offset: transferred,
            etag,
        })
    }

    fn finish_download(
        &self,
        store: &RoomStateStore,
        cache: &RoomCache,
        room_id: &str,
        artifact: &CanonicalRoomArtifact,
    ) -> Result<TransferOutcome, TransferEngineError> {
        let result = cache.commit_partial(artifact);
        let committed = match result {
            Ok(committed) => committed,
            Err(RoomCacheError::Verification(_)) => {
                cache.discard_partial(&artifact.content_hash)?;
                return Err(TransferEngineError::IntegrityCheckFailed);
            }
            Err(error) => return Err(error.into()),
        };
        store.remove_transfer(room_id, &artifact.content_hash, TransferDirection::Download)?;
        Ok(match committed {
            CacheCommit::Stored(path) => TransferOutcome::Downloaded(path),
            CacheCommit::AlreadyPresent(path) => TransferOutcome::AlreadyCached(path),
        })
    }

    fn probe_upload(
        &self,
        endpoint: &TransferEndpoint,
        artifact: &CanonicalRoomArtifact,
    ) -> Result<UploadProbe, AttemptFailure> {
        let response = self
            .client
            .head(endpoint.0.clone())
            .send()
            .map_err(|_| AttemptFailure::Retryable)?;
        if !response.status().is_success() {
            return Err(status_failure(response.status()));
        }
        let offset = response
            .headers()
            .get(&UPLOAD_OFFSET)
            .ok_or_else(|| {
                AttemptFailure::Fatal(TransferEngineError::Protocol(
                    "upload probe omitted Upload-Offset",
                ))
            })?
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| {
                AttemptFailure::Fatal(TransferEngineError::Protocol(
                    "upload probe returned an invalid Upload-Offset",
                ))
            })?;
        if offset > artifact.size_bytes {
            return Err(AttemptFailure::Fatal(TransferEngineError::Protocol(
                "server upload offset exceeds artifact size",
            )));
        }
        Ok(UploadProbe {
            offset,
            etag: header_text(&response, ETAG)?,
            receipt: header_text(&response, CONTENT_SHA256)?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn upload_attempt(
        &self,
        source: &Path,
        room_id: &str,
        artifact: &CanonicalRoomArtifact,
        endpoint: &TransferEndpoint,
        cancellation: &TransferCancellation,
        progress: Option<&TransferProgressCallback>,
        attempt: u32,
        probe: &UploadProbe,
    ) -> Result<(), AttemptFailure> {
        let mut file =
            fs::File::open(source).map_err(|error| AttemptFailure::Fatal(error.into()))?;
        file.seek(SeekFrom::Start(probe.offset))
            .map_err(|error| AttemptFailure::Fatal(error.into()))?;
        let remaining = artifact.size_bytes - probe.offset;
        let reader = UploadReader {
            file,
            cancellation: cancellation.clone(),
            progress: progress.cloned(),
            room_id: room_id.to_string(),
            content_hash: artifact.content_hash.clone(),
            total: artifact.size_bytes,
            transferred: probe.offset,
            attempt,
            last_reported_at: Instant::now(),
        };
        let body = Body::sized(reader, remaining);
        let end = artifact.size_bytes - 1;
        let mut request = self
            .client
            .put(endpoint.0.clone())
            .header(
                CONTENT_RANGE,
                format!("bytes {}-{end}/{}", probe.offset, artifact.size_bytes),
            )
            .header(CONTENT_LENGTH, remaining)
            .body(body);
        if let Some(etag) = &probe.etag {
            let etag = HeaderValue::from_str(etag).map_err(|_| {
                AttemptFailure::Fatal(TransferEngineError::Protocol(
                    "upload probe returned an invalid ETag",
                ))
            })?;
            request = request.header(IF_MATCH, etag);
        }
        let response = match request.send() {
            Ok(response) => response,
            Err(_) if cancellation.is_cancelled() => {
                return Err(AttemptFailure::Fatal(TransferEngineError::Cancelled));
            }
            Err(_) => return Err(AttemptFailure::Retryable),
        };
        if !response.status().is_success() {
            return Err(status_failure(response.status()));
        }
        let receipt = header_text(&response, CONTENT_SHA256)?;
        verify_receipt(receipt.as_deref(), &artifact.content_hash).map_err(AttemptFailure::Fatal)
    }
}

#[derive(Debug, Error)]
pub enum TransferEngineError {
    #[error("room ID is invalid")]
    InvalidRoomId,
    #[error("transfer endpoint is invalid")]
    InvalidEndpoint,
    #[error("transfer endpoints must use HTTPS; HTTP is allowed only on loopback")]
    InsecureEndpoint,
    #[error("transfer retry policy is invalid")]
    InvalidPolicy,
    #[error("transfer artifact must not be empty")]
    InvalidArtifact,
    #[error("transfer was cancelled")]
    Cancelled,
    #[error("{operation} failed after {attempts} attempts")]
    AttemptsExhausted {
        operation: &'static str,
        attempts: u32,
    },
    #[error("HTTP transfer failed: {0}")]
    Network(&'static str),
    #[error("HTTP server returned status {0}")]
    HttpStatus(u16),
    #[error("transfer protocol error: {0}")]
    Protocol(&'static str),
    #[error("not enough disk space: need {required_bytes} bytes, have {available_bytes}")]
    InsufficientDiskSpace {
        required_bytes: u64,
        available_bytes: u64,
    },
    #[error("upload source is not verified in the room cache: {0}")]
    MissingCachedSource(ContentHash),
    #[error("downloaded bytes failed their final size or SHA-256 check")]
    IntegrityCheckFailed,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Cache(#[from] RoomCacheError),
    #[error(transparent)]
    State(#[from] RoomStateError),
    #[error("transfer concurrency lock is poisoned")]
    ConcurrencyLockPoisoned,
}

struct DownloadAttempt {
    offset: u64,
    etag: Option<String>,
}

struct UploadProbe {
    offset: u64,
    etag: Option<String>,
    receipt: Option<String>,
}

enum AttemptFailure {
    Restart,
    Retryable,
    Fatal(TransferEngineError),
}

impl From<io::Error> for AttemptFailure {
    fn from(error: io::Error) -> Self {
        Self::Fatal(error.into())
    }
}

struct UploadReader {
    file: fs::File,
    cancellation: TransferCancellation,
    progress: Option<TransferProgressCallback>,
    room_id: String,
    content_hash: ContentHash,
    total: u64,
    transferred: u64,
    attempt: u32,
    last_reported_at: Instant,
}

impl Read for UploadReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "upload cancelled",
            ));
        }
        let read = self.file.read(buffer)?;
        self.transferred = self.transferred.saturating_add(read as u64);
        if read != 0
            && (self.last_reported_at.elapsed() >= REPORT_EVERY || self.transferred == self.total)
            && let Some(progress) = &self.progress
        {
            progress(TransferProgress {
                room_id: self.room_id.clone(),
                content_hash: self.content_hash.clone(),
                display_name: None,
                direction: TransferDirection::Upload,
                transferred_bytes: self.transferred,
                total_bytes: self.total,
                attempt: self.attempt,
            });
            self.last_reported_at = Instant::now();
        }
        Ok(read)
    }
}

#[derive(Debug)]
struct TransferLimiter {
    maximum: usize,
    active: Mutex<usize>,
    available: Condvar,
}

impl TransferLimiter {
    fn new(maximum: usize) -> Self {
        Self {
            maximum,
            active: Mutex::new(0),
            available: Condvar::new(),
        }
    }

    fn acquire(
        self: &Arc<Self>,
        cancellation: &TransferCancellation,
    ) -> Result<TransferPermit, TransferEngineError> {
        check_cancelled(cancellation)?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| TransferEngineError::ConcurrencyLockPoisoned)?;
        while *active >= self.maximum {
            check_cancelled(cancellation)?;
            active = self
                .available
                .wait_timeout(active, Duration::from_millis(100))
                .map_err(|_| TransferEngineError::ConcurrencyLockPoisoned)?
                .0;
        }
        check_cancelled(cancellation)?;
        *active += 1;
        Ok(TransferPermit(Arc::clone(self)))
    }
}

struct TransferPermit(Arc<TransferLimiter>);

impl Drop for TransferPermit {
    fn drop(&mut self) {
        if let Ok(mut active) = self.0.active.lock() {
            *active = active.saturating_sub(1);
            self.0.available.notify_one();
        }
    }
}

fn validate_download_response(
    response: &Response,
    offset: u64,
    expected_size: u64,
) -> Result<(), AttemptFailure> {
    if response.status() == StatusCode::PARTIAL_CONTENT {
        let content_range = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range)
            .ok_or_else(|| {
                AttemptFailure::Fatal(TransferEngineError::Protocol(
                    "partial response omitted a valid Content-Range",
                ))
            })?;
        if content_range.0 != offset
            || content_range.1 != expected_size - 1
            || content_range.2 != expected_size
        {
            return Err(AttemptFailure::Restart);
        }
    } else if offset != 0 {
        return Err(AttemptFailure::Restart);
    }

    if let Some(length) = response.content_length()
        && length != expected_size - offset
    {
        return Err(AttemptFailure::Fatal(TransferEngineError::Protocol(
            "response Content-Length does not match the manifest",
        )));
    }
    Ok(())
}

fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let value = value.strip_prefix("bytes ")?;
    let (range, total) = value.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    let total = total.parse::<u64>().ok()?;
    if end < start || end >= total {
        return None;
    }
    Some((start, end, total))
}

fn header_text(response: &Response, name: HeaderName) -> Result<Option<String>, AttemptFailure> {
    response
        .headers()
        .get(name)
        .map(|value| {
            value.to_str().map(str::to_owned).map_err(|_| {
                AttemptFailure::Fatal(TransferEngineError::Protocol(
                    "server returned a non-text transfer header",
                ))
            })
        })
        .transpose()
}

fn verify_receipt(value: Option<&str>, expected: &ContentHash) -> Result<(), TransferEngineError> {
    let actual = value
        .ok_or(TransferEngineError::Protocol(
            "successful upload omitted X-Content-SHA256",
        ))
        .and_then(|value| {
            ContentHash::parse(value).map_err(|_| {
                TransferEngineError::Protocol("upload returned an invalid SHA-256 receipt")
            })
        })?;
    if &actual == expected {
        Ok(())
    } else {
        Err(TransferEngineError::IntegrityCheckFailed)
    }
}

fn status_failure(status: StatusCode) -> AttemptFailure {
    if matches!(status.as_u16(), 408 | 409 | 412 | 425 | 429) || status.is_server_error() {
        AttemptFailure::Retryable
    } else {
        AttemptFailure::Fatal(TransferEngineError::HttpStatus(status.as_u16()))
    }
}

fn partial_length(path: &Path) -> Result<u64, TransferEngineError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata.len()),
        Ok(_) => Err(TransferEngineError::Protocol(
            "partial cache entry is not a regular file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn truncate(path: &Path) -> Result<(), TransferEngineError> {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    Ok(())
}

fn existing_transfer(
    store: &RoomStateStore,
    room_id: &str,
    content_hash: &ContentHash,
    direction: TransferDirection,
) -> Result<Option<PendingTransfer>, TransferEngineError> {
    Ok(store
        .pending_transfers(room_id)?
        .into_iter()
        .find(|transfer| &transfer.content_hash == content_hash && transfer.direction == direction))
}

#[allow(clippy::too_many_arguments)]
fn persist_transfer(
    store: &RoomStateStore,
    room_id: &str,
    artifact: &CanonicalRoomArtifact,
    direction: TransferDirection,
    state: TransferState,
    transferred_bytes: u64,
    etag: Option<String>,
) -> Result<(), TransferEngineError> {
    let mut transfer = PendingTransfer::new(room_id, artifact, direction);
    transfer.state = state;
    transfer.transferred_bytes = transferred_bytes;
    transfer.etag = etag;
    store.save_transfer(transfer)?;
    Ok(())
}

fn emit_progress(
    progress: Option<&TransferProgressCallback>,
    room_id: &str,
    artifact: &CanonicalRoomArtifact,
    direction: TransferDirection,
    transferred_bytes: u64,
    attempt: u32,
) {
    if let Some(progress) = progress {
        progress(TransferProgress {
            room_id: room_id.to_string(),
            content_hash: artifact.content_hash.clone(),
            display_name: None,
            direction,
            transferred_bytes,
            total_bytes: artifact.size_bytes,
            attempt,
        });
    }
}

fn check_cancelled(cancellation: &TransferCancellation) -> Result<(), TransferEngineError> {
    if cancellation.is_cancelled() {
        Err(TransferEngineError::Cancelled)
    } else {
        Ok(())
    }
}

fn wait_for_retry(
    policy: &RetryPolicy,
    artifact: &CanonicalRoomArtifact,
    attempt: u32,
    cancellation: &TransferCancellation,
) -> Result<(), TransferEngineError> {
    let exponent = attempt.saturating_sub(1).min(20);
    let exponential = policy
        .base_backoff
        .saturating_mul(1_u32 << exponent)
        .min(policy.max_backoff);
    // Stable per-content jitter spreads retries without another RNG or secret process state.
    let seed = artifact.content_hash.as_str().as_bytes()[attempt as usize % 64] as u32;
    let percent = 75 + seed % 26;
    let delay = exponential.saturating_mul(percent) / 100;
    let started = Instant::now();
    while started.elapsed() < delay {
        check_cancelled(cancellation)?;
        std::thread::sleep((delay - started.elapsed()).min(Duration::from_millis(50)));
    }
    Ok(())
}

fn ensure_disk_space(
    path: &Path,
    remaining_bytes: u64,
    reserve_bytes: u64,
) -> Result<(), TransferEngineError> {
    let required = remaining_bytes.saturating_add(reserve_bytes);
    if let Some(available) = available_disk_space(path)?
        && available < required
    {
        return Err(TransferEngineError::InsufficientDiskSpace {
            required_bytes: required,
            available_bytes: available,
        });
    }
    Ok(())
}

#[cfg(windows)]
fn available_disk_space(path: &Path) -> io::Result<Option<u64>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0_u64;
    // SAFETY: wide is null-terminated and available points to writable u64 storage. The optional
    // total-size outputs are null because the transfer needs only caller-available bytes.
    if unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(Some(available))
    }
}

#[cfg(not(windows))]
fn available_disk_space(_path: &Path) -> io::Result<Option<u64>> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room_sync::RoomModFormat;
    use std::collections::HashMap;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::thread;

    #[derive(Debug)]
    struct TestRequest {
        method: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    struct TestResponse {
        status: u16,
        reason: &'static str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl TestResponse {
        fn ok(body: impl Into<Vec<u8>>) -> Self {
            Self {
                status: 200,
                reason: "OK",
                headers: Vec::new(),
                body: body.into(),
            }
        }

        fn status(status: u16, reason: &'static str) -> Self {
            Self {
                status,
                reason,
                headers: Vec::new(),
                body: Vec::new(),
            }
        }

        fn header(mut self, name: &str, value: impl Into<String>) -> Self {
            self.headers.push((name.to_string(), value.into()));
            self
        }
    }

    fn serve(
        requests: usize,
        handler: impl Fn(TestRequest) -> TestResponse + Send + Sync + 'static,
    ) -> (TransferEndpoint, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        let thread = thread::spawn(move || {
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                let response = handler(request);
                write_response(&mut stream, response);
            }
        });
        (
            TransferEndpoint::parse(&format!("http://{address}/blob?signature=secret")).unwrap(),
            thread,
        )
    }

    fn read_request(stream: &mut TcpStream) -> TestRequest {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "connection ended before HTTP headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let head = std::str::from_utf8(&bytes[..header_end]).unwrap();
        let mut lines = head.split("\r\n");
        let method = lines
            .next()
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .to_string();
        let headers: HashMap<_, _> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let content_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        while bytes.len() - header_end < content_length {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "connection ended before HTTP body");
            bytes.extend_from_slice(&buffer[..read]);
        }
        TestRequest {
            method,
            headers,
            body: bytes[header_end..header_end + content_length].to_vec(),
        }
    }

    fn write_response(stream: &mut TcpStream, response: TestResponse) {
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            response.reason,
            response.body.len()
        )
        .unwrap();
        for (name, value) in response.headers {
            write!(stream, "{name}: {value}\r\n").unwrap();
        }
        stream.write_all(b"\r\n").unwrap();
        stream.write_all(&response.body).unwrap();
        stream.flush().unwrap();
    }

    fn artifact(bytes: &[u8]) -> CanonicalRoomArtifact {
        CanonicalRoomArtifact {
            content_hash: ContentHash::from_reader(bytes).unwrap(),
            size_bytes: bytes.len() as u64,
            format: RoomModFormat::Modpkg,
        }
    }

    fn setup(directory: &Path, bytes: &[u8]) -> (RoomStateStore, RoomCache, CanonicalRoomArtifact) {
        let artifact = artifact(bytes);
        let library = directory.join("library");
        fs::create_dir_all(&library).unwrap();
        let cache = RoomCache::open(directory.join("cache"), &[library]).unwrap();
        let partial = cache.prepare_partial(&artifact.content_hash).unwrap();
        fs::write(partial, bytes).unwrap();
        cache.commit_partial(&artifact).unwrap();
        let store = RoomStateStore::open(directory.join("rooms.sqlite3")).unwrap();
        store.join_room("room_a", "member-1").unwrap();
        (store, cache, artifact)
    }

    fn download_setup(
        directory: &Path,
        bytes: &[u8],
    ) -> (RoomStateStore, RoomCache, CanonicalRoomArtifact) {
        let artifact = artifact(bytes);
        let library = directory.join("library");
        fs::create_dir_all(&library).unwrap();
        let cache = RoomCache::open(directory.join("cache"), &[library]).unwrap();
        let store = RoomStateStore::open(directory.join("rooms.sqlite3")).unwrap();
        store.join_room("room_a", "member-1").unwrap();
        (store, cache, artifact)
    }

    fn policy(max_attempts: u32) -> RetryPolicy {
        RetryPolicy {
            max_concurrent: 2,
            max_attempts,
            base_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(5),
            buffer_bytes: 4,
            disk_reserve_bytes: 0,
        }
    }

    #[test]
    fn download_resumes_with_range_and_etag_then_commits_verified_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"hello world";
        let (store, cache, artifact) = download_setup(directory.path(), bytes);
        let partial = cache.prepare_partial(&artifact.content_hash).unwrap();
        fs::write(&partial, &bytes[..5]).unwrap();
        let mut pending = PendingTransfer::new("room_a", &artifact, TransferDirection::Download);
        pending.state = TransferState::Paused;
        pending.transferred_bytes = 5;
        pending.etag = Some("\"version-1\"".to_string());
        store.save_transfer(pending).unwrap();

        let (endpoint, server) = serve(1, move |request| {
            assert_eq!(request.method, "GET");
            assert_eq!(
                request.headers.get("range").map(String::as_str),
                Some("bytes=5-")
            );
            assert_eq!(
                request.headers.get("if-range").map(String::as_str),
                Some("\"version-1\"")
            );
            TestResponse::status(206, "Partial Content")
                .header("Content-Range", "bytes 5-10/11")
                .header("ETag", "\"version-1\"")
                .header("Accept-Ranges", "bytes")
                .with_body(b" world")
        });
        let engine = HttpTransferEngine::new(policy(1)).unwrap();
        let outcome = engine
            .download(
                TransferContext::new(&store, &cache, "room_a", &TransferCancellation::default()),
                &artifact,
                &endpoint,
            )
            .unwrap();
        server.join().unwrap();

        assert!(matches!(outcome, TransferOutcome::Downloaded(_)));
        assert_eq!(
            fs::read(cache.blob_path(&artifact.content_hash)).unwrap(),
            bytes
        );
        assert!(store.pending_transfers("room_a").unwrap().is_empty());
    }

    #[test]
    fn retry_recovers_from_a_transient_server_failure() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"retry me";
        let (store, cache, artifact) = download_setup(directory.path(), bytes);
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = Arc::clone(&calls);
        let (endpoint, server) = serve(2, move |request| {
            assert_eq!(request.method, "GET");
            if server_calls.fetch_add(1, AtomicOrdering::SeqCst) == 0 {
                TestResponse::status(503, "Unavailable")
            } else {
                TestResponse::ok(bytes.to_vec()).header("ETag", "\"retry-v1\"")
            }
        });
        let engine = HttpTransferEngine::new(policy(2)).unwrap();
        engine
            .download(
                TransferContext::new(&store, &cache, "room_a", &TransferCancellation::default()),
                &artifact,
                &endpoint,
            )
            .unwrap();
        server.join().unwrap();
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn upload_uses_server_offset_and_requires_sha256_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"abcdefgh";
        let (store, cache, artifact) = setup(directory.path(), bytes);
        let expected_hash = artifact.content_hash.to_string();
        let (endpoint, server) = serve(2, move |request| match request.method.as_str() {
            "HEAD" => TestResponse::ok([])
                .header("Upload-Offset", "3")
                .header("ETag", "\"upload-v1\""),
            "PUT" => {
                assert_eq!(
                    request.headers.get("content-range").map(String::as_str),
                    Some("bytes 3-7/8")
                );
                assert_eq!(
                    request.headers.get("if-match").map(String::as_str),
                    Some("\"upload-v1\"")
                );
                assert_eq!(request.body, b"defgh");
                TestResponse::ok([]).header("X-Content-SHA256", expected_hash.clone())
            }
            method => panic!("unexpected method {method}"),
        });
        let engine = HttpTransferEngine::new(policy(1)).unwrap();
        let outcome = engine
            .upload(
                TransferContext::new(&store, &cache, "room_a", &TransferCancellation::default()),
                &artifact,
                &endpoint,
            )
            .unwrap();
        server.join().unwrap();
        assert_eq!(outcome, TransferOutcome::Uploaded { size_bytes: 8 });
        assert!(store.pending_transfers("room_a").unwrap().is_empty());
    }

    #[test]
    fn successful_status_without_a_valid_receipt_does_not_complete_upload() {
        let directory = tempfile::tempdir().unwrap();
        let bytes = b"upload bytes";
        let (store, cache, artifact) = setup(directory.path(), bytes);
        let (endpoint, server) = serve(2, move |request| match request.method.as_str() {
            "HEAD" => TestResponse::ok([]).header("Upload-Offset", "0"),
            "PUT" => TestResponse::ok([]),
            method => panic!("unexpected method {method}"),
        });
        let engine = HttpTransferEngine::new(policy(1)).unwrap();
        assert!(matches!(
            engine.upload(
                TransferContext::new(&store, &cache, "room_a", &TransferCancellation::default(),),
                &artifact,
                &endpoint,
            ),
            Err(TransferEngineError::Protocol(_))
        ));
        server.join().unwrap();
    }

    #[test]
    fn cancellation_is_observed_before_network_or_state_changes() {
        let directory = tempfile::tempdir().unwrap();
        let (store, cache, artifact) = download_setup(directory.path(), b"cancelled");
        let cancellation = TransferCancellation::default();
        cancellation.cancel();
        let endpoint = TransferEndpoint::parse("http://127.0.0.1:1/blob").unwrap();
        let engine = HttpTransferEngine::new(policy(1)).unwrap();

        assert!(matches!(
            engine.download(
                TransferContext::new(&store, &cache, "room_a", &cancellation),
                &artifact,
                &endpoint,
            ),
            Err(TransferEngineError::Cancelled)
        ));
        assert!(store.pending_transfers("room_a").unwrap().is_empty());
    }

    #[test]
    fn limiter_wait_can_be_cancelled_without_exceeding_capacity() {
        let limiter = Arc::new(TransferLimiter::new(1));
        let held = limiter.acquire(&TransferCancellation::default()).unwrap();
        let cancellation = TransferCancellation::default();
        let worker_cancellation = cancellation.clone();
        let worker_limiter = Arc::clone(&limiter);
        let worker = thread::spawn(move || worker_limiter.acquire(&worker_cancellation));
        thread::sleep(Duration::from_millis(25));
        cancellation.cancel();

        assert!(matches!(
            worker.join().unwrap(),
            Err(TransferEngineError::Cancelled)
        ));
        drop(held);
        assert_eq!(*limiter.active.lock().unwrap(), 0);
    }

    #[test]
    fn endpoints_reject_plain_http_except_for_loopback_and_hide_signed_queries() {
        assert!(TransferEndpoint::parse("http://example.com/blob").is_err());
        let endpoint =
            TransferEndpoint::parse("https://example.com/blob?signature=secret").unwrap();
        assert_eq!(format!("{endpoint:?}"), "TransferEndpoint([REDACTED])");
        assert!(!format!("{endpoint:?}").contains("secret"));
    }

    impl TestResponse {
        fn with_body(mut self, body: impl Into<Vec<u8>>) -> Self {
            self.body = body.into();
            self
        }
    }
}
