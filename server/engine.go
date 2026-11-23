package main

import (
	"errors"
	"fmt"
	"sort"
	"sync"
	"time"
)

var (
	ErrNotFound = errors.New("not found")
	ErrConflict = errors.New("conflict")
	ErrBad      = errors.New("bad request")
)

type Engine struct {
	mu   sync.Mutex
	p    Persisted
	subs map[chan struct{}]struct{}
	save func(Persisted) // called with a copy after changes; may be nil
	now  func() time.Time
}

func NewEngine(p Persisted, save func(Persisted)) *Engine {
	e := &Engine{p: p, subs: map[chan struct{}]struct{}{}, save: save, now: time.Now}
	if e.p.Settings == (Settings{}) {
		e.p.Settings = defaultSettings()
	}
	if len(e.p.Groups) == 0 {
		e.p.Groups = []Group{{ID: "utv", Name: "UTV", MaxMph: 15, Adjust: 0.8}}
	}
	if e.p.NextInc == 0 {
		e.p.NextInc = 1
	}
	if e.p.Site.Name == "" {
		e.p.Site.Name = "Unnamed site"
	}
	for i := range e.p.Units {
		if e.p.Units[i].Status == "" {
			e.p.Units[i].Status = StAvailable
		}
	}
	return e
}

// changed must be called with mu held.
func (e *Engine) changed() {
	if e.save != nil {
		cp := e.p
		cp.Units = append([]Unit(nil), e.p.Units...)
		cp.Repeaters = append([]Repeater(nil), e.p.Repeaters...)
		cp.Incidents = append([]Incident(nil), e.p.Incidents...)
		for i := range cp.Incidents {
			cp.Incidents[i].Units = append([]string(nil), cp.Incidents[i].Units...)
		}
		cp.Log = append([]LogEntry(nil), e.p.Log...)
		cp.Groups = append([]Group(nil), e.p.Groups...)
		e.save(cp)
	}
	for ch := range e.subs {
		select {
		case ch <- struct{}{}:
		default:
		}
	}
}

func (e *Engine) logf(seat, format string, a ...any) {
	e.p.Log = append(e.p.Log, LogEntry{T: e.now(), Seat: seat, Text: fmt.Sprintf(format, a...)})
	if len(e.p.Log) > 2000 {
		e.p.Log = e.p.Log[len(e.p.Log)-2000:]
	}
}

func (e *Engine) Subscribe() (chan struct{}, func()) {
	ch := make(chan struct{}, 1)
	e.mu.Lock()
	e.subs[ch] = struct{}{}
	e.mu.Unlock()
	return ch, func() { e.mu.Lock(); delete(e.subs, ch); e.mu.Unlock() }
}

func (e *Engine) unit(id string) *Unit {
	for i := range e.p.Units {
		if e.p.Units[i].ID == id {
			return &e.p.Units[i]
		}
	}
	return nil
}

func (e *Engine) group(id string) Group {
	for _, g := range e.p.Groups {
		if g.ID == id {
			return g
		}
	}
	return Group{ID: "", Name: "none", MaxMph: 10, Adjust: 1}
}

// ReportPosition records a position report. Unknown unit IDs are ignored.
func (e *Engine) ReportPosition(p Position) {
	e.mu.Lock()
	defer e.mu.Unlock()
	u := e.unit(p.UnitID)
	if u == nil {
		return
	}
	u.HasPos, u.Lat, u.Lon, u.SpeedMph = true, p.Lat, p.Lon, p.SpeedMph
	u.Moving = p.SpeedMph >= e.p.Settings.MovingMinMph
	u.Battery = p.Battery
	u.LastSeen = e.now()
	u.seenMono = time.Now()
	e.changed()
}

// ReportRepeater records a repeater check-in.
func (e *Engine) ReportRepeater(id string) {
	e.mu.Lock()
	defer e.mu.Unlock()
	for i := range e.p.Repeaters {
		if e.p.Repeaters[i].ID == id {
			e.p.Repeaters[i].LastSeen = e.now()
			e.p.Repeaters[i].seenMono = time.Now()
			e.changed()
			return
		}
	}
}

// ---- snapshot ----

type UnitView struct {
	Unit
	Contact   string  `json:"contact"`
	AgeSec    float64 `json:"age_sec"`
	GroupName string  `json:"group_name"`
	PX        float64 `json:"px"`
	PY        float64 `json:"py"`
	HasPixel  bool    `json:"has_pixel"`
}

type RepeaterView struct {
	Repeater
	Contact  string  `json:"contact"`
	AgeSec   float64 `json:"age_sec"`
	PX       float64 `json:"px"`
	PY       float64 `json:"py"`
	HasPixel bool    `json:"has_pixel"`
}

