package main

import "math"

const (
	earthM    = 6371000.0
	feetPerM  = 3.280839895
	mpsPerMph = 0.44704
)

func haversineM(lat1, lon1, lat2, lon2 float64) float64 {
	r := math.Pi / 180
	dLat, dLon := (lat2-lat1)*r, (lon2-lon1)*r
	a := math.Sin(dLat/2)*math.Sin(dLat/2) + math.Cos(lat1*r)*math.Cos(lat2*r)*math.Sin(dLon/2)*math.Sin(dLon/2)
	return 2 * earthM * math.Asin(math.Min(1, math.Sqrt(a)))
}

// Georef maps lat/lon to overlay pixels using two anchors (no rotation).
// Returns ok=false if the anchors are degenerate.
func (s Site) ToPixel(lat, lon float64) (px, py float64, ok bool) {
	dLon := s.B.Lon - s.A.Lon
	dLat := s.B.Lat - s.A.Lat
	if dLon == 0 || dLat == 0 || s.B.PX == s.A.PX || s.B.PY == s.A.PY {
		return 0, 0, false
	}
	px = s.A.PX + (lon-s.A.Lon)/dLon*(s.B.PX-s.A.PX)
	py = s.A.PY + (lat-s.A.Lat)/dLat*(s.B.PY-s.A.PY)
	return px, py, true
}

func (s Site) ToLatLon(px, py float64) (lat, lon float64, ok bool) {
	if s.B.PX == s.A.PX || s.B.PY == s.A.PY || s.B.Lon == s.A.Lon || s.B.Lat == s.A.Lat {
		return 0, 0, false
	}
	lon = s.A.Lon + (px-s.A.PX)/(s.B.PX-s.A.PX)*(s.B.Lon-s.A.Lon)
	lat = s.A.Lat + (py-s.A.PY)/(s.B.PY-s.A.PY)*(s.B.Lat-s.A.Lat)
	return lat, lon, true
}

// EtaSeconds is the straight-line travel time at the group's effective speed.
// Obstacle routing arrives in a later milestone; this is the lower bound.
func EtaSeconds(distM float64, g Group) float64 {
	v := g.MaxMph * g.Adjust * mpsPerMph
	if v <= 0 {
		return math.Inf(1)
	}
	return distM / v
}
