# Room synchronization checklist

The room feature synchronizes immutable mod files and an optional profile suggestion. It does not
create a VPN, alter League files, build an overlay, start the patcher, switch the active profile, or
launch the game. Every action that prepares or applies synchronized mods remains local and initiated
by the user.

## End-to-end checklist

1. [x] Define the safety boundary and shared vocabulary.
   - A room owns file references and revisions, never game or patcher commands.
   - A synchronized file is not an installed or applied mod.
   - Remote messages cannot mutate the active library/profile.

2. [x] Add the versioned room manifest and content identity to `ltk-manager-core`.
   - Implementation formatted and verified by the targeted Rust unit tests.
   - Identify blobs by SHA-256 rather than local mod UUIDs.
   - Validate schema version, revision, room ID, file sizes, duplicate hashes, metadata, and layers.
   - Verify downloaded files by both byte length and SHA-256.

3. [x] Define and test the canonical room artifact.
   - Decision recorded in ADR-0035: transport the exact installed library archive, preserving its
     `.modpkg` or normalized `.fantome` format.
   - Fingerprinting and verification are covered by the targeted Rust unit tests.
   - Record both source metadata and canonical blob hash.

4. [x] Add an isolated room cache.
   - Store blobs under a content-addressed path outside the installed mod library.
   - Use `.part` files while downloading and atomic rename after verification.
   - Track references so leaving a room never deletes a user's personal library copy.
   - Implemented with protected-root overlap checks, link escape checks, explicit pruning, and unit
     coverage for shared references and personal-library isolation.

5. [x] Add persistent local room state.
   - Persist joined rooms, last accepted revision, member identity, pending transfers, and cache
     references in SQLite.
   - Store access/owner tokens in Windows Credential Manager, not in JSON or logs.
   - Recover cleanly after an app crash or interrupted update.
   - Implemented with bundled SQLite, WAL, foreign keys, versioned migration, transactional manifest
     acceptance, resumable transfer recovery, and redacted/zeroed credential values.

6. [x] Implement the resumable transfer engine.
   - Bounded parallel downloads and uploads with cancellation and backpressure.
   - HTTP Range/ETag resume, retry with jitter, bandwidth-friendly progress reporting, and disk-space
     preflight.
   - Never trust only filename, MIME type, `Content-Length`, or server success status.
   - Implemented with HTTPS-only remote endpoints, redacted signed URLs, SQLite checkpoints, final
     SHA-256 verification, and an explicit upload offset/receipt contract recorded in ADR-0036.

7. [x] Implement the room client state machine.
   - States: disconnected, connecting, comparing, transferring, verifying, synchronized, stale, and
     blocked.
   - Accept a revision atomically only after every referenced blob is verified.
   - Keep the previous complete revision usable while a new revision downloads.
   - Implemented with durable two-phase staging, accepted-plus-staged cache retention, restart
     recovery, explicit transition errors, and local length/SHA-256 re-verification documented in
     ADR-0037.

8. [x] Implement explicit local preparation/import.
   - Provide a user action to prepare a synchronized revision in the local library.
   - Reuse the existing staging and archive validation pipeline.
   - Map shared content hashes to local UUIDs without assuming IDs match across machines.
   - Do not automatically enable mods, switch profiles, rebuild overlays, or start the patcher.
   - Implemented with accepted-manifest persistence, verified extensionless-cache imports, disabled
     registration in every profile, durable SHA-256-to-local-UUID mappings, and idempotent recovery
     documented in ADR-0038.

9. [x] Add a dedicated room profile workflow.
   - Create/update a non-active room profile transactionally after explicit user confirmation.
   - Preserve exact mod order and suggested layer states.
   - Let the user select that profile and press the existing Start/Play control themselves.
   - Implemented with a durable room-to-profile binding, a recoverable per-profile room marker,
     pinned manifest order, complete layer maps, active-profile update protection, and the decision
     recorded in ADR-0039.

10. [x] Add Tauri commands, backend events, and permissions.

- Expose create/join/leave/sync/cache/import operations through typed IPC.
- Emit throttled transfer and presence progress events.
- Keep network payloads out of commands that control the patcher, launcher, settings, and active
  profile.
- Implemented as an isolated `tauri-specta` command group backed by separate SQLite/cache
  state outside `mods/` and `archives`, typed generated bindings, generic room error copy, and
  main-window-only Tauri capability. The boundary is recorded in ADR-0040.