type Snapshot struct {
	Now       time.Time      `json:"now"`
	Site      Site           `json:"site"`
	Settings  Settings       `json:"settings"`
	Groups    []Group        `json:"groups"`
	Units     []UnitView     `json:"units"`
	Repeaters []RepeaterView `json:"repeaters"`
	Incidents []Incident     `json:"incidents"`
	Log       []LogEntry     `json:"log"`
}

func (e *Engine) repeaterContact(r Repeater) (string, float64) {
	if r.seenMono.IsZero() {
		return CtNoContact, 0
	}
	age := time.Since(r.seenMono)
	// Repeaters check in on the slow cadence.
	return ContactState(e.p.Settings, true, false, age), age.Seconds()
}

func (e *Engine) Snapshot() Snapshot {
	e.mu.Lock()
	defer e.mu.Unlock()
	s := Snapshot{Now: e.now(), Site: e.p.Site, Settings: e.p.Settings, Groups: e.p.Groups, Incidents: []Incident{}, Log: []LogEntry{}}
	for _, u := range e.p.Units {
		v := UnitView{Unit: u, GroupName: e.group(u.GroupID).Name}
		var age time.Duration
		if !u.seenMono.IsZero() {
			age = time.Since(u.seenMono)
		}
		v.Contact = ContactState(e.p.Settings, !u.seenMono.IsZero(), u.Moving, age)
		v.AgeSec = age.Seconds()
		if u.HasPos {
			v.PX, v.PY, v.HasPixel = e.p.Site.ToPixel(u.Lat, u.Lon)
		}
		s.Units = append(s.Units, v)
	}
	for _, r := range e.p.Repeaters {
		v := RepeaterView{Repeater: r}
		v.Contact, v.AgeSec = e.repeaterContact(r)
		if r.Placed {
			v.PX, v.PY, v.HasPixel = e.p.Site.ToPixel(r.Lat, r.Lon)
		}
		s.Repeaters = append(s.Repeaters, v)
	}
	for _, in := range e.p.Incidents {
		if in.Open {
			s.Incidents = append(s.Incidents, in)
		}
	}
	n := len(e.p.Log)
	if n > 100 {
		n = 100
	}
	for i := len(e.p.Log) - 1; i >= len(e.p.Log)-n; i-- {
		s.Log = append(s.Log, e.p.Log[i])
	}
	return s
}

// ---- nearest ----

type Candidate struct {
	UnitID  string  `json:"unit_id"`
	Name    string  `json:"name"`
	Group   string  `json:"group"`
	DistM   float64 `json:"dist_m"`
	DistFt  float64 `json:"dist_ft"`
	EtaSec  float64 `json:"eta_sec"`
	Contact string  `json:"contact"`
	AgeSec  float64 `json:"age_sec"`
}

// Nearest returns available units ranked by straight-line distance.
// Units with no contact are included but sorted after the others, so
// command sees them without being steered toward a unit that may be gone.
func (e *Engine) Nearest(lat, lon float64) []Candidate {
	e.mu.Lock()
	defer e.mu.Unlock()
	var out []Candidate
	for _, u := range e.p.Units {
		if u.Status != StAvailable || !u.HasPos {
			continue
		}
		var age time.Duration
		if !u.seenMono.IsZero() {
			age = time.Since(u.seenMono)
		}
		ct := ContactState(e.p.Settings, !u.seenMono.IsZero(), u.Moving, age)
		d := haversineM(lat, lon, u.Lat, u.Lon)
		g := e.group(u.GroupID)
		out = append(out, Candidate{u.ID, u.Name, g.Name, d, d * feetPerM, EtaSeconds(d, g), ct, age.Seconds()})
	}
	sort.SliceStable(out, func(i, j int) bool {
		gi, gj := out[i].Contact == CtNoContact, out[j].Contact == CtNoContact
		if gi != gj {
			return !gi
		}
		return out[i].DistM < out[j].DistM
	})
	return out
}

// ---- dispatcher actions ----

func (e *Engine) SetStatus(unitID, status, seat string) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	if !validStatus[status] {
		return fmt.Errorf("%w: status %q", ErrBad, status)
	}
	u := e.unit(unitID)
	if u == nil {
		return ErrNotFound
	}
	old := u.Status
	if old == status {
		return nil
	}
	if (old == StDispatched || old == StOnScene) && u.IncidentID != "" {
		// Manual override releases the unit from its incident.
		e.detach(u)
	}
	u.Status = status
	e.logf(seat, "%s: %s -> %s", u.Name, old, status)
	e.changed()
	return nil
}

func (e *Engine) detach(u *Unit) {
	for i := range e.p.Incidents {
		in := &e.p.Incidents[i]
		var keep []string
		for _, id := range in.Units {
			if id != u.ID {
				keep = append(keep, id)
			}
		}
		in.Units = keep
	}
	u.IncidentID = ""
}

