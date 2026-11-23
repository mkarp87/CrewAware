package main

import (
	"embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

//go:embed web
var webFS embed.FS

type Server struct {
	e   *Engine
	dir string
}

func NewServer(e *Engine, dataDir string) http.Handler {
	s := &Server{e: e, dir: dataDir}
	mux := http.NewServeMux()
	sub, _ := fs.Sub(webFS, "web")
	mux.Handle("GET /", http.FileServer(http.FS(sub)))
	mux.Handle("GET /files/", http.StripPrefix("/files/", http.FileServer(http.Dir(filepath.Join(dataDir, "files")))))
	mux.HandleFunc("GET /api/state", s.state)
	mux.HandleFunc("GET /api/events", s.events)
	mux.HandleFunc("GET /api/nearest", s.nearest)
	mux.HandleFunc("POST /api/units", s.addUnit)
	mux.HandleFunc("POST /api/units/{id}/status", s.setStatus)
	mux.HandleFunc("POST /api/units/{id}/group", s.assignGroup)
	mux.HandleFunc("POST /api/position", s.position)
	mux.HandleFunc("POST /api/incidents", s.openIncident)
	mux.HandleFunc("POST /api/incidents/{id}/dispatch", s.dispatch)
	mux.HandleFunc("POST /api/incidents/{id}/arrive", s.arrive)
	mux.HandleFunc("POST /api/incidents/{id}/close", s.closeIncident)
	mux.HandleFunc("POST /api/repeaters", s.setRepeater)
	mux.HandleFunc("POST /api/groups", s.setGroup)
	mux.HandleFunc("POST /api/site", s.setSite)
	mux.HandleFunc("POST /api/site/overlay", s.upload("overlay"))
	mux.HandleFunc("POST /api/site/logo", s.upload("logo"))
	return mux
}

func seat(r *http.Request) string {
	v := strings.TrimSpace(r.Header.Get("X-Seat"))
	if v == "" {
		return "unknown"
	}
	if len(v) > 40 {
		v = v[:40]
	}
	return v
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	_ = json.NewEncoder(w).Encode(v)
}

func fail(w http.ResponseWriter, err error) {
	code := http.StatusInternalServerError
	switch {
	case errors.Is(err, ErrNotFound):
		code = http.StatusNotFound
	case errors.Is(err, ErrConflict):
		code = http.StatusConflict
	case errors.Is(err, ErrBad):
		code = http.StatusBadRequest
	}
	writeJSON(w, code, map[string]string{"error": err.Error()})
}

func decode(r *http.Request, v any) error {
	r.Body = http.MaxBytesReader(nil, r.Body, 1<<20)
	if err := json.NewDecoder(r.Body).Decode(v); err != nil {
		return fmt.Errorf("%w: %v", ErrBad, err)
	}
	return nil
}

func ok(w http.ResponseWriter) { writeJSON(w, 200, map[string]bool{"ok": true}) }

func (s *Server) state(w http.ResponseWriter, r *http.Request) { writeJSON(w, 200, s.e.Snapshot()) }

func (s *Server) events(w http.ResponseWriter, r *http.Request) {
	fl, okf := w.(http.Flusher)
	if !okf {
		http.Error(w, "streaming unsupported", 500)
		return
	}
	w.Header().Set("Content-Type", "text/event-stream")
	w.Header().Set("Cache-Control", "no-cache")
	ch, cancel := s.e.Subscribe()
	defer cancel()
	send := func() bool {
		b, _ := json.Marshal(s.e.Snapshot())
		_, err := fmt.Fprintf(w, "data: %s\n\n", b)
		fl.Flush()
		return err == nil
	}
	if !send() {
		return
	}
	tick := time.NewTicker(2 * time.Second) // keeps ages fresh without a change
	defer tick.Stop()
	for {
		select {
		case <-r.Context().Done():
			return
		case <-ch:
		case <-tick.C:
		}
		if !send() {
			return
		}
	}
}

func (s *Server) nearest(w http.ResponseWriter, r *http.Request) {
	lat, e1 := strconv.ParseFloat(r.URL.Query().Get("lat"), 64)
	lon, e2 := strconv.ParseFloat(r.URL.Query().Get("lon"), 64)
	if e1 != nil || e2 != nil {
		fail(w, fmt.Errorf("%w: lat and lon", ErrBad))
		return
	}
	c := s.e.Nearest(lat, lon)
	if c == nil {
		c = []Candidate{}
	}
	writeJSON(w, 200, c)
}

func (s *Server) addUnit(w http.ResponseWriter, r *http.Request) {
	var b struct{ ID, Name, Group string }
	if err := decode(r, &b); err != nil {
		fail(w, err)
		return
	}
	if err := s.e.AddUnit(b.ID, b.Name, b.Group); err != nil {
		fail(w, err)
		return
	}
	ok(w)
}

func (s *Server) setStatus(w http.ResponseWriter, r *http.Request) {
	var b struct{ Status string }
	if err := decode(r, &b); err != nil {
		fail(w, err)
		return
	}
	if err := s.e.SetStatus(r.PathValue("id"), b.Status, seat(r)); err != nil {
		fail(w, err)
		return
	}
	ok(w)
}

func (s *Server) assignGroup(w http.ResponseWriter, r *http.Request) {
	var b struct{ Group string }
	if err := decode(r, &b); err != nil {
		fail(w, err)
		return
	}
	if err := s.e.AssignGroup(r.PathValue("id"), b.Group); err != nil {
		fail(w, err)
		return
	}
	ok(w)
}

// position lets tools and tests inject a report. A real gateway calls Engine directly.
func (s *Server) position(w http.ResponseWriter, r *http.Request) {
	var b struct {
		Unit     string  `json:"unit"`
		Lat      float64 `json:"lat"`
		Lon      float64 `json:"lon"`
		SpeedMph float64 `json:"speed_mph"`
		Battery  *int    `json:"battery"`
	}
	if err := decode(r, &b); err != nil {
		fail(w, err)
		return
	}
	bat := -1
	if b.Battery != nil {
		bat = *b.Battery
	}
	s.e.ReportPosition(Position{b.Unit, b.Lat, b.Lon, b.SpeedMph, bat})
	ok(w)
}

func (s *Server) openIncident(w http.ResponseWriter, r *http.Request) {
	var b struct {
		Lat, Lon float64
		Label    string
	}
	if err := decode(r, &b); err != nil {
		fail(w, err)
		return
	}
	writeJSON(w, 200, s.e.OpenIncident(b.Lat, b.Lon, b.Label, seat(r)))
}

func incID(r *http.Request) (int, error) {
	n, err := strconv.Atoi(r.PathValue("id"))
	if err != nil {
		return 0, fmt.Errorf("%w: incident id", ErrBad)
	}
	return n, nil
}

func (s *Server) dispatch(w http.ResponseWriter, r *http.Request) {
	id, err := incID(r)
	var b struct {
		Units []string `json:"units"`
	}
	if err == nil {
		err = decode(r, &b)
	}
	if err == nil {
		err = s.e.Dispatch(id, b.Units, seat(r))
	}
	if err != nil {
		fail(w, err)
		return
	}
	ok(w)
}

func (s *Server) arrive(w http.ResponseWriter, r *http.Request) {
	id, err := incID(r)
	var b struct{ Unit string }
	if err == nil {
		err = decode(r, &b)
	}
	if err == nil {
		err = s.e.Arrive(id, b.Unit, seat(r))
	}
	if err != nil {
		fail(w, err)
		return
	}
	ok(w)
}

func (s *Server) closeIncident(w http.ResponseWriter, r *http.Request) {
	id, err := incID(r)
	if err == nil {
		err = s.e.CloseIncident(id, seat(r))
	}
	if err != nil {
		fail(w, err)
		return
	}
	ok(w)
}

func (s *Server) setRepeater(w http.ResponseWriter, r *http.Request) {
	var b Repeater
	if err := decode(r, &b); err != nil {
		fail(w, err)
		return
	}
	if err := s.e.SetRepeater(b); err != nil {
		fail(w, err)
		return
	}
	ok(w)
}

func (s *Server) setGroup(w http.ResponseWriter, r *http.Request) {
	var b Group
	if err := decode(r, &b); err != nil {
		fail(w, err)
		return
	}
	if err := s.e.SetGroup(b); err != nil {
		fail(w, err)
		return
	}
	ok(w)
}

func (s *Server) setSite(w http.ResponseWriter, r *http.Request) {
	var b struct {
		Name   string `json:"name"`
		A, B   Anchor
		Slogan string `json:"slogan"`
	}
	if err := decode(r, &b); err != nil {
		fail(w, err)
		return
	}
	if len(b.Slogan) > 60 {
		fail(w, fmt.Errorf("%w: slogan over 60 characters", ErrBad))
		return
	}
	s.e.SetSite(func(si *Site) {
		if b.Name != "" {
			si.Name = b.Name
		}
		si.A, si.B, si.Slogan = b.A, b.B, b.Slogan
	})
	ok(w)
}

// upload stores an overlay image or a logo. Only raster formats, sniffed from content.
func (s *Server) upload(kind string) http.HandlerFunc {
	return func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, 25<<20))
		if err != nil {
			fail(w, fmt.Errorf("%w: file too large (25 MB max)", ErrBad))
			return
		}
		ext := map[string]string{"image/png": ".png", "image/jpeg": ".jpg", "image/gif": ".gif", "image/webp": ".webp"}[http.DetectContentType(body)]
		if ext == "" {
			fail(w, fmt.Errorf("%w: use a PNG, JPEG, GIF or WebP image", ErrBad))
			return
		}
		name := fmt.Sprintf("%s-%d%s", kind, time.Now().UnixNano(), ext)
		if err := os.WriteFile(filepath.Join(s.dir, "files", name), body, 0o644); err != nil {
			fail(w, err)
			return
		}
		wpx, _ := strconv.ParseFloat(r.URL.Query().Get("w"), 64)
		hpx, _ := strconv.ParseFloat(r.URL.Query().Get("h"), 64)
		s.e.SetSite(func(si *Site) {
			if kind == "logo" {
				si.Logo = name
				return
			}
			si.Overlay = name
			if wpx > 0 && hpx > 0 {
				si.ImgW, si.ImgH = wpx, hpx
			}
		})
		writeJSON(w, 200, map[string]string{"file": name})
	}
}
