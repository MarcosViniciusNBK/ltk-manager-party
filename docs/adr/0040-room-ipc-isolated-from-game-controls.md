# ADR-0040: Room IPC is isolated from game controls

## Status

Accepted

## Context

The room feature needs desktop commands for durable local membership, synchronization state,
cache maintenance, preparation, and a dedicated profile. It must not turn a room manifest, a
future transport response, or a progress event into an implicit game action.

The app's frontend is a webview. IPC errors and events are therefore part of the security boundary:
they must not disclose signed URLs, access tokens, passwords, or local file paths. Tauri's
capability applies only to the `main` window, while application commands are registered in their
own typed handler rather than through a plugin permission manifest.

## Decision

Room commands are a dedicated `tauri-specta` command group backed by an independently managed
`RoomSyncState`. Its SQLite database and content-addressed cache live below the application-data
directory and are rejected if they overlap the installed `mods` or `archives` roots.

The group exposes local draft membership, snapshot and manifest comparison, cache status and
pruning, explicit preparation, and explicit non-active profile creation. It has no command that
selects a profile, writes settings, starts the patcher, rebuilds an overlay, or launches League.

Commands return generated, typed wire shapes. Room failures are reduced to stable categories before
crossing IPC. Events report snapshots, transfer counters, and member transitions only. The desktop
state throttles redundant transfer and presence updates to at most one per 100 milliseconds per
identity while allowing a presence state change immediately.

## Consequences

- A future transport can provide a manifest or transfer progress without access to game controls.
- The UI can distinguish synchronized, prepared, and applied states without receiving local paths
  or room credentials.
- Adding remote room creation, authentication, or signed object URLs requires a separate transport
  boundary, rather than widening this IPC surface.
