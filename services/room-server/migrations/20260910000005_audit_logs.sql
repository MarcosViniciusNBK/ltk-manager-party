-- Migration 000005: Audit Logs for Room and Owner Operations

CREATE TABLE IF NOT EXISTS room_audit_logs (
    id BIGSERIAL PRIMARY KEY,
    room_id VARCHAR(64) NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
    actor_member_id VARCHAR(64) NOT NULL,
    actor_role VARCHAR(32) NOT NULL,
    action VARCHAR(64) NOT NULL,
    details JSONB NULL,
    client_ip VARCHAR(45) NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_room_audit_logs_lookup ON room_audit_logs(room_id, created_at DESC);
