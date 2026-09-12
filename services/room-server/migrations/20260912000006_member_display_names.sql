-- Visible member names are separate from opaque member IDs. This avoids collisions when two
-- machines share a hostname while still letting a room show recognizable computer names.
ALTER TABLE room_members
    ADD COLUMN IF NOT EXISTS display_name VARCHAR(64) NOT NULL DEFAULT '';
