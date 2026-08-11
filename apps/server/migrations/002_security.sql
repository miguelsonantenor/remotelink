-- Auth attempts, blocklist, and audit events (PR 6 / DESIGN.md data model).
-- In-memory stores are used for single-node tests; these tables support Postgres path.

CREATE TABLE IF NOT EXISTS auth_attempts (
    id                  BIGSERIAL PRIMARY KEY,
    host_device_id      BIGINT REFERENCES devices (id) ON DELETE SET NULL,
    viewer_ip_hash      TEXT,
    success             BOOLEAN NOT NULL,
    reason              TEXT,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS auth_attempts_host_time
    ON auth_attempts (host_device_id, created_at);

CREATE TABLE IF NOT EXISTS blocklist (
    id                  BIGSERIAL PRIMARY KEY,
    host_device_id      BIGINT NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    subject_type        TEXT NOT NULL, -- ip|viewer_fingerprint|device
    subject_hash        TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (host_device_id, subject_type, subject_hash)
);

CREATE INDEX IF NOT EXISTS blocklist_host_idx ON blocklist (host_device_id);

CREATE TABLE IF NOT EXISTS audit_events (
    id                  BIGSERIAL PRIMARY KEY,
    device_id           BIGINT REFERENCES devices (id) ON DELETE SET NULL,
    session_id          UUID,
    event_type          TEXT NOT NULL,
    meta                JSONB,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS audit_events_device_time
    ON audit_events (device_id, created_at DESC);
CREATE INDEX IF NOT EXISTS audit_events_type_time
    ON audit_events (event_type, created_at DESC);
