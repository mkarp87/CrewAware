-- Repeater login passwords (encrypted by the app with a key from the environment) and the latest
-- reading from each repeater.

CREATE TABLE repeater_credentials (
    repeater_id text PRIMARY KEY REFERENCES repeaters(id) ON DELETE CASCADE,
    nonce       bytea NOT NULL,
    ciphertext  bytea NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE repeater_status (
    repeater_id text PRIMARY KEY REFERENCES repeaters(id) ON DELETE CASCADE,
    read_at     timestamptz NOT NULL,
    ok          boolean NOT NULL,
    error       text NOT NULL DEFAULT '',
    data        jsonb NOT NULL DEFAULT 'null'
);
