# CrewAware

Incident-command app for tracking utility vehicles on an event site.

- Live map of units and repeaters on a site overlay
- Click the map to place an incident and see the nearest available units, ranked by travel time
- Dispatch, on scene and clear, with an activity log
- Repeater status and check-in tracking
- Runs offline, as a single self-contained app on Raspberry Pi, Linux or Windows
- Position data comes from MeshCore LoRa trackers through a USB companion node

Status: planning. See [BUILD_PLAN.md](BUILD_PLAN.md) for scope, architecture, data model, rules, spikes and milestones.

## Suggested layout

```
server/     radio service, state engine, routing, API, SQLite
web/        browser UI
sim/        radio simulator for development and tests
docs/       notes and spike results
```

## First steps

1. Run the spikes in section 9 of the build plan, starting with S1 to S3.
2. Build the simulator and the live map (milestone M1).
