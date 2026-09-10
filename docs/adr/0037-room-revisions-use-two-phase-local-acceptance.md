# ADR-0037: Accept room revisions locally in two phases

## Status

Accepted

## Context

A room member can lose connectivity or close the application while a new revision is downloading.
The last complete revision must remain usable, and cache cleanup must not remove either its blobs or
the blobs already associated with the pending revision. A server announcement or transfer progress
event is not proof that content is present and intact locally.

## Decision

The room client uses an explicit state machine with these states: disconnected, connecting,
comparing, transferring, verifying, synchronized, stale, and blocked.

Revision changes use two durable phases:

1. After validating a newer manifest, the client stages its complete JSON and content-hash
   references in SQLite. The accepted revision and its references remain unchanged.
2. Each completed transfer is reverified from the isolated cache by exact length and SHA-256. Only
   after every referenced blob passes verification does one SQLite transaction replace the accepted
   references, advance the accepted revision, and delete the staged target.

Cache retention is the union of accepted and staged references. Disconnecting preserves the staged
target and resumable transfer checkpoints. Restarting the application restores both the last
accepted revision and the staged target. Conflicting content cannot reuse an accepted or staged
revision number, and older manifests cannot move state backward.

The machine coordinates room metadata and cache state only. It has no dependency on the installed
mod library, profiles, patcher, launcher, or game paths.

## Consequences

- A partially downloaded update never replaces a complete local revision.
- Cache pruning is safe during an in-progress revision transition.
- Network claims cannot mark a blob verified without checking local bytes.
- A later explicit preparation/import action remains separate from synchronization and is never
  triggered by a state transition.
