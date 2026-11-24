use crate::geo::{eta_seconds, haversine_m, FEET_PER_M, MPS_PER_MPH};
use crate::model::*;
use crate::route::RouteField;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum EngineError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    Bad(String),
}

/// Everything the engine can ask the store to persist. The engine itself does no I/O.
#[derive(Clone, Debug)]
pub enum Change {
    Unit(Unit),
    Repeater(Repeater),
    Reading {
        repeater_id: String,
        reading: RepeaterReading,
    },
    Incident(Incident),
    Log(LogEntry),
    Site(Site),
    /// The whole obstacle list, replacing what was stored.
    Obstacles(Vec<Obstacle>),
    RemoveUnit(String),
    RemoveRepeater(String),
    Group(Group),
    Settings(Settings),
    Position {
        unit_id: String,
        lat: f64,
        lon: f64,
        speed_mph: f64,
        battery: i32,
        at: DateTime<Utc>,
    },
}

/// State loaded from the database at startup.
#[derive(Default)]
pub struct State {
    pub site: Site,
    pub settings: Settings,
    pub groups: Vec<Group>,
    pub units: Vec<Unit>,
    pub repeaters: Vec<Repeater>,
    pub incidents: Vec<Incident>,
    pub log: Vec<LogEntry>,
    pub obstacles: Vec<Obstacle>,
    pub next_incident: i32,
}

pub struct Engine {
    pub(crate) site: Site,
    pub(crate) settings: Settings,
    pub(crate) groups: Vec<Group>,
    pub(crate) units: Vec<Unit>,
    pub(crate) repeaters: Vec<Repeater>,
    pub(crate) incidents: Vec<Incident>,
    pub(crate) log: Vec<LogEntry>,
    pub(crate) obstacles: Vec<Obstacle>,
    next_incident: i32,
    pending: Vec<Change>,
    /// Last contact state seen by `tick`, per unit or repeater, and whether it was ever in contact
    /// since start. Used to log transitions once.
    contact_seen: HashMap<String, (Contact, bool)>,
}

#[derive(Serialize)]
pub struct UnitView {
    #[serde(flatten)]
    pub unit: Unit,
    pub contact: Contact,
    pub age_sec: f64,
    pub group_name: String,
}

#[derive(Serialize)]
pub struct RepeaterView {
    #[serde(flatten)]
    pub repeater: Repeater,
    pub contact: Contact,
    pub age_sec: f64,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub now: DateTime<Utc>,
    pub site: Site,
    pub settings: Settings,
    pub groups: Vec<Group>,
    pub units: Vec<UnitView>,
    pub repeaters: Vec<RepeaterView>,
    pub incidents: Vec<Incident>,
    pub log: Vec<LogEntry>,
    pub obstacles: Vec<Obstacle>,
    pub alerts: Vec<Alert>,
    /// The most recently closed incidents, newest first, so their reports can be opened.
    pub closed: Vec<Incident>,
}

#[derive(Serialize, Debug, Clone)]
pub struct Candidate {
    pub unit_id: String,
    pub name: String,
    pub group: String,
    /// Distance to travel: along the route when one was found, otherwise in a straight line.
    pub dist_m: f64,
    pub dist_ft: f64,
    /// Straight-line distance, for comparison with the route.
    pub straight_m: f64,
    /// None when the group has no usable speed.
    pub eta_sec: Option<f64>,
    pub contact: Contact,
    pub age_sec: f64,
    /// True when `dist_m` and `eta_sec` come from a route around obstacles.
    pub routed: bool,
    /// The route in overlay pixels, from the unit to the incident.
    pub path_px: Option<Vec<[f64; 2]>>,
}

/// How long ago a node was last heard. After a restart only the stored wall-clock time is left
/// (contact itself is not resurrected), and a node never heard counts as a day.
fn age_of(t: Option<Instant>, wall: Option<DateTime<Utc>>) -> Duration {
    if let Some(i) = t {
        return i.elapsed();
    }
    match wall {
        Some(w) => (Utc::now() - w).to_std().unwrap_or_default(),
        None => Duration::from_secs(86_400),
    }
}

impl Engine {
    pub fn new(mut s: State) -> Engine {
        if s.groups.is_empty() {
            s.groups.push(Group {
                id: "utv".into(),
                name: "UTV".into(),
                max_mph: 15.0,
                adjust: 0.8,
            });
        }
        for u in &mut s.units {
            // A restart must not resurrect contact: ages are monotonic-only.
            u.seen_mono = None;
        }
        for r in &mut s.repeaters {
            r.seen_mono = None;
        }
        Engine {
            site: s.site,
            settings: s.settings,
            groups: s.groups,
            units: s.units,
            repeaters: s.repeaters,
            incidents: s.incidents,
            log: s.log,
            obstacles: s.obstacles,
            next_incident: s.next_incident.max(1),
            pending: Vec::new(),
            contact_seen: HashMap::new(),
        }
    }

