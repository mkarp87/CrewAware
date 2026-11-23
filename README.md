# CrewAware

Incident-command app for tracking utility vehicles on an event site.

- Live map of units and repeaters on a site overlay
- Click the map to place an incident and see the nearest available units, ranked by travel time
- Dispatch, on scene and clear, with an activity log
- Repeater status and check-in tracking
- Runs offline, as a single self-contained app on Raspberry Pi, Linux or Windows
- Position data comes from MeshCore LoRa trackers through a USB companion node

Status: early build. The server and web UI run against a built-in simulator. The real MeshCore radio gateway is not written yet, because it depends on the hardware spikes. See [BUILD_PLAN.md](BUILD_PLAN.md) for scope, architecture, rules, spikes and milestones.

CrewAware is a Rust app built and deployed the same way as DMR-Master-System: Docker Compose with PostgreSQL 17, a `.env` file, migrations run by the app at startup, and admin accounts for setup.

## Run it

You need Docker with Compose v2.

```
cp .env.example .env     # set POSTGRES_PASSWORD, CREWAWARE_ADMIN_PASSWORD (12+ characters) and CREWAWARE_SECRET_KEY
                         # and set CREWAWARE_SIM=1 to try it with simulated units
docker compose up -d --build
```

Open http://localhost:8080. Type a seat name in the header, click **Place incident**, then click the map. Open the **Setup** tab and log in as the admin to set the site name, logo, slogan, overlay image and anchors, vehicle groups, units, repeaters and the contact rules.

Change the admin password later with:

```
echo 'a new password here' | docker compose exec -T crewaware crewaware set-password admin
```

## Develop

```
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                       # engine tests; no database needed
TEST_DATABASE_URL=postgres://user:pw@localhost/crewaware_scratch cargo test -p crewaware-store   # wipes that database
DATABASE_URL=postgres://user:pw@localhost/crewaware CREWAWARE_SIM=1 CREWAWARE_ADMIN_PASSWORD='12+ characters here' cargo run -p crewaware
```

## What works now (milestones M1 to M3, partly)

- Live map over an uploaded overlay image, pinned with two anchors (north-up images only)
- Units by status, with contact state (normal, late, no contact) from report age, using parked and moving thresholds you can edit
- Place an incident and see available units ranked by straight-line distance, with an ETA from the vehicle group's max mph
- Dispatch is all-or-nothing and refuses a unit another seat already took
- Manual status per unit, on scene, close incident, and an activity log with seat names
- Repeaters with check-in status, vehicle groups, logo and slogan branding
- Repeater readings: an admin stores each repeater's login password (encrypted with `CREWAWARE_SECRET_KEY`), CrewAware reads repeaters one at a time on a slow schedule, and everyone sees the latest reading with its age. Admins can press **Read now** (limited to once a minute per repeater). A successful read counts as a repeater check-in. Reading is read-only: there is no way to change a repeater's settings.
- PostgreSQL stores configuration, incidents, the log and a throttled position history; overlay images are stored there too, so one backup covers everything
- Admin login (argon2 password hashes, session cookies, login throttling) protects Setup; dispatching needs no login
- Ages use the monotonic clock, so a wall-clock jump cannot change contact states, and a restart shows no contact until a unit reports again

## Not done yet

- MeshCore USB gateway (the simulator stands in for it). Until it exists, repeater reads only work in simulator mode, and **Read now** says no gateway is configured. The reading field names (battery, noise floor, SNR, uptime) are provisional until the gateway defines what a repeater returns.
- Obstacles and routed ETA, site profile export/import and site switching
- Repeater drag-to-place on the map, polling of silent units, alerts, report exports

## Layout

```
apps/crewaware/        axum server, API, live updates (SSE), simulator, embedded web UI (web/)
crates/crewaware-core/ model, contact rules, geometry, the in-memory engine (no I/O)
crates/crewaware-store/ PostgreSQL storage, admin accounts, assets
migrations/            SQL schema, applied by the app at startup
docs/                  mockup source and notes
```
