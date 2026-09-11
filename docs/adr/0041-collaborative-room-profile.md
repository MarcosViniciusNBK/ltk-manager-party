# ADR 0041: Collaborative room profiles converge automatically

## Status

Accepted

## Context

The earlier room workflow required the owner to publish and every member to manually download,
prepare, and create a local profile. That did not implement a shared room profile: it only exposed
separate synchronization primitives.

## Decision

- Creating a room requires a source profile and publishes it as revision 1.
- Joining downloads and verifies the current revision, prepares its archives through the existing
  importer, and creates a dedicated local room profile automatically.
- Every authenticated member may publish changes to that dedicated profile.
- Compare-and-swap revisions serialize concurrent writes. A failed writer retries from the latest
  server revision; the last successfully published complete profile wins.
- The desktop reconciles joined rooms in the background. Local profile changes are published and
  newer remote revisions are materialized on every member.
- Uploads and downloads are streamed and resumable. A manifest cannot be published until every
  referenced content-addressed blob is complete.
- Synchronization may update a selected room profile's library state, but it never builds an
  overlay, starts or restarts the patcher, launches the game, or invokes Start/Play. Applying the
  synchronized profile remains an explicit user action in the existing flow.

## Consequences

Members converge on the same enabled mods, ordering, and layer selection without repeated manual
imports. Room passwords and tokens remain outside renderer state, and immutable blobs remain
verified by size and SHA-256 before a revision is accepted. The currently deployed IP endpoint uses
plain HTTP for private validation; a trusted TLS hostname is mandatory before any public release.
