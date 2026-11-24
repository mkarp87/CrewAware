//! Connects the radio gateway to the engine: contact updates become position reports and
//! check-ins, unknown nodes are remembered for an admin to adopt, repeaters are read on request,
//! and units that go quiet are asked directly.

use crate::app::{App, Discovered};
use crate::reader::{ReadFuture, RepeaterReader};
use crewaware_core::Position;
use crewaware_gateway::client::{Event, Gateway};
use crewaware_gateway::proto::{self, ContactRec};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

/// A report with an untrustworthy age is recorded as a day old: the position is kept, but the
/// unit shows no contact until it is heard again.
const UNTRUSTED_AGE: u64 = 86_400;

pub async fn run(app: Arc<App>, mut rx: UnboundedReceiver<Event>) {
    while let Some(ev) = rx.recv().await {
        match ev {
            Event::Status(s) => app.set_radio_status(s),
            Event::Contact { rec, age_secs } => handle_contact(&app, rec, age_secs),
        }
    }
}

fn handle_contact(app: &App, rec: ContactRec, age: Option<u64>) {
    let age = age.unwrap_or(UNTRUSTED_AGE);
    if let Some(uid) = app.read(|e| e.unit_for_key(&rec.key)) {
        if rec.has_position() {
            app.with(|e| {
                e.report_position(Position {
                    unit_id: uid,
                    lat: rec.lat,
                    lon: rec.lon,
                    speed_mph: None,
                    battery: -1,
                    age_secs: age,
                })
            });
        } else {
            // The tracker advertised but has no GPS fix: it is alive, its position is unchanged.
            app.with(|e| e.report_checkin(&uid, age));
        }
    } else if let Some(rid) = app.read(|e| e.repeater_for_key(&rec.key)) {
        app.with(|e| e.report_repeater_aged(&rid, age));
    } else {
        let kind = match rec.kind {
            proto::ADV_TYPE_REPEATER => "repeater",
            proto::ADV_TYPE_ROOM => "room",
            _ => "unit",
        };
        app.note_discovered(Discovered {
            key: rec.key.clone(),
            name: if rec.name.is_empty() {
                rec.key[..8].to_string()
            } else {
                rec.name.clone()
            },
            kind,
            lat: rec.lat,
            lon: rec.lon,
            has_pos: rec.has_position(),
            heard: Instant::now()
                .checked_sub(Duration::from_secs(age))
                .unwrap_or_else(Instant::now),
        });
    }
}

/// Reads repeaters through the radio: log in, then ask for status. Nothing is written.
pub struct GatewayReader {
    pub app: Arc<App>,
    pub gw: Gateway,
}

impl RepeaterReader for GatewayReader {
    fn read<'a>(&'a self, repeater_id: &'a str, password: &'a str) -> ReadFuture<'a> {
        Box::pin(async move {
            let key = self
                .app
                .read(|e| e.repeater_key(repeater_id))
                .filter(|k| !k.is_empty())
                .ok_or_else(|| "no radio node is assigned to this repeater".to_string())?;
            let st = self.gw.read_repeater(&key, password).await.map_err(|e| e.to_string())?;
            serde_json::to_value(st).map_err(|e| e.to_string())
        })
    }
}

/// Rough charge from a lithium cell's voltage: 3.3 V empty, 4.15 V full. Good enough to show a
/// low tracker, not a fuel gauge.
pub fn voltage_to_percent(v: f64) -> i32 {
    (((v - 3.3) / (4.15 - 3.3)) * 100.0).round().clamp(0.0, 100.0) as i32
}

/// Asks units that have gone quiet for their telemetry, one at a time and no more than once per
/// `every` per unit. A tracker that answers is alive even if its adverts are not getting through.
pub async fn poll_silent(app: Arc<App>, gw: Gateway, every: Duration) {
    let mut last: HashMap<String, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        if !gw.status().connected {
            continue;
        }
        for (id, key) in app.read(|e| e.silent_units()) {
            if last.get(&id).is_some_and(|t| t.elapsed() < every) {
                continue;
            }
            last.insert(id.clone(), Instant::now());
            match gw.request_telemetry(&key).await {
                Ok(t) => {
                    tracing::info!("polled {id}: answered");
                    match (t.lat, t.lon) {
                        (Some(lat), Some(lon)) if lat != 0.0 || lon != 0.0 => app.with(|e| {
                            e.report_position(Position {
                                unit_id: id.clone(),
                                lat,
                                lon,
                                speed_mph: None,
                                battery: t.voltage.map(voltage_to_percent).unwrap_or(-1),
                                age_secs: 0,
                            })
                        }),
                        _ => app.with(|e| match t.voltage {
                            Some(v) => e.report_battery(&id, voltage_to_percent(v)),
                            None => e.report_checkin(&id, 0),
                        }),
                    }
                }
                Err(e) => tracing::info!("polled {id}: {e}"),
            }
        }
        last.retain(|_, t| t.elapsed() < every * 4);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_estimate() {
        assert_eq!(voltage_to_percent(4.2), 100);
        assert_eq!(voltage_to_percent(3.3), 0);
        assert_eq!(voltage_to_percent(3.0), 0);
        assert_eq!(voltage_to_percent(3.725), 50);
    }
}
