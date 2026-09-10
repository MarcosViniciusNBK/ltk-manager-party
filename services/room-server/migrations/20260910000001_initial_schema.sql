-- Room Synchronization schema (Etapas 12-16)

CREATE TABLE IF NOT EXISTS rooms (
    room_id VARCHAR(64) PRIMARY KEY,
    revision BIGINT NOT NULL DEFAULT 0,
    password_hash VARCHAR(255) NULL,
    owner_token VARCHAR(255) NULL,
    game_build VARCHAR(64) NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE IF NOT EXISTS room_members (
    room_id VARCHAR(64) NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
    member_id VARCHAR(64) NOT NULL,
    member_token VARCHAR(255) NULL,
    role VARCHAR(32) NOT NULL DEFAULT 'member',
    last_acknowledged_revision BIGINT NOT NULL DEFAULT 0,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (room_id, member_id)
);

CREATE TABLE IF NOT EXISTS room_manifests (
    room_id VARCHAR(64) NOT NULL REFERENCES rooms(room_id) ON DELETE CASCADE,
    revision BIGINT NOT NULL,
    schema_version INT NOT NULL DEFAULT 1,
    game_build VARCHAR(64) NULL,
    manifest_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (room_id, revision)
);

CREATE TABLE IF NOT EXISTS room_blobs (
    content_hash VARCHAR(64) PRIMARY KEY,
    size_bytes BIGINT NOT NULL,
    format VARCHAR(32) NOT NULL,
    storage_path TEXT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS idx_room_members_seen ON room_members(last_seen_at);
CREATE INDEX IF NOT EXISTS idx_room_manifests_lookup ON room_manifests(room_id, revision DESC);
