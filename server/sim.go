package main

import (
	"context"
	"math"
	"math/rand"
	"time"
)

// Sim is a fake radio transport: moving units, parked units on the slow cadence,
// one unit that goes silent, and one that never reports.
type Sim struct {
	ParkedEvery time.Duration // how often parked units check in
	Tick        time.Duration
}

type simUnit struct {
	id         string
	cx, cy     float64 // path centre in lat/lon
	rx, ry     float64
	phase      float64
	rate       float64 // radians per second
	parked     bool
	silentAt   time.Duration // 0 = never goes silent
	mute       bool          // never reports
	lastParked time.Time
}

// SeedDemo adds demo units, a group and repeaters if the engine is empty.
func SeedDemo(e *Engine) {
	e.mu.Lock()
	defer e.mu.Unlock()
	if len(e.p.Units) > 0 {
		return
	}
	e.p.Site.Name = "Demo site"
	e.p.Site.ImgW, e.p.Site.ImgH = 1000, 700
	e.p.Site.A = Anchor{0, 0, 39.0020, -104.0030}
	e.p.Site.B = Anchor{1000, 700, 38.9980, -103.9970}
	e.p.Groups = []Group{{"utv", "UTV", 15, 0.8}, {"golf", "Golf cart", 8, 0.9}}
	names := []struct{ id, name, g string }{
		{"u1", "UTV 1", "utv"}, {"u2", "UTV 2", "utv"}, {"u3", "UTV 3", "utv"},
		{"u4", "UTV 4", "utv"}, {"u5", "UTV 5", "utv"}, {"c1", "Cart 1", "golf"}, {"u6", "UTV 6", "utv"},
	}
	for _, n := range names {
		e.p.Units = append(e.p.Units, Unit{ID: n.id, Name: n.name, GroupID: n.g, Status: StAvailable, Battery: -1})
	}
	e.p.Repeaters = []Repeater{
		{ID: "r1", Name: "Repeater 1", Lat: 39.0012, Lon: -104.0020, Placed: true},
		{ID: "r2", Name: "Repeater 2", Lat: 39.0008, Lon: -103.9975, Placed: true},
		{ID: "r3", Name: "Repeater 3", Lat: 38.9985, Lon: -104.0000, Placed: true},
	}
	e.changed()
}

func (s Sim) Run(ctx context.Context, e *Engine) {
	if s.Tick == 0 {
		s.Tick = 3 * time.Second
	}
	if s.ParkedEvery == 0 {
		s.ParkedEvery = 15 * time.Minute
	}
	units := []*simUnit{
		{id: "u1", cx: 39.0000, cy: -104.0000, rx: 0.0012, ry: 0.0020, rate: 0.05},
		{id: "u2", cx: 39.0005, cy: -103.9990, rx: 0.0008, ry: 0.0012, rate: 0.07, phase: 2},
		{id: "u3", cx: 38.9990, cy: -104.0010, rx: 0.0006, ry: 0.0010, rate: 0.04, phase: 4},
		{id: "u4", cx: 39.0010, cy: -104.0015, parked: true},
		{id: "u5", cx: 38.9988, cy: -103.9985, parked: true, silentAt: 45 * time.Second},
		{id: "c1", cx: 39.0003, cy: -103.9985, rx: 0.0004, ry: 0.0006, rate: 0.06, phase: 1},
		{id: "u6", mute: true},
	}
	start := time.Now()
	t := time.NewTicker(s.Tick)
	defer t.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case now := <-t.C:
			el := now.Sub(start)
			for _, u := range units {
				if u.mute || (u.silentAt > 0 && el > u.silentAt) {
					continue
				}
				if u.parked {
					if u.lastParked.IsZero() || now.Sub(u.lastParked) >= s.ParkedEvery {
						u.lastParked = now
						e.ReportPosition(Position{u.id, u.cx, u.cy, 0, 70 + rand.Intn(20)})
					}
					continue
				}
				a := u.phase + u.rate*el.Seconds()
				lat := u.cx + u.rx*math.Sin(a)
				lon := u.cy + u.ry*math.Cos(a)
				// speed from path derivative, in mph
				vx := u.rx * u.rate * math.Cos(a) * 111320
				vy := u.ry * u.rate * math.Sin(a) * 111320 * math.Cos(lat*math.Pi/180)
				mph := math.Hypot(vx, vy) / mpsPerMph
				e.ReportPosition(Position{u.id, lat, lon, mph, 60 + rand.Intn(30)})
			}
			for _, id := range []string{"r1", "r2"} { // r3 stays silent
				e.ReportRepeater(id)
			}
		}
	}
}
