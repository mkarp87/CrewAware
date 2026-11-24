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
    /// Public key (64 hex characters) of the MeshCore node carried by this unit, or empty.
    #[serde(default)]
    pub node_key: String,
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
            node_key: String::new(),
            seen_mono: None,
        }
    }
}

/// The result of the last attempt to read a repeater over the mesh. `data` holds whatever the radio
/// layer returned (battery, noise floor, airtime and so on) so the schema does not have to change
/// when the protocol layer learns to read more.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RepeaterReading {
    pub read_at: DateTime<Utc>,
    pub ok: bool,
    /// Why the attempt failed; empty on success.
    pub error: String,
    pub data: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Repeater {
    pub id: String,
    pub name: String,
    pub lat: f64,
    pub lon: f64,
    pub placed: bool,
    /// Public key (64 hex characters) of the repeater node, or empty.
    #[serde(default)]
    pub node_key: String,
    pub last_seen: Option<DateTime<Utc>>,
    /// Whether a login password is stored. The password itself never leaves the store.
    #[serde(default)]
    pub has_credentials: bool,
    pub reading: Option<RepeaterReading>,
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
    /// The incident this entry belongs to, for the incident report.
    #[serde(default)]
    pub incident: Option<i32>,
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
    /// None when the transport cannot tell (MeshCore adverts carry no speed); the engine then works
    /// it out from the previous report.
    pub speed_mph: Option<f64>,
    /// -1 when unknown; the unit keeps its last known battery.
    pub battery: i32,
    /// How long ago the position was recorded, for reports recovered from the radio's contact list
    /// after a restart. Zero for live reports.
    pub age_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObstacleKind {
    /// A line that cannot be crossed (fence, wall, building edge).
    Wall,
    /// A wall segment dispatchers can open and close.
    Gate,
    /// An area that cannot be entered.
    NoGo,
    /// An area that can be crossed, but only at a fraction of the usual speed.
    Slow,
}

/// Something drawn on the overlay that shapes routing. Points are overlay pixels, so obstacles
/// stay put when the georeferencing is adjusted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Obstacle {
    pub id: i32,
    pub kind: ObstacleKind,
    pub name: String,
    pub points: Vec<[f64; 2]>,
    /// Gates only: true lets traffic through.
    #[serde(default)]
    pub open: bool,
    /// Slow zones only: fraction of normal speed, in (0, 1].
    #[serde(default = "one")]
    pub factor: f64,
}

fn one() -> f64 {
    1.0
}

/// One repeater's place in a saved site.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Placement {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub node_key: String,
    pub lat: f64,
    pub lon: f64,
    pub placed: bool,
}

/// Everything that belongs to one event site: the overlay and its pinning, obstacles and where
/// the repeaters stand. Units and settings belong to the crew, not the site.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SiteData {
    pub site: Site,
    pub obstacles: Vec<Obstacle>,
    pub repeaters: Vec<Placement>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Alert {
    /// "unit" or "repeater".
    pub kind: String,
    pub id: String,
    pub name: String,
    /// "warn" or "crit".
    pub severity: String,
    pub text: String,
    pub incident: Option<i32>,
}
