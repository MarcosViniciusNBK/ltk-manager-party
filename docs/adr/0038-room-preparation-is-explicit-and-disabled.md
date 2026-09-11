# ADR-0038: Room preparation is explicit, local, and disabled by default

## Status

Superseded by ADR 0041. Preparation still imports safely as disabled library content, but room
membership now invokes it automatically so every member receives the collaborative profile.

## Context

Synchronization proves that the room cache contains the accepted revision, but cached content is
not yet a library mod. The existing install pipeline validates archives, preserves Fantome names,
normalizes its private copy, extracts metadata, assigns a local UUID, and updates the library index.
Its normal user-import path also enables the new mod in the active profile, which a remote room must
never cause implicitly.

Room cache paths are content-addressed and have no extension, so the importer also cannot infer
their format from the filename. Shared hashes are stable across members, while library UUIDs are
intentionally local and can differ on every machine.

## Decision

The application exposes a distinct core action to prepare the currently accepted room revision.
Nothing invokes it from synchronization or connection state changes; a later UI command must call
it only after an explicit user action.

Preparation follows this sequence:

1. Load the complete locally accepted manifest and verify every cache blob again by length and
   SHA-256 before changing the library.
2. Pass the manifest-declared `.modpkg` or `.fantome` format directly to the existing staging and
   archive-validation pipeline; never guess it from the extensionless cache path.
3. Register each imported mod in the library's visible order but leave it disabled in every
   profile. Do not change the active profile or any layer state.
4. Store a local marker containing the shared source hash and the hash of the pipeline's resulting
   archive. A retry reuses the local UUID only while that archive remains unchanged and packed.
5. After every mod succeeds, atomically persist the complete shared-hash-to-local-UUID mapping for
   the revision in the room SQLite database.

The immutable cache blob is never normalized or moved. Fantome preservation and normalization
operate only on the staging copy, matching the normal import contract. If preparation stops after
some imports, the revision is not marked prepared; retrying safely reuses intact completed imports.

## Consequences

- `Synchronized` and `Prepared in library` remain distinct durable states.
- Different members may have different local UUIDs for the same room hash without losing order or
  identity.
- User-modified library copies are not overwritten or falsely reused; preparation creates a new
  disabled local copy when needed.
- Preparation never enables a mod, switches a profile, builds an overlay, starts the patcher, or
  launches the game.
