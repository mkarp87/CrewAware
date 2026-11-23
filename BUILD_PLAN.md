# CrewAware: Build Plan

Status: draft v0.1, 2026-11-23

A self-contained incident-command app that shows where UTVs are on a site overlay, finds the nearest available unit for an incident, and tracks the status of the LoRa repeaters that carry the signal. Position data comes from MeshCore LoRa trackers.

The design mockup (command view, site setup, status rules, data model) lives in a private Claude artifact: https://claude.ai/artifact/VA8oQvuihJbMzzYNaVt6Wb. Export screenshots into the repo if others need to see it.

---

## 1. Scope and constraints

Decided with the project owner:

| Topic | Decision |
|---|---|
| Users | 2 dispatchers at once. No logins or roles. Each picks a seat name ("Dispatcher 1") so the log shows who did what. |
| Scale | 10 to 15 units, 3 to 5 repeaters, one server, one gateway. |
| Sites | One active site at a time. Switched between events. Saved as site profiles. |
| Connectivity | Must run fully offline. Internet may be available sometimes but is never required. |
| Trackers | One-way. They only report position. No driver screens, buttons or messages. |
| Platforms | Raspberry Pi, Linux server and Windows. Self-contained, no cloud dependency. |
| Units of measure | Speed in mph. Distance in feet and miles. Store metric internally if convenient. |
| Start state | Units start as Available. A unit with no position yet shows No contact, so it is not suggested. |
| Out of scope for v1 | Logins and roles, BLE gateway connection, driver-side features, cloud sync. |

---

## 2. Architecture

```
 UTV tracker (MeshCore, GPS)
        |  LoRa
        v
 Repeaters (3 to 5, fixed)
        |  LoRa
        v
 Gateway: MeshCore companion node on USB
        |  serial (or TCP/WiFi)
        v
 +---------------------------------------------+
 |  Server app (one binary)                    |
 |   - Radio service (companion protocol)      |
 |   - State engine (contact, status, rules)   |
 |   - Routing (obstacles, speed groups, ETA)  |
 |   - SQLite (WAL)                            |
 |   - HTTP + WebSocket API                    |
 |   - Embedded web UI                         |
 +---------------------------------------------+
        |  LAN, HTTP + WebSocket
        v
 Browsers (2 dispatchers)
```

### Components

**Radio service**
- Opens the companion node over USB serial. TCP/WiFi is supported through the same transport interface. No BLE in v1, because Bluetooth stacks differ too much between Linux and Windows.
- Starts with the app-start command, syncs contacts, then listens for adverts and messages. Coordinates arrive in microdegrees.
- Reconnects automatically if the port drops or renumbers.
- Polls quiet units for position (see section 5).
- Has a simulator implementation so the whole app can be built and tested without hardware.

**State engine**
- Derives each unit's contact state (normal, late, no contact) and each repeater's status (online, degraded, offline) from last-heard times.
- Owns the dispatch status machine and assignments.
- Rejects conflicting actions. Example: a second dispatch of a unit that is already dispatched returns "already dispatched by Dispatcher 1".

**Routing**
- Turns the site obstacles into a grid, runs A* from candidate units to the incident, and ranks by travel time.
- Details in section 6.

**API**
- REST for actions (place incident, dispatch, set status, edit site) and a WebSocket that pushes state changes to every connected browser.
- Every action carries the seat name.

**Storage**
- SQLite in WAL mode. One file, easy to back up. Migrations ship inside the binary.

**Web UI**
- Static assets embedded in the binary and served by the app. Fonts and map libraries are bundled, so nothing loads from the internet.
- Branding: the header of every page shows the active site's logo and slogan, set on the Site setup page. If a profile has none, the app name is shown alone. Logos are PNG, SVG or JPG up to 1 MB, shown about 40 px tall, with an option to place a dark logo on a white background. Uploads are stored in the app's data folder and included in the site profile export. SVGs are shown only through an image tag, with scripts stripped.

---

## 3. Data model

Full field lists are on the "Data model" board of the mockup. This plan changes it for single-site, site-profile use.

