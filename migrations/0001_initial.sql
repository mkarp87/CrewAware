-- CrewAware initial schema. Live positions and contact ages are kept in memory; the database holds
-- configuration, incidents, the activity log, a throttled position history and admin accounts.

CREATE TABLE site (
    id        smallint PRIMARY KEY CHECK (id = 1),
    name      text NOT NULL,
    overlay   text NOT NULL DEFAULT '',
    img_w     double precision NOT NULL DEFAULT 1000,
    img_h     double precision NOT NULL DEFAULT 700,
    a_px      double precision NOT NULL DEFAULT 0,
    a_py      double precision NOT NULL DEFAULT 0,
    a_lat     double precision NOT NULL DEFAULT 0,
    a_lon     double precision NOT NULL DEFAULT 0,
    b_px      double precision NOT NULL DEFAULT 0,
    b_py      double precision NOT NULL DEFAULT 0,
    b_lat     double precision NOT NULL DEFAULT 0,
    b_lon     double precision NOT NULL DEFAULT 0,
    logo      text NOT NULL DEFAULT '',
    slogan    text NOT NULL DEFAULT '' CHECK (char_length(slogan) <= 60)
);

CREATE TABLE settings (
    id   smallint PRIMARY KEY CHECK (id = 1),
    data jsonb NOT NULL
);

CREATE TABLE groups (
    id      text PRIMARY KEY,
    name    text NOT NULL,
    max_mph double precision NOT NULL CHECK (max_mph > 0),
    adjust  double precision NOT NULL CHECK (adjust > 0 AND adjust <= 1.5)
);

CREATE TABLE units (
    id          text PRIMARY KEY,
    name        text NOT NULL,
    group_id    text NOT NULL DEFAULT '',
    status      text NOT NULL DEFAULT 'available'
                CHECK (status IN ('available','dispatched','on_scene','busy','off_duty')),
    has_pos     boolean NOT NULL DEFAULT false,
    lat         double precision NOT NULL DEFAULT 0,
    lon         double precision NOT NULL DEFAULT 0,
    speed_mph   double precision NOT NULL DEFAULT 0,
    moving      boolean NOT NULL DEFAULT false,
    battery     integer NOT NULL DEFAULT -1,
    last_seen   timestamptz,
    incident_id integer
);

CREATE TABLE repeaters (
    id        text PRIMARY KEY,
    name      text NOT NULL,
    lat       double precision NOT NULL DEFAULT 0,
    lon       double precision NOT NULL DEFAULT 0,
    placed    boolean NOT NULL DEFAULT false,
    last_seen timestamptz
);

CREATE TABLE incidents (
    id        integer PRIMARY KEY,
    label     text NOT NULL,
    lat       double precision NOT NULL,
    lon       double precision NOT NULL,
    opened    timestamptz NOT NULL,
    closed    timestamptz,
    open      boolean NOT NULL DEFAULT true,
    units     text[] NOT NULL DEFAULT '{}',
    opened_by text NOT NULL DEFAULT ''
);

CREATE TABLE activity_log (
    id   bigserial PRIMARY KEY,
    t    timestamptz NOT NULL,
    seat text NOT NULL,
    text text NOT NULL
);
CREATE INDEX activity_log_t ON activity_log (t DESC);

CREATE TABLE position_history (
    id        bigserial PRIMARY KEY,
    unit_id   text NOT NULL,
    at        timestamptz NOT NULL,
    lat       double precision NOT NULL,
    lon       double precision NOT NULL,
    speed_mph double precision NOT NULL,
    battery   integer NOT NULL
);
CREATE INDEX position_history_unit_at ON position_history (unit_id, at DESC);

-- Overlay images and logos live in the database so one backup covers everything.
CREATE TABLE assets (
    name         text PRIMARY KEY,
    content_type text NOT NULL,
    data         bytea NOT NULL,
    created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE admin_users (
    id            bigserial PRIMARY KEY,
    username      text NOT NULL UNIQUE,
    password_hash text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE admin_sessions (
    token_hash text PRIMARY KEY,
    user_id    bigint NOT NULL REFERENCES admin_users(id) ON DELETE CASCADE,
    expires_at timestamptz NOT NULL
);
