package main

import (
	"errors"
	"math"
	"os"
	"sync"
	"testing"
	"time"
)

func TestContactState(t *testing.T) {
	s := defaultSettings()
	m := func(x float64) time.Duration { return time.Duration(x * float64(time.Minute)) }
	cases := []struct {
		name        string
		has, moving bool
		age         time.Duration
		want        string
	}{
		{"never reported", false, false, 0, CtNoContact},
		{"parked fresh", true, false, m(5), CtNormal},
		{"parked at 15 exactly is still normal", true, false, m(15), CtNormal},
		{"parked missed one check-in", true, false, m(16), CtLate},
		{"parked at 30 exactly is late", true, false, m(30), CtLate},
		{"parked missed two check-ins", true, false, m(30.1), CtNoContact},
		{"moving fresh", true, true, m(1), CtNormal},
		{"moving late", true, true, m(3), CtLate},
		{"moving gone", true, true, m(11), CtNoContact},
	}
	for _, c := range cases {
		if got := ContactState(s, c.has, c.moving, c.age); got != c.want {
			t.Errorf("%s: got %s want %s", c.name, got, c.want)
		}
	}
}

func newTestEngine() *Engine {
	e := NewEngine(Persisted{}, nil)
	e.p.Site.A = Anchor{0, 0, 39.002, -104.003}
	e.p.Site.B = Anchor{1000, 700, 38.998, -103.997}
	e.p.Groups = []Group{{"utv", "UTV", 15, 0.8}, {"slow", "Cart", 5, 1}}
	for _, u := range []struct{ id, g string }{{"a", "utv"}, {"b", "utv"}, {"c", "slow"}} {
		_ = e.AddUnit(u.id, "Unit "+u.id, u.g)
	}
	return e
}

func TestNearestOrderingAndExclusions(t *testing.T) {
	e := newTestEngine()
	e.ReportPosition(Position{"a", 39.0000, -104.0000, 0, 50})
	e.ReportPosition(Position{"b", 39.0010, -104.0000, 0, 50})
	// c has never reported a position: excluded.
	got := e.Nearest(39.0001, -104.0000)
	if len(got) != 2 || got[0].UnitID != "a" || got[1].UnitID != "b" {
		t.Fatalf("order wrong: %+v", got)
	}
	_ = e.SetStatus("a", StBusy, "t")
	got = e.Nearest(39.0001, -104.0000)
	if len(got) != 1 || got[0].UnitID != "b" {
		t.Fatalf("busy unit should be excluded: %+v", got)
	}
}

func TestNearestPutsNoContactLast(t *testing.T) {
	e := newTestEngine()
	e.ReportPosition(Position{"a", 39.0000, -104.0000, 0, 50}) // closest
	e.ReportPosition(Position{"b", 39.0010, -104.0000, 0, 50})
	e.mu.Lock()
	e.unit("a").seenMono = time.Now().Add(-time.Hour) // parked, >30 min old
	e.mu.Unlock()
	got := e.Nearest(39.0000, -104.0000)
	if got[0].UnitID != "b" || got[1].Contact != CtNoContact {
		t.Fatalf("no-contact unit should sort after live ones: %+v", got)
	}
}

func TestEtaUsesGroupSpeed(t *testing.T) {
	g := Group{MaxMph: 10, Adjust: 1}
	d := 10 * mpsPerMph * 60 // distance covered in 60 s at 10 mph
	if got := EtaSeconds(d, g); math.Abs(got-60) > 0.01 {
		t.Fatalf("eta %v", got)
	}
	g.Adjust = 0.5
	if got := EtaSeconds(d, g); math.Abs(got-120) > 0.01 {
		t.Fatalf("adjusted eta %v", got)
	}
}

func TestDispatchFlowAndDoubleDispatch(t *testing.T) {
	e := newTestEngine()
	in := e.OpenIncident(39, -104, "", "seat1")
	if err := e.Dispatch(in.ID, []string{"a"}, "seat1"); err != nil {
		t.Fatal(err)
	}
	in2 := e.OpenIncident(39.001, -104, "", "seat2")
	err := e.Dispatch(in2.ID, []string{"a"}, "seat2")
	if !errors.Is(err, ErrConflict) {
		t.Fatalf("second dispatch should conflict, got %v", err)
	}
	// all-or-nothing: b is free but a is taken, so b must not be dispatched
	err = e.Dispatch(in2.ID, []string{"b", "a"}, "seat2")
	if !errors.Is(err, ErrConflict) || e.unit("b").Status != StAvailable {
		t.Fatalf("dispatch must be all-or-nothing, err=%v b=%s", err, e.unit("b").Status)
	}
	if err := e.Arrive(in.ID, "a", "seat1"); err != nil || e.unit("a").Status != StOnScene {
		t.Fatalf("arrive: %v %s", err, e.unit("a").Status)
	}
	if err := e.CloseIncident(in.ID, "seat1"); err != nil || e.unit("a").Status != StAvailable {
		t.Fatalf("close should free unit: %v %s", err, e.unit("a").Status)
	}
}

