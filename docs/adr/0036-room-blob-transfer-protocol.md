# ADR-0036: Resume room blobs with HTTP ranges and verified receipts

## Status

Accepted

## Context

Room blobs can be large, connections can fail, and a successful HTTP status does not prove that the
bytes match the room manifest. Transfers must resume without putting remote filenames or URLs into
the filesystem and without allowing a downloaded file into the installed mod library.

## Decision

The desktop transfer engine uses HTTPS for remote endpoints. Plain HTTP is accepted only for a
loopback address used by local development and tests. Endpoint debug output is redacted because a
signed query string is a credential.

Downloads use this contract:

1. New transfers send `GET` with `Accept-Encoding: identity`.
2. A partial transfer sends `Range: bytes=<offset>-` and `If-Range: <etag>`.
3. A resumed response must be `206 Partial Content`, preserve the ETag, and provide a matching
   `Content-Range`. A `200` response or changed/missing ETag restarts safely from byte zero.
4. Bytes stream through a bounded buffer into the cache `.part` file. Progress and the ETag are
   checkpointed in SQLite.
5. Only exact length and SHA-256 verification can promote the `.part` file into cache.

Uploads use this application-level resumable contract:

1. `HEAD` returns the server-confirmed `Upload-Offset` and, when applicable, an ETag.
2. `PUT` sends the remaining bytes with `Content-Range` and `If-Match`.
3. A successful final response must include `X-Content-SHA256` equal to the manifest hash. A success
   status without the receipt is not completion.

The default client permits three concurrent transfers. Each stream has cooperative cancellation,
bounded memory, retry with exponential per-content jitter, and a disk-space preflight. Retryable
statuses are 408, 409, 412, 425, 429, and 5xx. State remains resumable after cancellation or process
restart.

## Consequences

- The future room service and object-storage adapter must implement the upload probe/receipt
  headers, even when storage is backed by signed URLs.
- Downloads remain compatible with standard HTTP range servers.
- Progress callbacks are rate-limited and can later be mapped to throttled Tauri events.
- Transfer completion still does not install, enable, apply, or launch a mod.