**Shared across events**
- `unit_groups`: id, name, max_speed_mph.
- `units`: id, name, node_public_key, group_id, optional max_speed_override_mph, notes.
- `repeaters`: id, name, kind (repeater or gateway), node_public_key. A repeater is a physical node you own.

**Site profile (built once per location, reused)**
- `site_profiles`: id, name, gateway settings, check-in rules, logo file (optional), slogan (optional, up to 60 characters).
- `overlays`: profile, image file, two anchor points (pixel and lat/lon each), default opacity.
- `obstacles`: profile, kind (wall, gate, no_go, slow_zone), geometry, optional speed_limit_mph, active.
- `repeater_placements`: profile, repeater, lat, lon.

**Per event**
- `deployments`: id, site_profile_id, name, start, end, active. Only one active at a time.
- `incidents`, `assignments`, `unit_status`, `status_log`, `positions`, `repeater_checkins`: each links to a deployment, so every event's history stays separate.

**Computed, never stored:** unit contact state, repeater status, nearest-unit rank, route and ETA (saved only on the assignment when dispatched).

Site profiles export and import as a single file (JSON plus the overlay image) so they move between machines and operating systems.

---

## 4. Status and check-in rules

### Dispatch status (set by command)
Available, Dispatched, On scene, Busy, Off duty.
- Available to Dispatched happens when command confirms a dispatch. It creates the assignment in the same step.
- Dispatched to On scene when command marks arrival.
- On scene to Available only when command clears it. A unit never frees itself.
- Busy and Off duty are manual, from any state. Setting either removes the unit's assignment.

### Contact state (derived from the radio)

| | Normal | Late | No contact |
|---|---|---|---|
| Parked (fallback check-in every 15 min) | within 15 min | 15 to 30 min | over 30 min |
| Last seen moving (proposed defaults) | under 2 min | 2 to 10 min | over 10 min |

All four limits are per-site settings. The moving values are proposed defaults and have not been tuned.

- Status and contact state are independent. A unit can be Dispatched and No contact, and the UI shows both.
- Only Available units that are not in No contact are suggested for new incidents. Busy and No-contact units appear under "Not suggested" with an override.
- Measure ages with a clock that does not jump (monotonic time), not wall time. See the Pi notes in section 7.
- Alert when a Dispatched or Available unit moves to No contact, when a repeater goes degraded or offline, and when a repeater battery is low.

---

## 5. Radio strategy and acknowledgments

Goal: know a parked unit is parked, not dead.

1. Units send position when they move, and fall back to a 15 minute check-in when stationary.
2. If a unit goes quiet, the server polls it for position (telemetry request). Retry a few times. If there is no reply, mark it Late, then No contact.
3. Later, optionally: custom tracker firmware that sends position as a direct message and resends until acknowledged. MeshCore direct messages are acknowledged and retried by the sender, but stock tracker firmware is normally driven by a phone app, so this likely needs firmware work.

Notes:
- A radio-level ACK proves the gateway radio received a message, not that the app stored it.
- Polling relies on the unit allowing telemetry replies with GPS. One firmware guide describes Deny, Base and Base+GPS settings. This is unverified on the chosen tracker.

---

## 6. Routing, obstacles and speed

- **Obstacle kinds:** wall (a line), gate (an opening in a wall), no-go zone (polygon), slow zone (polygon with its own mph limit). Each has an active toggle so command can close or reopen areas during an event.
- **Method:** rasterize the site into small cells in geographic coordinates, mark blocked cells, and run A* with 8-direction moves. Cost per cell is distance divided by allowed speed.
- **Ranking:** take the nearest few units by straight line, route each, and rank by travel time. This catches the case where the straight-line nearest unit is behind a wall.
- **Speed:** each unit belongs to a vehicle group with a max mph. Travel speed on a stretch is the lower of the group's max and any slow-zone limit. An optional per-unit override exists. A site-wide adjustment factor (a number below 1) converts max speed to realistic average speed. Tune it after field tests. A parked unit may also get a fixed start-up delay.
- **Failure cases:** if no path exists, show "no route found" and fall back to straight-line distance with a warning. Add a small buffer around walls, because GPS error of a few meters can put a unit on the wrong side.
- **Position meaning:** routes are estimates. Drivers choose their own path.