11. [x] Build the room user interface.
    - Create/join by room code and password.
    - Show members, manifest revision, per-member synchronization status, files, size, and progress.
    - Clearly distinguish `Synchronized`, `Prepared in library`, and `Applied by Start/Play`.
    - Require a local click for all preparation/import actions.
    - Implemented the `/rooms` workspace with a truthful local-draft state while the authenticated
      service is still pending in steps 12-13: the password field remains disabled, no credential is
      accepted or persisted, and remote member rows are never simulated. Durable local preparation
      and non-active profile bindings drive the workflow status after restart.

12. [x] Create the server foundation.
    - Rust service using Axum, Tokio, Tower, SQLx, and PostgreSQL.
    - Versioned HTTPS endpoints plus authenticated WebSocket events.
    - Database migrations, health/readiness endpoints, structured errors, and graceful shutdown.
    - Implemented in `services/room-server` containerized via Docker Compose with PostgreSQL 16
      Alpine, automatic migrations, health and readiness endpoints, and deployed live to the Ubuntu VPS
      at `http://177.153.59.168:3000`. Fully documented in `docs/ops/server-runbook.md`.

13. [x] Implement room authentication and roles.
    - Hash room passwords with Argon2id and rate-limit join attempts.
    - Use separate high-entropy member and owner tokens with short-lived sessions.
    - Roles: owner and member; only the owner may publish a new manifest revision.
    - Do not request or store Riot credentials.
    - Implemented in `services/room-server` with Argon2id password hashing, 256-bit CSPRNG tokens,
      in-memory sliding window rate limiting (5 attempts/min -> 429), `POST /v1/rooms`, `POST /v1/rooms/:id/join`,
      authenticated metadata query, and WebSocket token verification. Tested and live on VPS.

14. [x] Implement authoritative revisions and presence.
    - Monotonic revisions with compare-and-swap updates to prevent two writers from racing.
    - Member acknowledgements identify the exact verified revision.
    - Reconnect, owner disconnect, ownership transfer, room expiry, and stale-member behavior.
    - Implemented in `services/room-server` with CAS revision validation (`previous_revision` check,
      `409 Conflict`), `POST /v1/rooms/:id/manifest`, `GET /v1/rooms/:id/manifest`,
      `POST /v1/rooms/:id/ack`, `GET /v1/rooms/:id/members`, `POST /v1/rooms/:id/transfer_owner`,
      real-time WebSocket presence broadcasting (`member_presence`, `owner_disconnected`,
      `member_acknowledged`), 24-hour inactivity room expiration with automatic background pruning,
      and migration 000003. Tested live on VPS at `http://177.153.59.168:3000`.

15. [ ] Integrate content-addressed object storage.
    - S3-compatible storage with short-lived signed upload/download URLs.
    - Upload only missing hashes; download through HTTPS rather than WebSocket.
    - Quotas, retention, orphan cleanup, and optional CDN delivery.

16. [ ] Enforce server and client authorization.
    - A member can access only blobs referenced by a room they have joined.
    - Prevent arbitrary remote URLs, SSRF, path injection, cross-room blob enumeration, and replay of
      expired upload grants.
    - Audit owner actions without logging passwords, tokens, or local filesystem paths.

17. [ ] Harden untrusted file handling.
    - Allow only supported archive formats and reject empty, oversized, malformed, or ambiguous
      files.
    - Limit archive entry count, expanded size, compression ratio, path length, nesting, and parser
      time/memory.
    - Reject traversal paths, links/reparse points, and executable payloads.
    - Validate in a non-elevated isolated worker before the file reaches the library.

18. [ ] Add compatibility and safety gates.
    - Report app protocol and game-build mismatches without applying anything automatically.
    - Keep `Scripts.wad.client` blocked and the anti-skinhack scan enforced for room content.
    - Queue updates while the patcher is running; never hot-swap a live session from a room event.

19. [ ] Add observability and production operations.
    - Tracing, OpenTelemetry, Prometheus metrics, dashboards, alerts, and privacy-safe error reports.
    - Database backups, object lifecycle policies, secret rotation, deployment rollback, and capacity
      limits.
    - Terms/privacy, content rights attestation, abuse reporting, and takedown process before public
      file hosting.

20. [ ] Complete verification and staged release.
    - Unit, property, integration, protocol-compatibility, and two-machine end-to-end tests.
    - Fault injection for disconnects, corrupt chunks, disk-full, stale revisions, and server restart.
    - Security review, dependency audit, signed builds, private alpha, limited beta, and monitored
      production rollout.

## Delivery slices

- Slice A: items 1-4 — trustworthy local artifacts and cache, no server.
- Slice B: items 5-10 — complete desktop workflow against a fake/local transport.
- Slice C: items 11-16 — UI and production room service.
- Slice D: items 17-20 — hardening, operations, and release.
