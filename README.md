# CrewAware

Incident-command app for tracking utility vehicles on an event site.

- Live map of units and repeaters on a site overlay
- Click the map to place an incident and see the nearest available units, ranked by travel time
- Dispatch, on scene and clear, with an activity log
- Repeater status and check-in tracking
- Runs offline, as a single self-contained app on Raspberry Pi, Linux or Windows
- Position data comes from MeshCore LoRa trackers through a USB companion node

Status: feature-complete against the plan, tested end to end against a fake radio, **not yet run against real MeshCore hardware**. See [BUILD_PLAN.md](BUILD_PLAN.md) for scope, architecture and rules, and "Before you trust it" below.

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

## Connecting a radio

Set `CREWAWARE_RADIO` in `.env` to the MeshCore companion radio: `serial:/dev/ttyACM0`, or `tcp://host:port` for a radio reached over the network. Then open Setup, Radio: the radio's name and node count show there, and every node it hears appears under "Heard on the mesh". Name each one and adopt it as a unit or a repeater; adopted nodes show on the map at once.

Docker Desktop on macOS and Windows cannot pass a USB device into a container. On those, run the radio's serial port through `ser2net` or `socat` on the host and use `tcp://host.docker.internal:PORT`, or run CrewAware natively with `cargo run`.

To try everything without hardware, run the fake radio and point CrewAware at it:

```
cargo run -p crewaware-gateway --bin fake-radio -- --listen 127.0.0.1:7777   # 6 units, 3 repeaters (password demo-password; Rpt West never answers)
CREWAWARE_RADIO=tcp://127.0.0.1:7777 DATABASE_URL=... CREWAWARE_ADMIN_PASSWORD='12+ characters' cargo run -p crewaware
```

## What works

- Live map over an uploaded overlay image, pinned with two anchors (north-up images only)
- Units by status, with contact state (normal, late, no contact) from report age, using parked and moving thresholds you can edit. A unit that goes late is asked to report; one that is dispatched or on scene and silent raises a red banner (and a beep if Sound is on) and is logged.
- Place an incident and see available units ranked by routed ETA. Routes go around **walls**, **closed gates**, and **no-go areas**, and cross **slow zones** at a fraction of speed. Hover or tick a unit to see its route. ETA uses the vehicle group's max mph.
- Dispatch is all-or-nothing and refuses a unit another seat already took. Manual status per unit, on scene, close incident, and an activity log with seat names.
- Dispatchers open and close gates from the Units tab
- Repeaters: drag them to where they stand (Setup, then Edit map on the map), or place with a click. An admin stores each repeater's login password (encrypted with `CREWAWARE_SECRET_KEY`); CrewAware logs in over the mesh one repeater at a time and shows battery, noise floor, SNR and uptime. Read-only: there is no way to change a repeater's settings.
- Saved sites: a saved site holds the overlay, its pinning, obstacles and repeater positions. Switch sites with two clicks, export one as a file, import it on another server. Crew, passwords and rules stay as they are. Switching is refused while incidents are open.
- Reports: a printable page per incident (timeline and units) and a CSV of all incidents
- Branding (name, logo, slogan) in Setup; admin login protects Setup, dispatching needs none
- PostgreSQL stores everything, including overlay images, so one backup covers it. A restart shows no contact until a unit is heard again.

## Before you trust it

- The MeshCore radio protocol was written from the published firmware sources and tested only against the fake radio in this repository. The first real radio may disagree on a detail. Expect to adjust `crates/crewaware-gateway/src/proto.rs`.
- Repeater reads depend on the repeater answering a login and status request over the mesh, which can fail for range, airtime or firmware reasons the fake does not model.
- Position age comes from the radio's own clock, which CrewAware sets on connect. If a tracker's GPS never gets a fix its position is not updated, but it still counts as checking in.
- Routing works on a coarse grid built from what you draw. It is only as good as the walls and no-go areas you draw, and it ranks units; it is not turn-by-turn navigation.
- Anyone on the network can dispatch. Keep the server on a network you trust.

## Layout

```
apps/crewaware/        axum server, API, live updates (SSE), simulator, embedded web UI (web/)
crates/crewaware-core/ model, contact rules, geometry, routing, the in-memory engine (no I/O)
crates/crewaware-gateway/ MeshCore companion protocol, radio client, fake radio
crates/crewaware-store/ PostgreSQL storage, admin accounts, assets
migrations/            SQL schema, applied by the app at startup
docs/                  mockup source and notes
```
