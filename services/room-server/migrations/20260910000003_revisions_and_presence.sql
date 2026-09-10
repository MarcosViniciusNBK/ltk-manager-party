-- Migration 000003: Revisions, Presence, and Expiration

ALTER TABLE rooms ADD COLUMN IF NOT EXISTS expires_at TIMESTAMPTZ NOT NULL DEFAULT (NOW() + INTERVAL '24 hours');
CREATE INDEX IF NOT EXISTS idx_rooms_expires_at ON rooms(expires_at);

ALTER TABLE room_members ADD COLUMN IF NOT EXISTS ack_status VARCHAR(32) NOT NULL DEFAULT 'joined';
CREATE INDEX IF NOT EXISTS idx_room_members_seen_room ON room_members(room_id, last_seen_at);
