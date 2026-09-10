-- Migration 000002: Auth Indexes and Token Expiration

ALTER TABLE room_members ADD COLUMN IF NOT EXISTS token_expires_at TIMESTAMPTZ NULL;

CREATE INDEX IF NOT EXISTS idx_rooms_owner_token ON rooms(owner_token);
CREATE INDEX IF NOT EXISTS idx_room_members_token ON room_members(room_id, member_token);
