# CrewAware

Incident-command app for tracking utility vehicles on an event site.

- Live map of units and repeaters on a site overlay
- Click the map to place an incident and see the nearest available units, ranked by travel time
- Dispatch, on scene and clear, with an activity log
- Repeater status and check-in tracking
- Runs offline, as a single self-contained app on Raspberry Pi, Linux or Windows
- Position data comes from MeshCore LoRa trackers through a USB companion node

Status: early build. A working server and web UI run against a built-in simulator. The real MeshCore radio gateway is not written yet, because it depends on the hardware spikes. See [BUILD_PLAN.md](BUILD_PLAN.md) for scope, architecture, rules, spikes and milestones.

## Try it

You need Go 1.22 or newer. The server uses only the standard library.

```
cd server
go run . -sim            # then open http://localhost:8080
go test -race ./...
```

`-sim` seeds a demo site with moving and parked units and three repeaters. Without it you get an empty site. Data is kept in `./data` (override with `-data`). Open the page, type a seat name in the header, click **Place incident**, then click the map.

Cross-compile one self-contained binary per target:

```
CGO_ENABLED=0 GOOS=linux   GOARCH=arm64 go build -o crewaware-pi .
CGO_ENABLED=0 GOOS=windows GOARCH=amd64 go build -o crewaware.exe .
```

## Run in a container

```
docker compose up --build        # demo mode with the simulator, http://localhost:8080
```

Data lives in the `crewaware-data` volume. Remove `command: ["-sim"]` in `docker-compose.yml` for a real site. USB passthrough for the radio only works on Linux hosts, so on Windows and macOS run the plain binary instead once the gateway exists. The container still uses the JSON-file store. A Postgres option needs a Go driver module, which is not added yet.

## What works now (milestones M1 to M3, partly)

- Live map over an uploaded overlay image, pinned with two anchors (north-up images only)
- Units by status, with contact state (normal, late, no contact) from report age, using parked and moving thresholds
- Place an incident, see available units ranked by straight-line distance with ETA from the vehicle group's max mph
- Dispatch is all-or-nothing and refuses a unit another seat already took
- Manual status per unit, on scene, close incident, activity log with seat names
- Repeaters with check-in status; vehicle groups; logo and slogan branding
- State saved to a JSON file; ages use the monotonic clock, so a clock jump cannot change contact states

## Not done yet

- MeshCore USB gateway (transport interface exists, simulator implements it)
- Obstacles and routed ETA, site profile export/import and site switching
- SQLite storage and WebSocket. The build environment could not fetch third-party Go modules, so storage is a JSON file behind the `Store` type and live updates use Server-Sent Events. Both can be swapped later.
- Repeater drag-to-place on the map, polling of silent units, alerts, authentication

## Layout

```
server/     Go server: engine, contact rules, geo, simulator, HTTP API, embedded web UI (server/web)
docs/       mockup source and notes
```