func TestConcurrentDispatchOnlyOneWins(t *testing.T) {
	e := newTestEngine()
	in := e.OpenIncident(39, -104, "", "x")
	var wg sync.WaitGroup
	var mu sync.Mutex
	wins := 0
	for i := 0; i < 20; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if e.Dispatch(in.ID, []string{"a"}, "s") == nil {
				mu.Lock()
				wins++
				mu.Unlock()
			}
		}()
	}
	wg.Wait()
	if wins != 1 {
		t.Fatalf("expected exactly one winner, got %d", wins)
	}
}

func TestManualStatusReleasesFromIncident(t *testing.T) {
	e := newTestEngine()
	in := e.OpenIncident(39, -104, "", "x")
	_ = e.Dispatch(in.ID, []string{"a"}, "x")
	_ = e.SetStatus("a", StOffDuty, "x")
	if e.unit("a").IncidentID != "" || len(e.incident(in.ID).Units) != 0 {
		t.Fatal("off duty unit should be detached from the incident")
	}
	// closing must not bring an off-duty unit back
	_ = e.CloseIncident(in.ID, "x")
	if e.unit("a").Status != StOffDuty {
		t.Fatalf("status %s", e.unit("a").Status)
	}
}

func TestBadStatusRejected(t *testing.T) {
	e := newTestEngine()
	if err := e.SetStatus("a", "nonsense", "x"); !errors.Is(err, ErrBad) {
		t.Fatal(err)
	}
	if err := e.SetStatus("zzz", StBusy, "x"); !errors.Is(err, ErrNotFound) {
		t.Fatal(err)
	}
}

func TestMovingFlagFromSpeed(t *testing.T) {
	e := newTestEngine()
	e.ReportPosition(Position{"a", 39, -104, 8, 50})
	if !e.unit("a").Moving {
		t.Fatal("8 mph should be moving")
	}
	e.ReportPosition(Position{"a", 39, -104, 0.3, 50})
	if e.unit("a").Moving {
		t.Fatal("0.3 mph should be parked")
	}
}

func TestGeorefRoundTrip(t *testing.T) {
	e := newTestEngine()
	s := e.p.Site
	px, py, ok := s.ToPixel(39.000, -104.000)
	if !ok {
		t.Fatal("anchors should be valid")
	}
	lat, lon, _ := s.ToLatLon(px, py)
	if math.Abs(lat-39.000) > 1e-9 || math.Abs(lon+104.000) > 1e-9 {
		t.Fatalf("round trip %v %v", lat, lon)
	}
	if _, _, ok := (Site{}).ToPixel(1, 1); ok {
		t.Fatal("empty anchors must report not ok")
	}
}

func TestPersistenceRoundTrip(t *testing.T) {
	dir := t.TempDir()
	st, _ := NewStore(dir)
	e := NewEngine(Persisted{}, st.Save)
	_ = e.AddUnit("x1", "UTV X", "utv")
	e.ReportPosition(Position{"x1", 39, -104, 0, 80})
	in := e.OpenIncident(39, -104, "Test", "s")
	_ = e.Dispatch(in.ID, []string{"x1"}, "s")
	st.Close()

	st2, _ := NewStore(dir)
	defer st2.Close()
	p, err := st2.Load()
	if err != nil {
		t.Fatal(err)
	}
	e2 := NewEngine(p, nil)
	u := e2.unit("x1")
	if u == nil || u.Status != StDispatched || !u.HasPos {
		t.Fatalf("unit not restored: %+v", u)
	}
	// ages are monotonic-only, so a restart must show no contact until a new report
	if got := e2.Snapshot().Units[0].Contact; got != CtNoContact {
		t.Fatalf("after restart contact should be no_contact, got %s", got)
	}
	if _, err := os.Stat(dir + "/state.json"); err != nil {
		t.Fatal(err)
	}
}
