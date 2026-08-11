-- Mode A OTP hash store (PR 14 / DESIGN.md otp_codes).
-- In-memory store is used for single-node tests; this table supports Postgres path.

CREATE TABLE IF NOT EXISTS otp_codes (
    id                  BIGSERIAL PRIMARY KEY,
    host_device_id      BIGINT NOT NULL REFERENCES devices (id) ON DELETE CASCADE,
    digest              BYTEA NOT NULL,
    salt                BYTEA NOT NULL,
    keyed               BOOLEAN NOT NULL DEFAULT TRUE,
    session_intent_id   UUID,
    expires_at          TIMESTAMPTZ NOT NULL,
    consumed_at         TIMESTAMPTZ,
    attempts            INT NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX IF NOT EXISTS otp_expires_idx ON otp_codes (expires_at);
CREATE INDEX IF NOT EXISTS otp_host_active_idx
    ON otp_codes (host_device_id)
    WHERE consumed_at IS NULL;