---

## 7. Packaging and cross-platform

Targets: Linux x86-64, Linux ARM64 (Raspberry Pi 4 or 5, 64-bit OS) and Windows x86-64.

**Decision (November 2026):** CrewAware is built in Rust and deployed like DMR-Master-System: Docker Compose with PostgreSQL 17 and the app, settings in a `.env` file, schema migrations run by the app at startup, admin accounts for setup. The server is therefore not a single self-contained binary any more. The older single-binary-with-SQLite shape below is kept as an option for a standalone laptop or Pi with no Docker. The radio gateway has to run where the USB companion node is plugged in, and Docker Desktop on macOS and Windows cannot pass USB serial devices into a container, so on those systems the gateway will run as a native process next to the containers.

**Install shape (original plan)**
- One native binary per target with the web UI embedded and SQLite built in. No runtime to install.
- Native binaries are the primary install. Docker is optional on Linux and Pi only. On Windows, passing USB serial into Docker is painful, which is why Docker is not the main path.
- Run as a systemd service on Linux and as a Windows service. Both start at boot and restart on failure.
- Ship a first-run setup page: pick the serial port from a list, set the listen port, and show the server's LAN address so dispatchers know where to browse.

**Language options (decide after the first spike)**
- **Rust or Go:** single static binary, easy cross-compiling. A Rust MeshCore library exists (`meshcore-rs`, a port of the Python library, serial and TCP on by default) but it is at version 0.1.x, so check its coverage. In Go the companion protocol would be written from the protocol docs.
- **Node or Python:** more mature MeshCore libraries (`meshcore.js` for Node over serial and TCP, `meshcore_py`), but each OS needs a bundled runtime, which weakens the "self-contained" goal.
- Recommended order: spike the radio layer with the existing library of whichever language you prefer, then decide.

**Platform gotchas**
- **Serial ports:** Linux uses `/dev/ttyACM0` style paths and needs permissions (dialout group or a udev rule). Windows uses `COM` ports and some boards need a USB driver. Always list ports in the UI and let the user choose. Handle renumbering after replug.
- **Raspberry Pi has no real-time clock.** Offline, it boots with the wrong time. Use monotonic time for ages, sanity-check wall time, and consider a cheap RTC module. The gateway's GPS time is another possible source.
- **SD card wear:** batch position writes, trim old history, or use a USB SSD.
- **Firewall:** open the listen port on Windows and Linux. The app should bind to the LAN only by default.
- **Time zones:** store UTC, display local.

---

## 8. Hardware notes

Gateway: any board with a USB companion firmware build, with a good external antenna mounted as high as possible. A server-room desk is not a good place.

Trackers (decision pending the pilot):

| | Wio Tracker L1 Pro | SenseCAP T1000-E | Heltec Wireless Tracker V2 |
|---|---|---|---|
| GPS | L76K multi-GNSS | Mediatek AG3335 | Multi-GNSS (GPS, GLONASS, BDS, Galileo, NAVIC, QZSS) |
| Battery | 2000 mAh built in, solar input | 700 mAh | none built in |
| Waterproof | no IP rating found | IP65 (spray, not submersion) | no rating found |
| Antennas | external LoRa antenna supported | internal only | board antenna |
| Notes | OLED and USB-C open to the weather | operating -20 to 60 C, charging 0 to 45 C | ESP32-S3, higher power draw |

- With no driver interaction, a screenless sealed unit is fine.
- A roof mount means sun, rain and vibration. Prefer radio inside the cab with only antennas on the roof. If the radio must go outside, use an IP67 box with cable glands, a UV-resistant case, and a vent plug or desiccant.
- 24 hour battery: runtime with MeshCore and continuous GPS is unknown. Measure it. The T1000-E may need a vehicle power feed.

---

## 9. Spikes (do these first)

Each spike should end with a written result in this repo.

