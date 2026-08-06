-- Device registry + long-lived credentials (PR 4 / DESIGN.md data model).

CREATE TABLE IF NOT EXISTS devices (
    id                      BIGSERIAL PRIMARY KEY,
    public_id               TEXT NOT NULL UNIQUE,
    display_name            TEXT,
    public_key              BYTEA NOT NULL,
    password_hash           TEXT,
    protocol_version_last   INT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_seen_at            TIMESTAMPTZ,
    status                  TEXT NOT NULL DEFAULT 'active',
    deleted_at              TIMESTAMPTZ,
    active_session_id       UUID
);

CREATE INDEX IF NOT EXISTS devices_public_id_idx ON devices (public_id);
CREATE INDEX IF NOT EXISTS devices_status_idx ON devices (status);

CREATE TABLE IF NOT EXISTS device_credentials (
    id                      BIGSERIAL PRIMARY KEY,
    device_id               BIGINT NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    token_hash              TEXT NOT NULL,
    refresh_token_hash      TEXT NOT NULL,
    expires_at              TIMESTAMPTZ NOT NULL,
    revoked_at              TIMESTAMPTZ,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS device_credentials_device_id_idx
    ON device_credentials (device_id);
CREATE INDEX IF NOT EXISTS device_credentials_token_hash_idx
    ON device_credentials (token_hash)
    WHERE revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS device_credentials_refresh_hash_idx
    ON device_credentials (refresh_token_hash)
    WHERE revoked_at IS NULL;
