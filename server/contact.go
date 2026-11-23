package main

import "time"

// ContactState derives a unit's contact state from the age of its last report.
// Parked units fall back to a slow check-in, so they get long thresholds.
// A unit that has never reported is no contact.
func ContactState(s Settings, hasReport, moving bool, age time.Duration) string {
	if !hasReport {
		return CtNoContact
	}
	late, none := s.ParkedLateMin, s.ParkedNoContactMin
	if moving {
		late, none = s.MovingLateMin, s.MovingNoContactMin
	}
	m := age.Minutes()
	switch {
	case m > none:
		return CtNoContact
	case m > late:
		return CtLate
	}
	return CtNormal
}
