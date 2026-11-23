package main

import "time"

// Dispatcher-controlled status. Contact state is derived separately.
const (
	StAvailable  = "available"
	StDispatched = "dispatched"
	StOnScene    = "on_scene"
	StBusy       = "busy"
	StOffDuty    = "off_duty"
)

var validStatus = map[string]bool{StAvailable: true, StDispatched: true, StOnScene: true, StBusy: true, StOffDuty: true}

// Contact states derived from report age.
const (
	CtNormal    = "normal"
	CtLate      = "late"
	CtNoContact = "no_contact"
)

type Group struct {
	ID     string  `json:"id"`
	Name   string  `json:"name"`
	MaxMph float64 `json:"max_mph"`
	Adjust float64 `json:"adjust"` // multiplier on max speed, e.g. 0.7 for rough ground
}

type Unit struct {
	ID      string `json:"id"`
	Name    string `json:"name"`
	GroupID string `json:"group_id"`
	Status  string `json:"status"`

	HasPos   bool      `json:"has_pos"`
	Lat      float64   `json:"lat"`
	Lon      float64   `json:"lon"`
	SpeedMph float64   `json:"speed_mph"`
	Moving   bool      `json:"moving"`
	Battery  int       `json:"battery"`   // percent, -1 unknown
	LastSeen time.Time `json:"last_seen"` // wall clock, display only
	// seenMono holds the monotonic reading used for ages. Not persisted.
	seenMono time.Time

	IncidentID string `json:"incident_id,omitempty"`
}

type Repeater struct {
	ID       string    `json:"id"`
	Name     string    `json:"name"`
	Lat      float64   `json:"lat"`
	Lon      float64   `json:"lon"`
	Placed   bool      `json:"placed"`
	Status   string    `json:"status"` // ok, late, down, unknown
	LastSeen time.Time `json:"last_seen"`
	seenMono time.Time
}

type Incident struct {
	ID       int       `json:"id"`
	Label    string    `json:"label"`
	Lat      float64   `json:"lat"`
	Lon      float64   `json:"lon"`
	Opened   time.Time `json:"opened"`
	Closed   time.Time `json:"closed,omitempty"`
	Open     bool      `json:"open"`
	Units    []string  `json:"units"`
	OpenedBy string    `json:"opened_by"`
}

type LogEntry struct {
	T    time.Time `json:"t"`
	Seat string    `json:"seat"`
	Text string    `json:"text"`
}

type Anchor struct {
	PX  float64 `json:"px"`
	PY  float64 `json:"py"`
	Lat float64 `json:"lat"`
	Lon float64 `json:"lon"`
}

type Site struct {
	Name    string  `json:"name"`
	Overlay string  `json:"overlay"` // filename in data dir, "" for none
	ImgW    float64 `json:"img_w"`
	ImgH    float64 `json:"img_h"`
	A       Anchor  `json:"a"`
	B       Anchor  `json:"b"`
	Logo    string  `json:"logo"`
	Slogan  string  `json:"slogan"`
}

type Settings struct {
	ParkedLateMin      float64 `json:"parked_late_min"`
	ParkedNoContactMin float64 `json:"parked_no_contact_min"`
	MovingLateMin      float64 `json:"moving_late_min"`
	MovingNoContactMin float64 `json:"moving_no_contact_min"`
	MovingMinMph       float64 `json:"moving_min_mph"`
}

func defaultSettings() Settings {
	return Settings{15, 30, 2, 10, 2}
}

// Position is what a transport hands the engine.
type Position struct {
	UnitID   string
	Lat, Lon float64
	SpeedMph float64
	Battery  int // -1 unknown
}

// Persisted is the on-disk shape.
type Persisted struct {
	Site      Site       `json:"site"`
	Settings  Settings   `json:"settings"`
	Groups    []Group    `json:"groups"`
	Units     []Unit     `json:"units"`
	Repeaters []Repeater `json:"repeaters"`
	Incidents []Incident `json:"incidents"`
	Log       []LogEntry `json:"log"`
	NextInc   int        `json:"next_inc"`
}
