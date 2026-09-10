-- Migration 000004: Storage Blobs Metadata and Quotas

ALTER TABLE room_blobs ADD COLUMN IF NOT EXISTS uploaded_by_room_id VARCHAR(64) NULL REFERENCES rooms(room_id) ON DELETE SET NULL;
ALTER TABLE room_blobs ADD COLUMN IF NOT EXISTS last_accessed_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

CREATE INDEX IF NOT EXISTS idx_room_blobs_room ON room_blobs(uploaded_by_room_id);
CREATE INDEX IF NOT EXISTS idx_room_blobs_accessed ON room_blobs(last_accessed_at);
