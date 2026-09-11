# ADR-0039: Room profiles are explicit, non-active, and pinned

## Status

Superseded by ADR 0041. Room profiles remain dedicated and are never applied automatically, but
creation and updates are now automatic and collaborative.

## Context

An accepted room revision may be fully prepared in the local library without being appropriate to
apply. The library normally derives each profile's mod order from the user's folder arrangement,
but a room needs the manifest's exact order and the manifest's suggested layer selection. Updating
the active profile as a side effect of a room event or preparation would alter what Start/Play
uses, violating the local confirmation boundary.

The room SQLite database and the library index cannot share one transaction. A failure between
writing either durable store must therefore recover to one identifiable local room profile rather
than creating a duplicate or silently selecting it.

## Decision

The application exposes an explicit local action that creates or updates a dedicated room profile
only for the current accepted revision after it is completely prepared.

The action:

1. Requires the accepted manifest, matching prepared revision, and a complete content-hash to
   local-UUID mapping.
2. Creates a profile marked by room ID on disk, or recovers that marker to update the same profile
   after an interrupted cross-store write.
3. Marks the profile `RoomPinned`, records the manifest's UUID order in both `mod_order` and
   `enabled_mods`, and writes a complete layer map: suggested layers are `true`, all other declared
   layers are `false`.
4. Does not select the profile, touch the active profile, rebuild an overlay, start the patcher, or
   launch the game. If the existing room profile is active, an update is refused until the user
   switches away.
5. Binds the profile ID and revision in room SQLite only after the local profile operation succeeds.

Library folder changes preserve `RoomPinned` profile order; they only remove references to local
mods that no longer exist. The normal user profile mode keeps its existing folder-derived behavior.

## Consequences

- Preparing and applying remain separate, user-visible decisions.
- Every member can reproduce the room's intended mod ordering and layer selection despite local
  UUIDs and personal folder arrangements being different.
- A newer room revision cannot silently modify a profile currently selected for Start/Play.
- A retry after a process interruption updates the marked profile instead of adding another room
  profile.