func (e *Engine) OpenIncident(lat, lon float64, label, seat string) Incident {
	e.mu.Lock()
	defer e.mu.Unlock()
	in := Incident{ID: e.p.NextInc, Label: label, Lat: lat, Lon: lon, Opened: e.now(), Open: true, OpenedBy: seat, Units: []string{}}
	if in.Label == "" {
		in.Label = fmt.Sprintf("Incident %d", in.ID)
	}
	e.p.NextInc++
	e.p.Incidents = append(e.p.Incidents, in)
	e.logf(seat, "Opened %s", in.Label)
	e.changed()
	return in
}

func (e *Engine) incident(id int) *Incident {
	for i := range e.p.Incidents {
		if e.p.Incidents[i].ID == id {
			return &e.p.Incidents[i]
		}
	}
	return nil
}

// Dispatch assigns units to an open incident. It is all-or-nothing: if any unit
// is no longer Available (for example another seat just dispatched it), nothing changes
// and ErrConflict is returned with the offending unit named.
func (e *Engine) Dispatch(incID int, unitIDs []string, seat string) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	in := e.incident(incID)
	if in == nil || !in.Open {
		return ErrNotFound
	}
	if len(unitIDs) == 0 {
		return fmt.Errorf("%w: no units", ErrBad)
	}
	us := make([]*Unit, 0, len(unitIDs))
	for _, id := range unitIDs {
		u := e.unit(id)
		if u == nil {
			return ErrNotFound
		}
		if u.Status != StAvailable {
			return fmt.Errorf("%w: %s is already %s", ErrConflict, u.Name, u.Status)
		}
		us = append(us, u)
	}
	for _, u := range us {
		u.Status = StDispatched
		u.IncidentID = fmt.Sprint(in.ID)
		in.Units = append(in.Units, u.ID)
		e.logf(seat, "Dispatched %s to %s", u.Name, in.Label)
	}
	e.changed()
	return nil
}

func (e *Engine) Arrive(incID int, unitID, seat string) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	u := e.unit(unitID)
	in := e.incident(incID)
	if u == nil || in == nil {
		return ErrNotFound
	}
	if u.Status != StDispatched || u.IncidentID != fmt.Sprint(in.ID) {
		return fmt.Errorf("%w: %s is not dispatched to %s", ErrConflict, u.Name, in.Label)
	}
	u.Status = StOnScene
	e.logf(seat, "%s on scene at %s", u.Name, in.Label)
	e.changed()
	return nil
}

// CloseIncident returns its units to Available, except units a dispatcher set to Busy or Off duty.
func (e *Engine) CloseIncident(incID int, seat string) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	in := e.incident(incID)
	if in == nil || !in.Open {
		return ErrNotFound
	}
	for _, id := range in.Units {
		if u := e.unit(id); u != nil && (u.Status == StDispatched || u.Status == StOnScene) {
			u.Status = StAvailable
			u.IncidentID = ""
		}
	}
	in.Open, in.Closed = false, e.now()
	e.logf(seat, "Closed %s", in.Label)
	e.changed()
	return nil
}

// ---- configuration ----

func (e *Engine) AddUnit(id, name, group string) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	if id == "" || e.unit(id) != nil {
		return fmt.Errorf("%w: unit id empty or in use", ErrBad)
	}
	if name == "" {
		name = id
	}
	e.p.Units = append(e.p.Units, Unit{ID: id, Name: name, GroupID: group, Status: StAvailable, Battery: -1})
	e.changed()
	return nil
}

func (e *Engine) SetRepeater(r Repeater) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	if r.ID == "" {
		return fmt.Errorf("%w: repeater id", ErrBad)
	}
	for i := range e.p.Repeaters {
		if e.p.Repeaters[i].ID == r.ID {
			old := e.p.Repeaters[i]
			r.LastSeen, r.seenMono = old.LastSeen, old.seenMono
			e.p.Repeaters[i] = r
			e.changed()
			return nil
		}
	}
	if r.Name == "" {
		r.Name = r.ID
	}
	e.p.Repeaters = append(e.p.Repeaters, r)
	e.changed()
	return nil
}

func (e *Engine) SetSite(f func(*Site)) {
	e.mu.Lock()
	defer e.mu.Unlock()
	f(&e.p.Site)
	e.changed()
}

func (e *Engine) SetGroup(g Group) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	if g.ID == "" || g.MaxMph <= 0 || g.Adjust <= 0 || g.Adjust > 1.5 {
		return fmt.Errorf("%w: group needs id, max_mph > 0, adjust in (0,1.5]", ErrBad)
	}
	for i := range e.p.Groups {
		if e.p.Groups[i].ID == g.ID {
			e.p.Groups[i] = g
			e.changed()
			return nil
		}
	}
	e.p.Groups = append(e.p.Groups, g)
	e.changed()
	return nil
}

func (e *Engine) AssignGroup(unitID, groupID string) error {
	e.mu.Lock()
	defer e.mu.Unlock()
	u := e.unit(unitID)
	if u == nil {
		return ErrNotFound
	}
	u.GroupID = groupID
	e.changed()
	return nil
}
