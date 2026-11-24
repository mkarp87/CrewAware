-- Radio node keys on units and repeaters, obstacles drawn on the overlay, saved site profiles,
-- and incident tagging on the activity log for reports.

ALTER TABLE units ADD COLUMN node_key text NOT NULL DEFAULT '';
ALTER TABLE repeaters ADD COLUMN node_key text NOT NULL DEFAULT '';
CREATE UNIQUE INDEX units_node_key ON units (node_key) WHERE node_key <> '';
CREATE UNIQUE INDEX repeaters_node_key ON repeaters (node_key) WHERE node_key <> '';

ALTER TABLE activity_log ADD COLUMN incident_id integer;
CREATE INDEX activity_log_incident ON activity_log (incident_id, t) WHERE incident_id IS NOT NULL;

CREATE TABLE obstacles (
    id     integer PRIMARY KEY,
    kind   text NOT NULL CHECK (kind IN ('wall','gate','no_go','slow')),
    name   text NOT NULL DEFAULT '',
    points jsonb NOT NULL,
    open   boolean NOT NULL DEFAULT false,
    factor double precision NOT NULL DEFAULT 1
);

CREATE TABLE site_profiles (
    id       serial PRIMARY KEY,
    name     text NOT NULL UNIQUE,
    data     jsonb NOT NULL,
    saved_at timestamptz NOT NULL DEFAULT now()
);
