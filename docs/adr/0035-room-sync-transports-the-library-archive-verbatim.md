# ADR-0035: Room sync transports the library archive verbatim

Status: accepted (2026-09-10)

## Context

A room must give every member the same immutable mod bytes without installing or applying them.
The library accepts both `.modpkg` and `.fantome`, and those formats have different contracts:

- A modpkg is mountable and remains the source of truth for its content.
- A fantome arriving through the normal install path is copied through name preservation and archive
  normalization before it becomes the library's source of truth.
- A project-storage fantome can be repacked, at which point that new archive is the only packed
  representation of the current project.

Converting every room artifact to modpkg would introduce a second representation, make fidelity a
property of a conversion path, and duplicate work already owned by the import/packing code. Hashing
the file originally selected by a publisher is also insufficient: for a fantome, those are not
necessarily the bytes the library kept after preservation and normalization.

## Decision

**The canonical room artifact is the installed library archive, copied verbatim after the normal
local import or repack has completed.** Its identity is the SHA-256 digest of every byte in that
archive. Its length, format, display metadata, suggested layer selection, and suggested priority are
recorded by the room manifest.

Both formats remain themselves in transport:

- A modpkg is published as `.modpkg`.
- A fantome is published as `.fantome` after the existing preservation/normalization path has made
  it the library's source of truth.

Room storage is content-addressed by the digest. Upload and download code must treat the blob as
immutable and must verify both its length and digest. The server does not unpack, normalize, repair,
or rename its contents.

The receiving client first writes the exact blob to the isolated room cache. Synchronization ends
there. A later explicit user action may ask the existing library pipeline to prepare/import the mod,
but that action is not part of room synchronization and must not switch a profile, build an overlay,
start the patcher, or launch the game.

Because the normal generic install path is allowed to preserve or normalize archives, the future
room-import path must not use a possibly rewritten library copy as proof that the cached room blob
changed. The immutable cached blob remains the shared identity. If an import transforms a copy, the
client verifies the resulting logical mod separately while retaining the original room blob.

## Consequences

Members download exactly the bytes the publisher's working library used. Existing mods need no
format migration, and room synchronization stays independent from overlay and patcher behavior.

The same logical mod packed twice may produce two byte hashes if archive metadata or packing order
differs. That is intentional: the room promises exact files, so different bytes are different room
artifacts even when they might render identically.

Project-storage mods have no archive to publish until the user explicitly repacks or exports them.
Publishing must report that condition rather than silently inventing an archive or changing the
mod's storage mode.