    /// Takes the changes recorded since the last call, in order.
    pub fn drain_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.pending)
    }

    fn logf(&mut self, seat: &str, text: String) {
        self.logi(seat, text, None);
    }

    fn logi(&mut self, seat: &str, text: String, incident: Option<i32>) {
        let e = LogEntry {
            t: Utc::now(),
            seat: seat.into(),
            text,
            incident,
        };
        self.log.push(e.clone());
        if self.log.len() > 2000 {
            let cut = self.log.len() - 2000;
            self.log.drain(..cut);
        }
        self.pending.push(Change::Log(e));
    }

    fn unit_idx(&self, id: &str) -> Option<usize> {
        self.units.iter().position(|u| u.id == id)
    }

    fn group_of(&self, id: &str) -> Group {
        self.groups.iter().find(|g| g.id == id).cloned().unwrap_or(Group {
            id: String::new(),
            name: "none".into(),
            max_mph: 10.0,
            adjust: 1.0,
        })
    }

    fn contact_of(&self, u: &Unit) -> (Contact, Duration) {
        let age = age_of(u.seen_mono, u.last_seen);
        (contact_state(&self.settings, u.seen_mono.is_some(), u.moving, age), age)
    }

    pub fn incident(&self, id: i32) -> Option<Incident> {
        self.incidents.iter().find(|i| i.id == id).cloned()
    }

    pub fn all_incidents(&self) -> Vec<Incident> {
        self.incidents.clone()
    }

    pub fn unit_name(&self, id: &str) -> String {
        self.units
            .iter()
            .find(|u| u.id == id)
            .map(|u| u.name.clone())
            .unwrap_or_else(|| id.to_string())
    }

    pub fn site(&self) -> &Site {
        &self.site
    }
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    // ---- reports from transports ----

    /// Records a position report. Unknown unit ids are ignored, and so is a report older than the
    /// one already held (the radio replays its contact list after a reconnect).
    pub fn report_position(&mut self, p: Position) {
        let Some(i) = self.unit_idx(&p.unit_id) else { return };
        if !(p.lat.is_finite() && p.lon.is_finite() && p.lat.abs() <= 90.0 && p.lon.abs() <= 180.0) {
            return;
        }
        let min = self.settings.moving_min_mph;
        let age = Duration::from_secs(p.age_secs);
        let now = Utc::now();
        let at_mono = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        let at = now - chrono::Duration::seconds(p.age_secs.min(i64::MAX as u64 / 4) as i64);
        let u = &mut self.units[i];
        let prev = u.seen_mono.filter(|_| u.has_pos);
        if let Some(pm) = prev {
            if at_mono < pm {
                return;
            }
        }
        // Speed: as reported, else from how far the unit moved since its previous report. GPS
        // wander of a few metres is not movement, and very close reports say nothing about speed.
        let speed = match (p.speed_mph, prev) {
            (Some(s), _) => s,
            (None, Some(pm)) => {
                let dt = at_mono.duration_since(pm).as_secs_f64();
                let d = haversine_m(u.lat, u.lon, p.lat, p.lon);
                if d < 8.0 {
                    0.0
                } else if dt >= 10.0 {
                    d / dt / MPS_PER_MPH
                } else {
                    u.speed_mph
                }
            }
            (None, None) => 0.0,
        };
        u.has_pos = true;
        u.lat = p.lat;
        u.lon = p.lon;
        u.speed_mph = speed;
        u.moving = speed >= min;
        if p.battery >= 0 {
            u.battery = p.battery;
        }
        u.last_seen = Some(at);
        u.seen_mono = Some(at_mono);
        let u = u.clone();
        self.pending.push(Change::Position {
            unit_id: u.id.clone(),
            lat: u.lat,
            lon: u.lon,
            speed_mph: u.speed_mph,
            battery: u.battery,
            at,
        });
        self.pending.push(Change::Unit(u));
    }

    /// Records a battery level learned some other way than a position report, and counts as a
    /// check-in because the unit answered.
    pub fn report_battery(&mut self, unit_id: &str, percent: i32) {
        let Some(i) = self.unit_idx(unit_id) else { return };
        let u = &mut self.units[i];
        u.battery = percent.clamp(0, 100);
        u.last_seen = Some(Utc::now());
        u.seen_mono = Some(Instant::now());
        self.pending.push(Change::Unit(u.clone()));
    }

    pub fn report_repeater(&mut self, id: &str) {
        self.report_repeater_aged(id, 0);
    }

    /// Records that a repeater was heard `age_secs` ago. Older than what is already known is ignored.
    pub fn report_repeater_aged(&mut self, id: &str, age_secs: u64) {
        if let Some(r) = self.repeaters.iter_mut().find(|r| r.id == id) {
            let at_mono = Instant::now()
                .checked_sub(Duration::from_secs(age_secs))
                .unwrap_or_else(Instant::now);
            if r.seen_mono.is_some_and(|m| at_mono < m) {
                return;
            }
            r.last_seen = Some(Utc::now() - chrono::Duration::seconds(age_secs.min(1 << 40) as i64));
            r.seen_mono = Some(at_mono);
            let r = r.clone();
            self.pending.push(Change::Repeater(r));
        }
    }

    /// Records that a unit was heard without a position (an advert from a tracker with no GPS fix).
    pub fn report_checkin(&mut self, unit_id: &str, age_secs: u64) {
        let Some(i) = self.unit_idx(unit_id) else { return };
        let at_mono = Instant::now()
            .checked_sub(Duration::from_secs(age_secs))
            .unwrap_or_else(Instant::now);
        let u = &mut self.units[i];
        if u.seen_mono.is_some_and(|m| at_mono < m) {
            return;
        }
        u.last_seen = Some(Utc::now() - chrono::Duration::seconds(age_secs.min(1 << 40) as i64));
        u.seen_mono = Some(at_mono);
        self.pending.push(Change::Unit(u.clone()));
    }

    /// Units with a radio key that have gone quiet and are worth asking directly.
    pub fn silent_units(&self) -> Vec<(String, String)> {
        self.units
            .iter()
            .filter(|u| !u.node_key.is_empty() && u.status != Status::OffDuty && u.seen_mono.is_some())
            .filter(|u| self.contact_of(u).0 != Contact::Normal)
            .map(|u| (u.id.clone(), u.node_key.clone()))
            .collect()
    }

    /// Records the outcome of reading a repeater. A successful read proves the repeater is alive,
    /// so it also counts as a check-in; a failed read changes nothing about its contact state.
    pub fn record_reading(
        &mut self,
        repeater_id: &str,
        ok: bool,
        error: &str,
        data: serde_json::Value,
    ) -> Result<(), EngineError> {
        let r = self
            .repeaters
            .iter_mut()
            .find(|r| r.id == repeater_id)
            .ok_or(EngineError::NotFound)?;
        let now = Utc::now();
        let reading = RepeaterReading {
            read_at: now,
            ok,
            error: error.to_string(),
            data,
        };
        r.reading = Some(reading.clone());
        if ok {
            r.last_seen = Some(now);
            r.seen_mono = Some(Instant::now());
            let r = r.clone();
            self.pending.push(Change::Repeater(r));
        }
        self.pending.push(Change::Reading {
            repeater_id: repeater_id.into(),
            reading,
        });
        Ok(())
    }

    /// Called by the app after a password is stored or removed.
    pub fn set_has_credentials(&mut self, repeater_id: &str, has: bool) -> Result<(), EngineError> {
        let r = self
            .repeaters
            .iter_mut()
            .find(|r| r.id == repeater_id)
            .ok_or(EngineError::NotFound)?;
        r.has_credentials = has;
        Ok(())
    }

    pub fn repeater_exists(&self, id: &str) -> bool {
        self.repeaters.iter().any(|r| r.id == id)
    }

    /// Repeaters that have a stored password, in a stable order, for the poller.
    pub fn credentialed_repeaters(&self) -> Vec<String> {
        self.repeaters
            .iter()
            .filter(|r| r.has_credentials)
            .map(|r| r.id.clone())
            .collect()
    }

    // ---- snapshot ----

    pub fn snapshot(&self) -> Snapshot {
        let units = self
            .units
            .iter()
            .map(|u| {
                let (contact, age) = self.contact_of(u);
                UnitView {
                    unit: u.clone(),
                    contact,
                    age_sec: age.as_secs_f64(),
                    group_name: self.group_of(&u.group_id).name,
                }
            })
            .collect();
        let repeaters = self
            .repeaters
            .iter()
            .map(|r| {
                let age = age_of(r.seen_mono, r.last_seen);
                // Repeaters check in on the slow cadence.
                let contact = contact_state(&self.settings, r.seen_mono.is_some(), false, age);
                RepeaterView {
                    repeater: r.clone(),
                    contact,
                    age_sec: age.as_secs_f64(),
                }
            })
            .collect();
        Snapshot {
            now: Utc::now(),
            site: self.site.clone(),
            settings: self.settings.clone(),
            groups: self.groups.clone(),
            units,
            repeaters,
            incidents: self.incidents.iter().filter(|i| i.open).cloned().collect(),
            log: self.log.iter().rev().take(100).cloned().collect(),
            obstacles: self.obstacles.clone(),
            alerts: self.alerts(),
            closed: self
                .incidents
                .iter()
                .rev()
                .filter(|i| !i.open)
                .take(20)
                .cloned()
                .collect(),
        }
    }

    /// Things command should know about right now: units and repeaters that have gone quiet.
    /// A unit working an incident that stops reporting is the serious case.
    pub fn alerts(&self) -> Vec<Alert> {
        let mut out = Vec::new();
        for u in &self.units {
            if u.status == Status::OffDuty || u.seen_mono.is_none() {
                continue;
            }
            let (c, age) = self.contact_of(u);
            let mins = (age.as_secs_f64() / 60.0).round() as i64;
            let on_job = matches!(u.status, Status::Dispatched | Status::OnScene);
            let (sev, text) = match c {
                Contact::Normal => continue,
                Contact::Late => ("warn", format!("{} is late: no report for {mins} min", u.name)),
                Contact::NoContact => (
                    if on_job { "crit" } else { "warn" },
                    format!("{} has no contact: last report {mins} min ago", u.name),
                ),
            };
            out.push(Alert {
                kind: "unit".into(),
                id: u.id.clone(),
                name: u.name.clone(),
                severity: sev.into(),
                text,
                incident: u.incident_id,
            });
        }
        // Repeaters are only judged when we read them ourselves: their own adverts can be hours apart.
        for r in &self.repeaters {
            if !r.placed || !r.has_credentials || r.seen_mono.is_none() {
                continue;
            }
            let age = age_of(r.seen_mono, r.last_seen);
            if contact_state(&self.settings, true, false, age) == Contact::NoContact {
                out.push(Alert {
                    kind: "repeater".into(),
                    id: r.id.clone(),
                    name: r.name.clone(),
                    severity: "warn".into(),
                    text: format!(
                        "{} has no contact: last heard {} min ago",
                        r.name,
                        (age.as_secs_f64() / 60.0).round() as i64
                    ),
                    incident: None,
                });
            }
        }
        out
    }

    /// Notices contact changes and writes them to the activity log once each. Called every few
    /// seconds. Nothing is logged for something that has not been heard from since start, so a
    /// restart does not log every unit as lost.
    pub fn tick(&mut self) {
        let mut lines: Vec<(String, Option<i32>)> = Vec::new();
        let mut seen = std::mem::take(&mut self.contact_seen);
        for u in &self.units {
            let (c, age) = self.contact_of(u);
            let mins = (age.as_secs_f64() / 60.0).round() as i64;
            let entry = seen.entry(format!("u:{}", u.id)).or_insert((c, c == Contact::Normal));
            let (prev, had) = *entry;
            if prev != c {
                let text = match c {
                    Contact::Late if had => Some(format!("{} is late (no report for {mins} min)", u.name)),
                    Contact::NoContact if had => {
                        let job = if u.incident_id.is_some() && matches!(u.status, Status::Dispatched | Status::OnScene)
                        {
                            format!(" while {}", u.status.as_str().replace('_', " "))
                        } else {
                            String::new()
                        };
                        Some(format!("{}: no contact{job} (last report {mins} min ago)", u.name))
                    }
                    Contact::Normal if had && prev != Contact::Normal => Some(format!("{} is back in contact", u.name)),
                    _ => None,
                };
                if let Some(t) = text {
                    lines.push((t, u.incident_id));
                }
            }
            *entry = (c, had || c == Contact::Normal);
        }
        for r in &self.repeaters {
            if !r.placed || !r.has_credentials {
                continue;
            }
            let age = age_of(r.seen_mono, r.last_seen);
            let c = contact_state(&self.settings, r.seen_mono.is_some(), false, age);
            let mins = (age.as_secs_f64() / 60.0).round() as i64;
            let entry = seen.entry(format!("r:{}", r.id)).or_insert((c, c == Contact::Normal));
            let (prev, had) = *entry;
            if prev != c && had {
                match c {
                    Contact::NoContact => {
                        lines.push((format!("{} has no contact (last heard {mins} min ago)", r.name), None))
                    }
                    Contact::Normal => lines.push((format!("{} is back in contact", r.name), None)),
                    Contact::Late => {}
                }
            }
            *entry = (c, had || c == Contact::Normal);
        }
        self.contact_seen = seen;
        for (t, inc) in lines {
            self.logi("system", t, inc);
        }
    }

    /// Available units ranked by how soon they would arrive. With obstacles drawn, arrival is
    /// worked out along a route around them; without, in a straight line. Units with no contact
    /// are included but sorted after live ones, so command sees them without being steered toward
    /// a unit that may be gone.
    pub fn nearest(&self, lat: f64, lon: f64) -> Vec<Candidate> {
        let field = if self.obstacles.is_empty() {
            None
        } else {
            self.site
                .to_pixel(lat, lon)
                .and_then(|g| RouteField::build(&self.site, &self.obstacles, g))
        };
        let mut out: Vec<Candidate> = self
            .units
            .iter()
            .filter(|u| u.status == Status::Available && u.has_pos)
            .map(|u| {
                let (contact, age) = self.contact_of(u);
                let straight = haversine_m(lat, lon, u.lat, u.lon);
                let g = self.group_of(&u.group_id);
                let speed = g.max_mph * g.adjust * MPS_PER_MPH;
                let route = field.as_ref().and_then(|f| {
                    let (px, py) = self.site.to_pixel(u.lat, u.lon)?;
                    f.route_from(px, py)
                });
                let (dist, eta, routed, path) = match route {
                    Some(r) => (r.dist_m, (speed > 0.0).then(|| r.eff_m / speed), true, Some(r.path_px)),
                    None => {
                        let e = eta_seconds(straight, &g);
                        (straight, e.is_finite().then_some(e), false, None)
                    }
                };
                Candidate {
                    unit_id: u.id.clone(),
                    name: u.name.clone(),
                    group: g.name,
                    dist_m: dist,
                    dist_ft: dist * FEET_PER_M,
                    straight_m: straight,
                    eta_sec: eta,
                    contact,
                    age_sec: age.as_secs_f64(),
                    routed,
                    path_px: path,
                }
            })
            .collect();
        // A unit that has no route at all (walled off) ranks below those that do, but above ones
        // with no contact only if it is in contact: the unreachable flag is `routed == false`
        // while obstacles exist.
        let have_field = field.is_some();
        out.sort_by(|a, b| {
            let dead = |c: &Candidate| (c.contact == Contact::NoContact, have_field && !c.routed);
            let key = |c: &Candidate| c.eta_sec.unwrap_or(f64::INFINITY);
            dead(a)
                .cmp(&dead(b))
                .then(key(a).total_cmp(&key(b)))
                .then(a.dist_m.total_cmp(&b.dist_m))
        });
        out
    }

    // ---- dispatcher actions ----

    pub fn set_status(&mut self, unit_id: &str, status: Status, seat: &str) -> Result<(), EngineError> {
        let i = self.unit_idx(unit_id).ok_or(EngineError::NotFound)?;
        let old = self.units[i].status;
        if old == status {
            return Ok(());
        }
        if matches!(old, Status::Dispatched | Status::OnScene) && self.units[i].incident_id.is_some() {
            // A manual override releases the unit from its incident.
            self.detach(i);
        }
        self.units[i].status = status;
        let text = format!("{}: {} -> {}", self.units[i].name, old.as_str(), status.as_str());
        let u = self.units[i].clone();
        self.pending.push(Change::Unit(u));
        self.logf(seat, text);
        Ok(())
    }

    fn detach(&mut self, i: usize) {
        let uid = self.units[i].id.clone();
        self.units[i].incident_id = None;
        let mut touched = Vec::new();
        for inc in &mut self.incidents {
            let before = inc.units.len();
            inc.units.retain(|x| *x != uid);
            if inc.units.len() != before {
                touched.push(inc.clone());
            }
        }
        for inc in touched {
            self.pending.push(Change::Incident(inc));
        }
    }

    pub fn open_incident(&mut self, lat: f64, lon: f64, label: &str, seat: &str) -> Incident {
        let id = self.next_incident;
        self.next_incident += 1;
        let inc = Incident {
            id,
            label: if label.trim().is_empty() {
                format!("Incident {id}")
            } else {
                label.trim().to_string()
            },
            lat,
            lon,
            opened: Utc::now(),
            closed: None,
            open: true,
            units: vec![],
            opened_by: seat.into(),
        };
        self.incidents.push(inc.clone());
        self.pending.push(Change::Incident(inc.clone()));
        self.logi(seat, format!("Opened {}", inc.label), Some(inc.id));
        inc
    }

    fn inc_idx(&self, id: i32) -> Option<usize> {
        self.incidents.iter().position(|i| i.id == id)
    }

    /// Assigns units to an open incident. All or nothing: if any unit is no longer Available
    /// (for example another seat just dispatched it), nothing changes and the conflict names it.
    pub fn dispatch(&mut self, incident_id: i32, unit_ids: &[String], seat: &str) -> Result<(), EngineError> {
        let ii = self
            .inc_idx(incident_id)
            .filter(|&i| self.incidents[i].open)
            .ok_or(EngineError::NotFound)?;
        if unit_ids.is_empty() {
            return Err(EngineError::Bad("no units".into()));
        }
        let mut idx = Vec::new();
        for id in unit_ids {
            let ui = self.unit_idx(id).ok_or(EngineError::NotFound)?;
            if self.units[ui].status != Status::Available {
                return Err(EngineError::Conflict(format!(
                    "{} is already {}",
                    self.units[ui].name,
                    self.units[ui].status.as_str()
                )));
            }
            if idx.contains(&ui) {
                return Err(EngineError::Bad("duplicate unit".into()));
            }
            idx.push(ui);
        }
        let label = self.incidents[ii].label.clone();
        for ui in idx {
            self.units[ui].status = Status::Dispatched;
            self.units[ui].incident_id = Some(incident_id);
            let uid = self.units[ui].id.clone();
            let name = self.units[ui].name.clone();
            self.incidents[ii].units.push(uid);
            self.pending.push(Change::Unit(self.units[ui].clone()));
            self.logi(seat, format!("Dispatched {name} to {label}"), Some(incident_id));
        }
        self.pending.push(Change::Incident(self.incidents[ii].clone()));
        Ok(())
    }

    pub fn arrive(&mut self, incident_id: i32, unit_id: &str, seat: &str) -> Result<(), EngineError> {
        let ui = self.unit_idx(unit_id).ok_or(EngineError::NotFound)?;
        let ii = self.inc_idx(incident_id).ok_or(EngineError::NotFound)?;
        let u = &self.units[ui];
        if u.status != Status::Dispatched || u.incident_id != Some(incident_id) {
            return Err(EngineError::Conflict(format!(
                "{} is not dispatched to {}",
                u.name, self.incidents[ii].label
            )));
        }
        self.units[ui].status = Status::OnScene;
        let text = format!("{} on scene at {}", self.units[ui].name, self.incidents[ii].label);
        self.pending.push(Change::Unit(self.units[ui].clone()));
        self.logi(seat, text, Some(incident_id));
        Ok(())
    }

    /// Closing returns the incident's units to Available, except units a dispatcher set to
    /// Busy or Off duty.
    pub fn close_incident(&mut self, incident_id: i32, seat: &str) -> Result<(), EngineError> {
        let ii = self
            .inc_idx(incident_id)
            .filter(|&i| self.incidents[i].open)
            .ok_or(EngineError::NotFound)?;
        for uid in self.incidents[ii].units.clone() {
            if let Some(ui) = self.unit_idx(&uid) {
                if matches!(self.units[ui].status, Status::Dispatched | Status::OnScene) {
                    self.units[ui].status = Status::Available;
                    self.units[ui].incident_id = None;
                    self.pending.push(Change::Unit(self.units[ui].clone()));
                }
            }
        }
        self.incidents[ii].open = false;
        self.incidents[ii].closed = Some(Utc::now());
        let label = self.incidents[ii].label.clone();
        self.pending.push(Change::Incident(self.incidents[ii].clone()));
        self.logi(seat, format!("Closed {label}"), Some(incident_id));
        Ok(())
    }

    // ---- configuration ----

    pub fn add_unit(&mut self, id: &str, name: &str, group_id: &str) -> Result<(), EngineError> {
        let id = id.trim();
        if id.is_empty() || id.len() > 64 || self.unit_idx(id).is_some() {
            return Err(EngineError::Bad("unit id empty, too long or already in use".into()));
        }
        let name = if name.trim().is_empty() { id } else { name.trim() };
        let u = Unit::new(id, name, group_id);
        self.units.push(u.clone());
        self.pending.push(Change::Unit(u));
        Ok(())
    }

    /// Removes a unit from the crew. A unit working an incident has to be released first.
    pub fn remove_unit(&mut self, unit_id: &str, seat: &str) -> Result<(), EngineError> {
        let ui = self.unit_idx(unit_id).ok_or(EngineError::NotFound)?;
        if self.units[ui].incident_id.is_some() {
            return Err(EngineError::Conflict("release the unit from its incident first".into()));
        }
        let u = self.units.remove(ui);
        self.contact_seen.remove(&format!("u:{unit_id}"));
        self.pending.push(Change::RemoveUnit(unit_id.into()));
        self.logf(seat, format!("Removed unit {}", u.name));
        Ok(())
    }

    pub fn remove_repeater(&mut self, repeater_id: &str, seat: &str) -> Result<(), EngineError> {
        let ri = self
            .repeaters
            .iter()
            .position(|r| r.id == repeater_id)
            .ok_or(EngineError::NotFound)?;
        let r = self.repeaters.remove(ri);
        self.contact_seen.remove(&format!("r:{repeater_id}"));
        self.pending.push(Change::RemoveRepeater(repeater_id.into()));
        self.logf(seat, format!("Removed repeater {}", r.name));
        Ok(())
    }

    pub fn assign_group(&mut self, unit_id: &str, group_id: &str) -> Result<(), EngineError> {
        let ui = self.unit_idx(unit_id).ok_or(EngineError::NotFound)?;
        self.units[ui].group_id = group_id.into();
        self.pending.push(Change::Unit(self.units[ui].clone()));
        Ok(())
    }

    pub fn set_group(&mut self, g: Group) -> Result<(), EngineError> {
        if g.id.trim().is_empty() || g.max_mph <= 0.0 || g.adjust <= 0.0 || g.adjust > 1.5 {
            return Err(EngineError::Bad(
                "group needs id, max_mph > 0 and adjust in (0, 1.5]".into(),
            ));
        }
        match self.groups.iter_mut().find(|x| x.id == g.id) {
            Some(x) => *x = g.clone(),
            None => self.groups.push(g.clone()),
        }
        self.pending.push(Change::Group(g));
        Ok(())
    }

    pub fn set_repeater(&mut self, id: &str, name: &str, lat: f64, lon: f64, placed: bool) -> Result<(), EngineError> {
        let id = id.trim();
        if id.is_empty() {
            return Err(EngineError::Bad("repeater id".into()));
        }
        let name = if name.trim().is_empty() { id } else { name.trim() };
        let r = match self.repeaters.iter_mut().find(|r| r.id == id) {
            Some(r) => {
                r.name = name.into();
                r.lat = lat;
                r.lon = lon;
                r.placed = placed;
                r.clone()
            }
            None => {
                let r = Repeater {
                    id: id.into(),
                    name: name.into(),
                    lat,
                    lon,
                    placed,
                    node_key: String::new(),
                    last_seen: None,
                    has_credentials: false,
                    reading: None,
                    seen_mono: None,
                };
                self.repeaters.push(r.clone());
                r
            }
        };
        self.pending.push(Change::Repeater(r));
        Ok(())
    }

    pub fn update_site(&mut self, f: impl FnOnce(&mut Site)) {
        f(&mut self.site);
        self.pending.push(Change::Site(self.site.clone()));
    }

    pub fn set_settings(&mut self, s: Settings) -> Result<(), EngineError> {
        let ok = s.parked_late_min > 0.0
            && s.parked_no_contact_min > s.parked_late_min
            && s.moving_late_min > 0.0
            && s.moving_no_contact_min > s.moving_late_min
            && s.moving_min_mph >= 0.0;
        if !ok {
            return Err(EngineError::Bad(
                "late thresholds must be positive and below the no-contact thresholds".into(),
            ));
        }
        self.settings = s.clone();
        self.pending.push(Change::Settings(s));
        Ok(())
    }

    // ---- radio nodes ----

    fn clean_key(key: &str) -> Result<String, EngineError> {
        let k = key.trim().to_ascii_lowercase();
        if k.is_empty() {
            return Ok(k);
        }
        if k.len() != 64 || !k.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(EngineError::Bad("a node key is 64 hex characters".into()));
        }
        Ok(k)
    }

    pub fn unit_for_key(&self, key: &str) -> Option<String> {
        self.units
            .iter()
            .find(|u| !u.node_key.is_empty() && u.node_key == key)
            .map(|u| u.id.clone())
    }

    pub fn repeater_for_key(&self, key: &str) -> Option<String> {
        self.repeaters
            .iter()
            .find(|r| !r.node_key.is_empty() && r.node_key == key)
            .map(|r| r.id.clone())
    }

    /// Radio key of a repeater, for the reader.
    pub fn repeater_key(&self, id: &str) -> Option<String> {
        self.repeaters.iter().find(|r| r.id == id).map(|r| r.node_key.clone())
    }

    /// Units that have a radio key, for polling ones that go quiet.
    pub fn unit_keys(&self) -> Vec<(String, String)> {
        self.units
            .iter()
            .filter(|u| !u.node_key.is_empty())
            .map(|u| (u.id.clone(), u.node_key.clone()))
            .collect()
    }

    fn key_free(&self, key: &str, except_unit: Option<&str>, except_rep: Option<&str>) -> bool {
        key.is_empty()
            || (!self
                .units
                .iter()
                .any(|u| u.node_key == key && Some(u.id.as_str()) != except_unit)
                && !self
                    .repeaters
                    .iter()
                    .any(|r| r.node_key == key && Some(r.id.as_str()) != except_rep))
    }

    pub fn set_unit_key(&mut self, unit_id: &str, key: &str) -> Result<(), EngineError> {
        let key = Self::clean_key(key)?;
        let ui = self.unit_idx(unit_id).ok_or(EngineError::NotFound)?;
        if !self.key_free(&key, Some(unit_id), None) {
            return Err(EngineError::Conflict("that node is already assigned".into()));
        }
        self.units[ui].node_key = key;
        self.pending.push(Change::Unit(self.units[ui].clone()));
        Ok(())
    }

    pub fn set_repeater_key(&mut self, repeater_id: &str, key: &str) -> Result<(), EngineError> {
        let key = Self::clean_key(key)?;
        let ri = self
            .repeaters
            .iter()
            .position(|r| r.id == repeater_id)
            .ok_or(EngineError::NotFound)?;
        if !self.key_free(&key, None, Some(repeater_id)) {
            return Err(EngineError::Conflict("that node is already assigned".into()));
        }
        self.repeaters[ri].node_key = key;
        self.pending.push(Change::Repeater(self.repeaters[ri].clone()));
        Ok(())
    }

    fn fresh_id(&self, name: &str, taken: impl Fn(&str) -> bool) -> String {
        let base: String = name
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let base = if base.is_empty() { "node".to_string() } else { base };
        let base: String = base.chars().take(40).collect();
        if !taken(&base) {
            return base;
        }
        (2..).map(|n| format!("{base}-{n}")).find(|c| !taken(c)).unwrap()
    }

    /// Creates a unit for a radio node found on the mesh. Returns the new unit id.
    pub fn adopt_unit(&mut self, key: &str, name: &str, group_id: &str) -> Result<String, EngineError> {
        let key = Self::clean_key(key)?;
        if key.is_empty() {
            return Err(EngineError::Bad("node key required".into()));
        }
        if !self.key_free(&key, None, None) {
            return Err(EngineError::Conflict("that node is already assigned".into()));
        }
        let id = self.fresh_id(name, |c| self.unit_idx(c).is_some());
        self.add_unit(&id, name, group_id)?;
        self.set_unit_key(&id, &key)?;
        Ok(id)
    }

    /// Creates a repeater for a radio node found on the mesh. Returns the new repeater id.
    pub fn adopt_repeater(&mut self, key: &str, name: &str) -> Result<String, EngineError> {
        let key = Self::clean_key(key)?;
        if key.is_empty() {
            return Err(EngineError::Bad("node key required".into()));
        }
        if !self.key_free(&key, None, None) {
            return Err(EngineError::Conflict("that node is already assigned".into()));
        }
        let id = self.fresh_id(name, |c| self.repeater_exists(c));
        self.set_repeater(&id, name, 0.0, 0.0, false)?;
        self.set_repeater_key(&id, &key)?;
        Ok(id)
    }

    // ---- obstacles ----

    fn validate_obstacle(&self, o: &Obstacle) -> Result<(), EngineError> {
        let need = match o.kind {
            ObstacleKind::Wall | ObstacleKind::Gate => 2,
            ObstacleKind::NoGo | ObstacleKind::Slow => 3,
        };
        if o.points.len() < need || o.points.len() > 200 {
            return Err(EngineError::Bad(
                format!("a {:?} needs {need} to 200 points", o.kind).to_lowercase(),
            ));
        }
        let (w, h) = (self.site.img_w, self.site.img_h);
        let ok = o.points.iter().all(|p| {
            p[0].is_finite()
                && p[1].is_finite()
                && p[0] >= -w * 0.1
                && p[0] <= w * 1.1
                && p[1] >= -h * 0.1
                && p[1] <= h * 1.1
        });
        if !ok {
            return Err(EngineError::Bad("points must lie on the overlay".into()));
        }
        if o.kind == ObstacleKind::Slow && !(o.factor > 0.0 && o.factor <= 1.0) {
            return Err(EngineError::Bad(
                "slow zone speed must be above 0 and at most 100%".into(),
            ));
        }
        if o.name.chars().count() > 60 {
            return Err(EngineError::Bad("name too long".into()));
        }
        Ok(())
    }

    pub fn obstacles(&self) -> &[Obstacle] {
        &self.obstacles
    }

    /// Adds an obstacle (id 0) or replaces one. Returns it with its id.
    pub fn set_obstacle(&mut self, mut o: Obstacle, seat: &str) -> Result<Obstacle, EngineError> {
        self.validate_obstacle(&o)?;
        o.name = o.name.trim().to_string();
        if o.kind != ObstacleKind::Slow {
            o.factor = 1.0;
        }
        if o.kind != ObstacleKind::Gate {
            o.open = false;
        }
        if o.id == 0 {
            if self.obstacles.len() >= 500 {
                return Err(EngineError::Bad("too many obstacles".into()));
            }
            o.id = self.obstacles.iter().map(|x| x.id).max().unwrap_or(0) + 1;
            self.obstacles.push(o.clone());
            let what = if o.name.is_empty() {
                format!("{:?}", o.kind).to_lowercase()
            } else {
                o.name.clone()
            };
            self.logf(seat, format!("Added obstacle: {what}"));
        } else {
            let x = self
                .obstacles
                .iter_mut()
                .find(|x| x.id == o.id)
                .ok_or(EngineError::NotFound)?;
            *x = o.clone();
        }
        self.pending.push(Change::Obstacles(self.obstacles.clone()));
        Ok(o)
    }

    pub fn delete_obstacle(&mut self, id: i32, seat: &str) -> Result<(), EngineError> {
        let i = self
            .obstacles
            .iter()
            .position(|x| x.id == id)
            .ok_or(EngineError::NotFound)?;
        let o = self.obstacles.remove(i);
        self.pending.push(Change::Obstacles(self.obstacles.clone()));
        let what = if o.name.is_empty() {
            format!("{:?}", o.kind).to_lowercase()
        } else {
            o.name
        };
        self.logf(seat, format!("Removed obstacle: {what}"));
        Ok(())
    }

    /// Opens or closes a gate. Dispatchers do this as conditions change during an event.
    pub fn set_gate(&mut self, id: i32, open: bool, seat: &str) -> Result<(), EngineError> {
        let o = self
            .obstacles
            .iter_mut()
            .find(|x| x.id == id)
            .ok_or(EngineError::NotFound)?;
        if o.kind != ObstacleKind::Gate {
            return Err(EngineError::Bad("not a gate".into()));
        }
        if o.open == open {
            return Ok(());
        }
        o.open = open;
        let name = if o.name.is_empty() {
            format!("Gate {}", o.id)
        } else {
            o.name.clone()
        };
        self.pending.push(Change::Obstacles(self.obstacles.clone()));
        self.logf(seat, format!("{name} {}", if open { "opened" } else { "closed" }));
        Ok(())
    }

    // ---- site profiles ----

    /// The current site, its obstacles and where its repeaters stand, for saving or exporting.
    pub fn export_site(&self) -> SiteData {
        SiteData {
            site: self.site.clone(),
            obstacles: self.obstacles.clone(),
            repeaters: self
                .repeaters
                .iter()
                .map(|r| Placement {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    node_key: r.node_key.clone(),
                    lat: r.lat,
                    lon: r.lon,
                    placed: r.placed,
                })
                .collect(),
        }
    }

    /// Makes a saved site the active one. Refused while an incident is open, because incidents
    /// are located on the current overlay.
    pub fn activate_site(&mut self, data: SiteData, seat: &str) -> Result<(), EngineError> {
        if self.incidents.iter().any(|i| i.open) {
            return Err(EngineError::Conflict(
                "close the open incidents before switching sites".into(),
            ));
        }
        if data.site.slogan.chars().count() > 60 {
            return Err(EngineError::Bad("slogan too long".into()));
        }
        let name = data.site.name.clone();
        self.site = data.site;
        self.pending.push(Change::Site(self.site.clone()));
        let mut obstacles = Vec::new();
        for (n, mut o) in data.obstacles.into_iter().enumerate() {
            o.id = n as i32 + 1;
            self.validate_obstacle(&o)?;
            obstacles.push(o);
        }
        self.obstacles = obstacles;
        self.pending.push(Change::Obstacles(self.obstacles.clone()));
        // Repeaters belong to the crew's kit and keep their passwords; only where they stand changes.
        let mut touched = Vec::new();
        for r in &mut self.repeaters {
            match data.repeaters.iter().find(|p| p.id == r.id) {
                Some(p) => {
                    r.lat = p.lat;
                    r.lon = p.lon;
                    r.placed = p.placed;
                }
                None => r.placed = false,
            }
            touched.push(r.clone());
        }
        for r in touched {
            self.pending.push(Change::Repeater(r));
        }
        for p in data.repeaters {
            if !self.repeater_exists(&p.id) {
                self.set_repeater(&p.id, &p.name, p.lat, p.lon, p.placed)?;
                if !p.node_key.is_empty() && self.key_free(&p.node_key, None, None) {
                    let _ = self.set_repeater_key(&p.id, &p.node_key);
                }
            }
        }
        self.logf(seat, format!("Switched to site {name}"));
        Ok(())
    }

    /// Fill in a demo site, units and repeaters when the engine is empty (simulator mode).
    pub fn seed_demo(&mut self) {
        if !self.units.is_empty() {
            return;
        }
        self.site.name = "Demo site".into();
        self.site.img_w = 1000.0;
        self.site.img_h = 700.0;
        self.site.a = Anchor {
            px: 0.0,
            py: 0.0,
            lat: 39.002,
            lon: -104.003,
        };
        self.site.b = Anchor {
            px: 1000.0,
            py: 700.0,
            lat: 38.998,
            lon: -103.997,
        };
        self.pending.push(Change::Site(self.site.clone()));
        for g in [
            Group {
                id: "utv".into(),
                name: "UTV".into(),
                max_mph: 15.0,
                adjust: 0.8,
            },
            Group {
                id: "golf".into(),
                name: "Golf cart".into(),
                max_mph: 8.0,
                adjust: 0.9,
            },
        ] {
            let _ = self.set_group(g);
        }
        for (id, name, g) in [
            ("u1", "UTV 1", "utv"),
            ("u2", "UTV 2", "utv"),
            ("u3", "UTV 3", "utv"),
            ("u4", "UTV 4", "utv"),
            ("u5", "UTV 5", "utv"),
            ("c1", "Cart 1", "golf"),
            ("u6", "UTV 6", "utv"),
        ] {
            let _ = self.add_unit(id, name, g);
        }
        for (id, name, lat, lon) in [
            ("r1", "Repeater 1", 39.0012, -104.0020),
            ("r2", "Repeater 2", 39.0008, -103.9975),
            ("r3", "Repeater 3", 38.9985, -104.0000),
        ] {
            let _ = self.set_repeater(id, name, lat, lon, true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eng() -> Engine {
        let mut e = Engine::new(State::default());
        e.site.a = Anchor {
            px: 0.0,
            py: 0.0,
            lat: 39.002,
            lon: -104.003,
        };
        e.site.b = Anchor {
            px: 1000.0,
            py: 700.0,
            lat: 38.998,
            lon: -103.997,
        };
        e.groups = vec![
            Group {
                id: "utv".into(),
                name: "UTV".into(),
                max_mph: 15.0,
                adjust: 0.8,
            },
            Group {
                id: "slow".into(),
                name: "Cart".into(),
                max_mph: 5.0,
                adjust: 1.0,
            },
        ];
        for (id, g) in [("a", "utv"), ("b", "utv"), ("c", "slow")] {
            e.add_unit(id, &format!("Unit {id}"), g).unwrap();
        }
        e
    }

    fn pos(id: &str, lat: f64, lon: f64, mph: f64) -> Position {
        Position {
            unit_id: id.into(),
            lat,
            lon,
            speed_mph: Some(mph),
            age_secs: 0,
            battery: 50,
        }
    }

    fn m(x: f64) -> Duration {
        Duration::from_secs_f64(x * 60.0)
    }

    #[test]
    fn contact_rules() {
        let s = Settings::default();
        let cases = [
            ("never reported", false, false, 0.0, Contact::NoContact),
            ("parked fresh", true, false, 5.0, Contact::Normal),
            (
                "parked at 15 exactly is still normal",
                true,
                false,
                15.0,
                Contact::Normal,
            ),
            ("parked missed one check-in", true, false, 16.0, Contact::Late),
            ("parked at 30 exactly is late", true, false, 30.0, Contact::Late),
            ("parked missed two check-ins", true, false, 30.1, Contact::NoContact),
            ("moving fresh", true, true, 1.0, Contact::Normal),
            ("moving late", true, true, 3.0, Contact::Late),
            ("moving gone", true, true, 11.0, Contact::NoContact),
        ];
        for (name, has, moving, age, want) in cases {
            assert_eq!(contact_state(&s, has, moving, m(age)), want, "{name}");
        }
    }

    #[test]
    fn nearest_order_and_exclusions() {
        let mut e = eng();
        e.report_position(pos("a", 39.0000, -104.0000, 0.0));
        e.report_position(pos("b", 39.0010, -104.0000, 0.0));
        let got = e.nearest(39.0001, -104.0000);
        assert_eq!(got.iter().map(|c| c.unit_id.as_str()).collect::<Vec<_>>(), ["a", "b"]); // c never reported
        e.set_status("a", Status::Busy, "t").unwrap();
        let got = e.nearest(39.0001, -104.0000);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].unit_id, "b");
    }

    #[test]
    fn nearest_puts_no_contact_last() {
        let mut e = eng();
        e.report_position(pos("a", 39.0000, -104.0000, 0.0)); // closest
        e.report_position(pos("b", 39.0010, -104.0000, 0.0));
        e.units[0].seen_mono = Some(Instant::now() - Duration::from_secs(3600)); // parked and silent for an hour
        let got = e.nearest(39.0000, -104.0000);
        assert_eq!(got[0].unit_id, "b");
        assert_eq!(got[1].contact, Contact::NoContact);
    }

    #[test]
    fn eta_uses_group_speed() {
        let d = 10.0 * crate::geo::MPS_PER_MPH * 60.0; // distance covered in 60 s at 10 mph
        let g = Group {
            id: "g".into(),
            name: "g".into(),
            max_mph: 10.0,
            adjust: 1.0,
        };
        assert!((eta_seconds(d, &g) - 60.0).abs() < 0.01);
        let g = Group { adjust: 0.5, ..g };
        assert!((eta_seconds(d, &g) - 120.0).abs() < 0.01);
        let g = Group { max_mph: 0.0, ..g };
        assert!(eta_seconds(d, &g).is_infinite());
    }

    #[test]
    fn dispatch_flow_and_double_dispatch() {
        let mut e = eng();
        let i1 = e.open_incident(39.0, -104.0, "", "seat1").id;
        e.dispatch(i1, &["a".into()], "seat1").unwrap();
        let i2 = e.open_incident(39.001, -104.0, "", "seat2").id;
        assert!(matches!(
            e.dispatch(i2, &["a".into()], "seat2"),
            Err(EngineError::Conflict(_))
        ));
        // all or nothing: b is free but a is taken, so b must stay available
        assert!(matches!(
            e.dispatch(i2, &["b".into(), "a".into()], "seat2"),
            Err(EngineError::Conflict(_))
        ));
        assert_eq!(e.units[1].status, Status::Available);
        e.arrive(i1, "a", "seat1").unwrap();
        assert_eq!(e.units[0].status, Status::OnScene);
        e.close_incident(i1, "seat1").unwrap();
        assert_eq!(e.units[0].status, Status::Available);
        assert!(e.units[0].incident_id.is_none());
    }

    #[test]
    fn duplicate_unit_in_one_dispatch_rejected() {
        let mut e = eng();
        let i = e.open_incident(39.0, -104.0, "", "s").id;
        assert!(matches!(
            e.dispatch(i, &["a".into(), "a".into()], "s"),
            Err(EngineError::Bad(_))
        ));
        assert_eq!(e.units[0].status, Status::Available);
    }

    #[test]
    fn manual_status_releases_from_incident() {
        let mut e = eng();
        let i = e.open_incident(39.0, -104.0, "", "x").id;
        e.dispatch(i, &["a".into()], "x").unwrap();
        e.set_status("a", Status::OffDuty, "x").unwrap();
        assert!(e.units[0].incident_id.is_none());
        assert!(e.incidents[0].units.is_empty());
        e.close_incident(i, "x").unwrap();
        assert_eq!(e.units[0].status, Status::OffDuty); // closing must not bring it back
    }

    #[test]
    fn errors() {
        let mut e = eng();
        assert_eq!(e.set_status("zzz", Status::Busy, "x"), Err(EngineError::NotFound));
        assert!(e.dispatch(99, &["a".into()], "x").is_err());
        assert!(e.add_unit("a", "dup", "utv").is_err());
        assert!(e.add_unit("", "", "utv").is_err());
        let g = Group {
            id: "x".into(),
            name: "x".into(),
            max_mph: 10.0,
            adjust: 9.0,
        };
        assert!(e.set_group(g).is_err());
    }

    #[test]
    fn moving_flag_from_speed() {
        let mut e = eng();
        e.report_position(pos("a", 39.0, -104.0, 8.0));
        assert!(e.units[0].moving);
        e.report_position(pos("a", 39.0, -104.0, 0.3));
        assert!(!e.units[0].moving);
    }

    #[test]
    fn georef_round_trip() {
        let e = eng();
        let (px, py) = e.site.to_pixel(39.0, -104.0).unwrap();
        let (lat, lon) = e.site.to_lat_lon(px, py).unwrap();
        assert!((lat - 39.0).abs() < 1e-9 && (lon + 104.0).abs() < 1e-9);
        assert!(Site::default().to_pixel(1.0, 1.0).is_none());
    }

    #[test]
    fn restart_shows_no_contact_until_a_report() {
        let mut e = eng();
        e.report_position(pos("a", 39.0, -104.0, 0.0));
        let units = e.units.clone();
        let e2 = Engine::new(State {
            units,
            ..State::default()
        });
        let snap = e2.snapshot();
        assert_eq!(snap.units[0].contact, Contact::NoContact);
        assert!(snap.units[0].unit.has_pos);
    }

    #[test]
    fn changes_are_recorded_in_order() {
        let mut e = eng();
        e.drain_changes();
        let i = e.open_incident(39.0, -104.0, "Fire", "s").id;
        e.dispatch(i, &["a".into()], "s").unwrap();
        let ch = e.drain_changes();
        assert!(matches!(ch.first(), Some(Change::Incident(_))));
        assert!(ch
            .iter()
            .any(|c| matches!(c, Change::Unit(u) if u.id == "a" && u.status == Status::Dispatched)));
        assert!(e.drain_changes().is_empty());
    }

    #[test]
    fn successful_reading_counts_as_check_in_failed_does_not() {
        let mut e = eng();
        e.set_repeater("r1", "R1", 39.0, -104.0, true).unwrap();
        e.drain_changes();
        e.record_reading("r1", false, "no response", serde_json::Value::Null)
            .unwrap();
        let snap = e.snapshot();
        assert_eq!(snap.repeaters[0].contact, Contact::NoContact);
        assert!(!snap.repeaters[0].repeater.reading.as_ref().unwrap().ok);
        e.record_reading("r1", true, "", serde_json::json!({ "battery_mv": 4100 }))
            .unwrap();
        let snap = e.snapshot();
        assert_eq!(snap.repeaters[0].contact, Contact::Normal);
        assert_eq!(
            snap.repeaters[0].repeater.reading.as_ref().unwrap().data["battery_mv"],
            4100
        );
        assert!(matches!(
            e.record_reading("nope", true, "", serde_json::Value::Null),
            Err(EngineError::NotFound)
        ));
    }

    #[test]
    fn credentialed_repeaters_listed() {
        let mut e = eng();
        e.set_repeater("r1", "R1", 0.0, 0.0, false).unwrap();
        e.set_repeater("r2", "R2", 0.0, 0.0, false).unwrap();
        e.set_has_credentials("r2", true).unwrap();
        assert_eq!(e.credentialed_repeaters(), vec!["r2".to_string()]);
        assert!(e.set_has_credentials("zz", true).is_err());
    }

    #[test]
    fn settings_validated() {
        let mut e = eng();
        let bad = Settings {
            parked_late_min: 30.0,
            parked_no_contact_min: 15.0,
            ..Settings::default()
        };
        assert!(e.set_settings(bad).is_err());
        assert!(e.set_settings(Settings::default()).is_ok());
    }

    fn at_px(e: &Engine, px: f64, py: f64) -> (f64, f64) {
        e.site.to_lat_lon(px, py).unwrap()
    }

    fn pos_at(id: &str, ll: (f64, f64), speed: Option<f64>, age: u64) -> Position {
        Position {
            unit_id: id.into(),
            lat: ll.0,
            lon: ll.1,
            speed_mph: speed,
            battery: -1,
            age_secs: age,
        }
    }

    fn wall(points: &[[f64; 2]]) -> Obstacle {
        Obstacle {
            id: 0,
            kind: ObstacleKind::Wall,
            name: "Fence".into(),
            points: points.to_vec(),
            open: false,
            factor: 1.0,
        }
    }

    #[test]
    fn obstacles_change_who_is_nearest() {
        let mut e = eng();
        let inc = at_px(&e, 800.0, 350.0);
        e.report_position(pos_at("a", at_px(&e, 600.0, 350.0), Some(0.0), 0));
        e.report_position(pos_at("b", at_px(&e, 800.0, 600.0), Some(0.0), 0));
        let plain = e.nearest(inc.0, inc.1);
        assert_eq!(plain[0].unit_id, "a");
        assert!(!plain[0].routed && plain[0].path_px.is_none());

        e.set_obstacle(wall(&[[700.0, 0.0], [700.0, 500.0]]), "t").unwrap();
        let routed = e.nearest(inc.0, inc.1);
        assert_eq!(routed[0].unit_id, "b", "the fence puts a behind b: {routed:?}");
        let a = routed.iter().find(|c| c.unit_id == "a").unwrap();
        assert!(a.routed && a.dist_m > a.straight_m * 1.5, "{a:?}");
        let path = a.path_px.as_ref().unwrap();
        assert!(
            path.iter().any(|p| p[1] > 495.0),
            "route goes round the fence end: {path:?}"
        );
        // ETA follows the longer route.
        let (eta_a, eta_b) = (a.eta_sec.unwrap(), routed[0].eta_sec.unwrap());
        assert!(eta_a > eta_b);

        // Closing a gate in the fence is a dispatcher action and shows in the log.
        let g = e
            .set_obstacle(
                Obstacle {
                    id: 0,
                    kind: ObstacleKind::Gate,
                    name: "Gate 1".into(),
                    points: vec![[700.0, 500.0], [700.0, 700.0]],
                    open: false,
                    factor: 1.0,
                },
                "t",
            )
            .unwrap();
        assert!(!e.nearest(inc.0, inc.1)[1].routed || e.nearest(inc.0, inc.1)[1].path_px.is_some());
        e.set_gate(g.id, true, "seat 2").unwrap();
        assert!(e.log.last().unwrap().text.contains("Gate 1 opened"));
        assert!(e.set_gate(1, true, "x").is_err(), "a fence is not a gate");
    }

    #[test]
    fn a_walled_off_unit_ranks_below_reachable_ones() {
        let mut e = eng();
        let inc = at_px(&e, 800.0, 350.0);
        e.report_position(pos_at("a", at_px(&e, 100.0, 350.0), Some(0.0), 0));
        e.report_position(pos_at("b", at_px(&e, 500.0, 350.0), Some(0.0), 0));
        // A full-height wall between a and b.
        e.set_obstacle(wall(&[[300.0, -10.0], [300.0, 710.0]]), "t").unwrap();
        let got = e.nearest(inc.0, inc.1);
        assert_eq!(got.iter().map(|c| c.unit_id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert!(got[0].routed && !got[1].routed);
        assert_eq!(got[1].dist_m, got[1].straight_m, "falls back to straight line");
    }

    #[test]
    fn obstacle_validation() {
        let mut e = eng();
        assert!(e.set_obstacle(wall(&[[0.0, 0.0]]), "t").is_err());
        assert!(e.set_obstacle(wall(&[[0.0, 0.0], [5000.0, 0.0]]), "t").is_err());
        assert!(e.set_obstacle(wall(&[[0.0, 0.0], [f64::NAN, 0.0]]), "t").is_err());
        let mut zone = wall(&[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]]);
        zone.kind = ObstacleKind::Slow;
        zone.factor = 0.0;
        assert!(e.set_obstacle(zone.clone(), "t").is_err());
        zone.factor = 0.5;
        let z = e.set_obstacle(zone, "t").unwrap();
        assert_eq!(z.id, 1);
        assert_eq!(e.delete_obstacle(99, "t"), Err(EngineError::NotFound));
        e.delete_obstacle(1, "t").unwrap();
        assert!(e.obstacles().is_empty());
        let ch = e.drain_changes();
        assert!(ch.iter().any(|c| matches!(c, Change::Obstacles(v) if v.is_empty())));
    }

    #[test]
    fn speed_is_worked_out_from_movement_and_old_reports_are_ignored() {
        let mut e = eng();
        let p0 = (39.0000, -104.0000);
        let p300 = (39.0000 + 300.0 / 110_574.0, -104.0000); // 300 m north
        e.report_position(pos_at("a", p0, None, 60));
        assert_eq!(e.units[0].speed_mph, 0.0);
        e.report_position(pos_at("a", p300, None, 0));
        let mph = e.units[0].speed_mph;
        assert!((mph - 300.0 / 60.0 / MPS_PER_MPH).abs() < 0.3, "{mph}");
        assert!(e.units[0].moving);

        // A report older than the one held (a replay after reconnect) changes nothing.
        e.report_position(pos_at("a", p0, None, 300));
        assert!((e.units[0].lat - p300.0).abs() < 1e-9);

        // GPS wander while parked is not movement.
        let mut e = eng();
        e.report_position(pos_at("a", p0, None, 900));
        e.report_position(pos_at("a", (p0.0 + 3.0 / 110_574.0, p0.1), None, 0));
        assert_eq!(e.units[0].speed_mph, 0.0);
        assert!(!e.units[0].moving);
    }

    #[test]
    fn recovered_report_carries_its_age_into_contact_state() {
        let mut e = eng();
        e.report_position(pos_at("a", (39.0, -104.0), None, 20 * 60));
        let snap = e.snapshot();
        let a = snap.units.iter().find(|u| u.unit.id == "a").unwrap();
        assert_eq!(a.contact, Contact::Late, "parked and 20 minutes old");
        assert!(a.age_sec > 1190.0);
    }

    #[test]
    fn battery_is_kept_when_a_report_has_none() {
        let mut e = eng();
        e.report_battery("a", 80);
        e.report_position(pos_at("a", (39.0, -104.0), Some(0.0), 0)); // battery -1
        assert_eq!(e.units[0].battery, 80);
    }

    #[test]
    fn contact_changes_are_logged_once_and_not_at_startup() {
        let mut e = eng();
        e.tick(); // nothing has reported: no noise at startup
        assert!(e.log.is_empty());
        e.report_position(pos_at("a", (39.0, -104.0), Some(0.0), 0));
        e.tick();
        assert!(e.log.is_empty(), "first contact after start is not news");

        e.units[0].seen_mono = Some(Instant::now() - m(16.0));
        e.tick();
        e.tick();
        assert_eq!(e.log.len(), 1, "logged once: {:?}", e.log);
        assert!(e.log[0].text.contains("Unit a is late"));
        assert_eq!(e.log[0].seat, "system");

        let inc = e.open_incident(39.0, -104.0, "", "t");
        e.dispatch(inc.id, &["a".to_string()], "t").unwrap();
        e.units[0].seen_mono = Some(Instant::now() - m(31.0));
        let alerts = e.alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].severity, "crit", "a dispatched unit going dark is critical");
        e.tick();
        let last = e.log.last().unwrap();
        assert!(last.text.contains("no contact while dispatched"), "{}", last.text);
        assert_eq!(last.incident, Some(inc.id));

        e.report_position(pos_at("a", (39.0, -104.0), Some(0.0), 0));
        e.tick();
        assert!(e.log.last().unwrap().text.contains("back in contact"));
        assert!(e.alerts().is_empty());
    }

    #[test]
    fn off_duty_units_raise_no_alerts() {
        let mut e = eng();
        e.report_position(pos_at("a", (39.0, -104.0), Some(0.0), 0));
        e.set_status("a", Status::OffDuty, "t").unwrap();
        e.units[0].seen_mono = Some(Instant::now() - m(60.0));
        assert!(e.alerts().is_empty());
    }

    #[test]
    fn repeater_silence_is_flagged_only_for_placed_repeaters() {
        let mut e = eng();
        e.set_repeater("r1", "R1", 39.0, -104.0, true).unwrap();
        e.set_repeater("r2", "R2", 0.0, 0.0, false).unwrap();
        e.set_has_credentials("r1", true).unwrap();
        e.set_has_credentials("r2", true).unwrap();
        e.report_repeater("r1");
        e.report_repeater("r2");
        e.repeaters[0].seen_mono = Some(Instant::now() - m(45.0));
        e.repeaters[1].seen_mono = Some(Instant::now() - m(45.0));
        let alerts = e.alerts();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, "repeater");
    }

    const K1: &str = "11111111111111111111111111111111111111111111111111111111111111ab";
    const K2: &str = "22222222222222222222222222222222222222222222222222222222222222cd";

    #[test]
    fn adopting_radio_nodes() {
        let mut e = eng();
        let id = e.adopt_unit(&K1.to_uppercase(), "UTV 7", "utv").unwrap();
        assert_eq!(id, "utv-7");
        assert_eq!(e.unit_for_key(K1), Some(id.clone()));
        assert_eq!(e.units.iter().find(|u| u.id == id).unwrap().group_id, "utv");
        // The same node cannot be adopted twice, as a unit or a repeater.
        assert!(matches!(
            e.adopt_unit(K1, "Again", "utv"),
            Err(EngineError::Conflict(_))
        ));
        assert!(matches!(e.adopt_repeater(K1, "Again"), Err(EngineError::Conflict(_))));
        let r = e.adopt_repeater(K2, "Rpt North").unwrap();
        assert_eq!(r, "rpt-north");
        assert_eq!(e.repeater_for_key(K2), Some(r.clone()));
        assert!(
            !e.repeaters[0].placed,
            "an adopted repeater waits to be placed on the map"
        );
        // Name collisions get a suffix.
        let k3 = "33".repeat(32);
        assert_eq!(e.adopt_unit(&k3, "UTV 7", "utv").unwrap(), "utv-7-2");
        assert!(e.adopt_unit("xyz", "Bad", "utv").is_err());
        assert!(e.set_unit_key("a", "").is_ok(), "clearing a key is allowed");
    }

    #[test]
    fn switching_sites_swaps_overlay_obstacles_and_placements_but_keeps_the_crew() {
        let mut e = eng();
        e.set_repeater("r1", "R1", 39.0, -104.0, true).unwrap();
        e.set_repeater("r2", "R2", 39.1, -104.1, true).unwrap();
        e.set_has_credentials("r1", true).unwrap();
        e.set_obstacle(wall(&[[0.0, 0.0], [100.0, 100.0]]), "t").unwrap();
        e.report_position(pos_at("a", (39.0, -104.0), Some(0.0), 0));
        let before = e.export_site();

        let mut other = before.clone();
        other.site.name = "Airshow B".into();
        other.obstacles = vec![];
        other.repeaters = vec![Placement {
            id: "r2".into(),
            name: "R2".into(),
            node_key: String::new(),
            lat: 40.0,
            lon: -105.0,
            placed: true,
        }];
        let inc = e.open_incident(39.0, -104.0, "", "t");
        assert!(matches!(
            e.activate_site(other.clone(), "t"),
            Err(EngineError::Conflict(_))
        ));
        e.close_incident(inc.id, "t").unwrap();
        e.activate_site(other, "t").unwrap();
        assert_eq!(e.site.name, "Airshow B");
        assert!(e.obstacles.is_empty());
        let r1 = e.repeaters.iter().find(|r| r.id == "r1").unwrap();
        let r2 = e.repeaters.iter().find(|r| r.id == "r2").unwrap();
        assert!(
            !r1.placed && r1.has_credentials,
            "unlisted repeater is unplaced, keeps its password"
        );
        assert!(r2.placed && (r2.lat - 40.0).abs() < 1e-9);
        assert!(e.units[0].has_pos, "units belong to the crew, not the site");
        assert!(e.log.last().unwrap().text.contains("Switched to site Airshow B"));

        // Switching back restores the first site's obstacles, renumbered from 1.
        e.activate_site(before, "t").unwrap();
        assert_eq!(e.obstacles.len(), 1);
        assert_eq!(e.obstacles[0].id, 1);
        let ch = e.drain_changes();
        assert!(ch
            .iter()
            .any(|c| matches!(c, Change::Site(s) if s.name == "Unnamed site")));
    }
}
