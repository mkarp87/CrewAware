use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// Dispatcher-controlled status. Contact state is derived separately from report age.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Available,
    Dispatched,
    OnScene,
    Busy,
    OffDuty,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Available => "available",
            Status::Dispatched => "dispatched",
            Status::OnScene => "on_scene",
            Status::Busy => "busy",
            Status::OffDuty => "off_duty",
        }
    }
    pub fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "available" => Status::Available,
            "dispatched" => Status::Dispatched,
            "on_scene" => Status::OnScene,
            "busy" => Status::Busy,
            "off_duty" => Status::OffDuty,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Contact {
    Normal,
    Late,
    NoContact,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub parked_late_min: f64,
    pub parked_no_contact_min: f64,
    pub moving_late_min: f64,
    pub moving_no_contact_min: f64,
    pub moving_min_mph: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            parked_late_min: 15.0,
            parked_no_contact_min: 30.0,
            moving_late_min: 2.0,
            moving_no_contact_min: 10.0,
            moving_min_mph: 2.0,
        }
    }
}

/// Derives a contact state from the age of the last report. Parked units fall back to a
/// slow check-in so they get long thresholds. A unit that never reported is no contact.
pub fn contact_state(s: &Settings, has_report: bool, moving: bool, age: Duration) -> Contact {
    if !has_report {
        return Contact::NoContact;
    }
    let (late, none) = if moving {
        (s.moving_late_min, s.moving_no_contact_min)
    } else {
        (s.parked_late_min, s.parked_no_contact_min)
    };
    let m = age.as_secs_f64() / 60.0;
    if m > none {
        Contact::NoContact
    } else if m > late {
        Contact::Late
    } else {
        Contact::Normal
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    pub max_mph: f64,
    pub adjust: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unit {
    pub id: String,
    pub name: String,
    pub group_id: String,
    pub status: Status,
    pub has_pos: bool,
    pub lat: f64,
    pub lon: f64,
    pub speed_mph: f64,
    pub moving: bool,
    /// Percent, -1 when unknown.
    pub battery: i32,
    pub last_seen: Option<DateTime<Utc>>,
    pub incident_id: Option<i32>,
    /// Monotonic reading used for ages. Never persisted, so a restart shows no contact until a
    /// new report, and a wall-clock jump cannot change contact states.
    #[serde(skip)]
    pub seen_mono: Option<Instant>,
}

impl Unit {
    pub fn new(id: &str, name: &str, group_id: &str) -> Unit {
        Unit {
            id: id.into(),
            name: name.into(),
            group_id: group_id.into(),
            status: Status::Available,
            has_pos: false,
            lat: 0.0,
            lon: 0.0,
            speed_mph: 0.0,
            moving: false,
            battery: -1,
            last_seen: None,
            incident_id: None,
            seen_mono: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repeater {
    pub id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub placed: bool,
    pub last_seen: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub seen_mono: Option<Instant>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Incident {
    pub id: i32,
    pub label: String,
    pub lat: f64,
    pub lon: f64,
    pub opened: DateTime<Utc>,
    pub closed: Option<DateTime<Utc>>,
    pub open: bool,
    pub units: Vec<String>,
    pub opened_by: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogEntry {
    pub t: DateTime<Utc>,
    pub seat: String,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub px: f64,
    pub py: f64,
    pub lat: f64,
    pub lon: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Site {
    pub name: String,
    /// Asset name of the overlay image, empty for none.
    pub overlay: String,
    pub img_w: f64,
    pub img_h: f64,
    pub a: Anchor,
    pub b: Anchor,
    pub logo: String,
    pub slogan: String,
}

impl Default for Site {
    fn default() -> Self {
        Site {
            name: "Unnamed site".into(),
            overlay: String::new(),
            img_w: 1000.0,
            img_h: 700.0,
            a: Anchor::default(),
            b: Anchor::default(),
            logo: String::new(),
            slogan: String::new(),
        }
    }
}

/// What a transport hands the engine.
#[derive(Clone, Debug)]
pub struct Position {
    pub unit_id: String,
    pub lat: f64,
    pub lon: f64,
    pub speed_mph: f64,
    /// -1 when unknown.
    pub battery: i32,
}
