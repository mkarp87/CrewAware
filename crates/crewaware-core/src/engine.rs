use crate::geo::{eta_seconds, haversine_m, FEET_PER_M};
use crate::model::*;
use chrono::{DateTime, Utc};
use serde::Serialize;
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
    next_incident: i32,
    pending: Vec<Change>,
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
}

#[derive(Serialize, Debug, Clone)]
pub struct Candidate {
    pub unit_id: String,
    pub name: String,
    pub group: String,
    pub dist_m: f64,
    pub dist_ft: f64,
    /// None when the group has no usable speed.
    pub eta_sec: Option<f64>,
    pub contact: Contact,
    pub age_sec: f64,
}

fn age_of(t: Option<Instant>) -> Duration {
    t.map(|i| i.elapsed()).unwrap_or_default()
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
            next_incident: s.next_incident.max(1),
            pending: Vec::new(),
        }
    }

    /// Takes the changes recorded since the last call, in order.
    pub fn drain_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.pending)
    }

    fn logf(&mut self, seat: &str, text: String) {
        let e = LogEntry {
            t: Utc::now(),
            seat: seat.into(),
            text,
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
        let age = age_of(u.seen_mono);
        (contact_state(&self.settings, u.seen_mono.is_some(), u.moving, age), age)
    }

    pub fn site(&self) -> &Site {
        &self.site
    }
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    // ---- reports from transports ----

    /// Records a position report. Unknown unit ids are ignored.
    pub fn report_position(&mut self, p: Position) {
        let Some(i) = self.unit_idx(&p.unit_id) else { return };
        let min = self.settings.moving_min_mph;
        let now = Utc::now();
        let u = &mut self.units[i];
        u.has_pos = true;
        u.lat = p.lat;
        u.lon = p.lon;
        u.speed_mph = p.speed_mph;
        u.moving = p.speed_mph >= min;
        u.battery = p.battery;
        u.last_seen = Some(now);
        u.seen_mono = Some(Instant::now());
        let u = u.clone();
        self.pending.push(Change::Position {
            unit_id: u.id.clone(),
            lat: u.lat,
            lon: u.lon,
            speed_mph: u.speed_mph,
            battery: u.battery,
            at: now,
        });
        self.pending.push(Change::Unit(u));
    }

    pub fn report_repeater(&mut self, id: &str) {
        if let Some(r) = self.repeaters.iter_mut().find(|r| r.id == id) {
            r.last_seen = Some(Utc::now());
            r.seen_mono = Some(Instant::now());
            let r = r.clone();
            self.pending.push(Change::Repeater(r));
        }
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
                let age = age_of(r.seen_mono);
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
        }
    }

    /// Available units ranked by straight-line distance. Units with no contact are included
    /// but sorted after live ones, so command sees them without being steered toward a unit
    /// that may be gone.
    pub fn nearest(&self, lat: f64, lon: f64) -> Vec<Candidate> {
        let mut out: Vec<Candidate> = self
            .units
            .iter()
            .filter(|u| u.status == Status::Available && u.has_pos)
            .map(|u| {
                let (contact, age) = self.contact_of(u);
                let d = haversine_m(lat, lon, u.lat, u.lon);
                let g = self.group_of(&u.group_id);
                let eta = eta_seconds(d, &g);
                Candidate {
                    unit_id: u.id.clone(),
                    name: u.name.clone(),
                    group: g.name,
                    dist_m: d,
                    dist_ft: d * FEET_PER_M,
                    eta_sec: eta.is_finite().then_some(eta),
                    contact,
                    age_sec: age.as_secs_f64(),
                }
            })
            .collect();
        out.sort_by(|a, b| {
            let (ga, gb) = (a.contact == Contact::NoContact, b.contact == Contact::NoContact);
            ga.cmp(&gb).then(a.dist_m.total_cmp(&b.dist_m))
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
        self.logf(seat, format!("Opened {}", inc.label));
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
            self.logf(seat, format!("Dispatched {name} to {label}"));
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
        self.logf(seat, text);
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
        self.logf(seat, format!("Closed {label}"));
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
            speed_mph: mph,
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
}