| # | Question | Pass looks like |
|---|---|---|
| S1 | How does the chosen tracker report position, and how often? What does the stop-when-stationary option do, and does it send a final stopped report? | Positions reach the gateway at a known rate while moving, and a parked unit checks in every 15 min. |
| S2 | Can the gateway poll a tracker for position, with retries? | A quiet unit answers a poll, and an unplugged one is detected. |
| S3 | Which MeshCore library, if any, covers contacts, adverts, telemetry and reconnects well enough? | A script prints live positions for hours without dropping. |
| S4 | Does 10 to 15 units plus 3 to 5 repeaters stay within the radio's capacity at the planned report rates? | Position rate and loss measured on a busy test. |
| S5 | Coverage: where are the dead spots on the real site, and where do repeaters fix them? | A drive-test map with repeater candidate locations. |
| S6 | 24 hour battery and GPS quality on a real UTV with the planned mount. | Full day on one charge, fixes with acceptable dropouts. |
| S7 | Pi without internet: does time behave after reboot? | Ages stay correct across reboot and clock correction. |

---

## 10. Milestones

- **M0, spikes:** S1 to S3 minimum, with the simulator started.
- **M1, live map:** gateway service, SQLite, WebSocket, a map with unit markers and ages. Works with the simulator and real hardware.
- **M2, overlay and site profiles:** overlay upload and two-point pinning, site export and import, active-site switch with confirmation.
- **M3, dispatch:** place incident, nearest units (straight line), confirm dispatch, status dropdowns, activity log, seat names, double-dispatch protection.
- **M4a, repeater reads (decided November 2026):** CrewAware logs in to repeaters itself and reads their status; no separate admin tool and no writes. Passwords are stored encrypted with a key from `.env`. Reads are slow, one at a time, and rate limited because they share airtime with position reports. Built and tested against a simulated reader; the real reader depends on spike S-radio with hardware.
- **M4, check-ins and repeaters:** contact rules, polling, repeater status and placement, alerts.
- **M5, obstacles and ETA:** drawing tools, routing, vehicle groups, mph ETA, no-route handling.
- **M6, packaging:** builds and service install for all three targets, first-run setup, backups.
- **M7, pilot:** 2 or 3 units for a full day on the real site. Record battery, update rate, GPS and coverage.
- **M8, rollout:** full fleet, repeater placement from the survey, operator walkthrough.

---

## 11. Testing

- **Simulator:** a fake radio transport that generates moving, parked, late and silent units and repeaters. It drives development and automated tests.
- **Unit tests:** the contact rules, status machine, double-dispatch guard, router (including walls, gates and slow zones) and ETA math.
- **Clock tests:** a wall-clock jump must not change contact states.
- **Builds:** CI builds all three targets. Run smoke tests on a real Raspberry Pi and a Windows machine.
- **Field tests:** the spikes and the M7 pilot.

---

## 12. Security and operations

- Keep the server on a local network. Do not expose it to the internet.
- Seat names are for the audit log, not for security. Add a shared passcode if the network is shared.
- Back up the SQLite file and exported site profiles. Put the server and gateway on a UPS, and keep a spare companion node ready.
- Check with the base's communications or spectrum office before transmitting. Unlicensed radios can still be restricted on an airfield.
- Position data of units may be sensitive. Decide who can access the server and its backups.

---

## 13. Open questions

- Which tracker and firmware will be used? (decided after S1, S2 and S6)
- Do satellite basemap tiles matter, or is the overlay enough? If tiles matter, they must be downloaded ahead of time.
- Is a second gateway (hot spare) worth the extra radio traffic and complexity?
- What reports does command want after an event (timeline, unit tracks, response times)?
- Will other people ever need read-only access, such as a viewer screen?

---

## 14. References

- MeshCore companion protocol: https://docs.meshcore.io/companion_protocol/
- MeshCore.js (browser and Node): https://github.com/meshcore-dev/meshcore.js
- Python library: https://github.com/meshcore-dev/meshcore_py
- Rust library: https://docs.rs/crate/meshcore-rs/latest/source/README.md
- MeshCore Proxy (share a companion radio over TCP): https://pypi.org/project/meshcore-proxy/
- MeshCore Open (open-source client with a live map): https://github.com/zjs81/meshcore-open
- SenseCAP T1000-E: https://wiki.seeedstudio.com/t1000_e_intro/
- Wio Tracker L1 series: https://wiki.seeedstudio.com/wio_tracker_l1_node/
