package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"sync"
)

// Store persists state as one JSON file, written atomically (temp file then rename).
// Saves are queued latest-wins on a single worker, so a slow disk never reorders writes.
// It sits behind Engine's save callback so SQLite can replace it later.
type Store struct {
	dir     string
	mu      sync.Mutex
	pending *Persisted
	wake    chan struct{}
	done    chan struct{}
	quit    chan struct{}
	closed  bool
}

func NewStore(dir string) (*Store, error) {
	if err := os.MkdirAll(filepath.Join(dir, "files"), 0o755); err != nil {
		return nil, err
	}
	s := &Store{dir: dir, wake: make(chan struct{}, 1), done: make(chan struct{}), quit: make(chan struct{})}
	go s.worker()
	return s, nil
}

func (s *Store) path() string { return filepath.Join(s.dir, "state.json") }

func (s *Store) Load() (Persisted, error) {
	var p Persisted
	b, err := os.ReadFile(s.path())
	if os.IsNotExist(err) {
		return p, nil
	}
	if err != nil {
		return p, err
	}
	err = json.Unmarshal(b, &p)
	return p, err
}

// Save queues p; it returns immediately.
func (s *Store) Save(p Persisted) {
	s.mu.Lock()
	if s.closed {
		s.mu.Unlock()
		return
	}
	s.pending = &p
	s.mu.Unlock()
	select {
	case s.wake <- struct{}{}:
	default:
	}
}

func (s *Store) worker() {
	defer close(s.done)
	for {
		select {
		case <-s.wake:
			s.flush()
		case <-s.quit:
			return
		}
	}
}

func (s *Store) flush() {
	s.mu.Lock()
	p := s.pending
	s.pending = nil
	s.mu.Unlock()
	if p == nil {
		return
	}
	b, err := json.MarshalIndent(p, "", " ")
	if err != nil {
		return
	}
	tmp := s.path() + ".tmp"
	if err := os.WriteFile(tmp, b, 0o644); err != nil {
		return
	}
	_ = os.Rename(tmp, s.path())
}

// Close writes anything still queued and stops the worker.
func (s *Store) Close() {
	close(s.quit)
	<-s.done
	s.flush()
	s.mu.Lock()
	s.closed = true
	s.mu.Unlock()
}
