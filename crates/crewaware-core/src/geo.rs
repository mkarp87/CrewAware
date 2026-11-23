use crate::model::{Group, Site};

pub const FEET_PER_M: f64 = 3.280_839_895;
pub const MPS_PER_MPH: f64 = 0.447_04;
const EARTH_M: f64 = 6_371_000.0;

pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = std::f64::consts::PI / 180.0;
    let (d_lat, d_lon) = ((lat2 - lat1) * r, (lon2 - lon1) * r);
    let a = (d_lat / 2.0).sin().powi(2) + (lat1 * r).cos() * (lat2 * r).cos() * (d_lon / 2.0).sin().powi(2);
    2.0 * EARTH_M * a.sqrt().min(1.0).asin()
}

impl Site {
    /// The two anchors must differ on both axes, or the overlay cannot be pinned.
    pub fn geo_ok(&self) -> bool {
        self.b.px != self.a.px && self.b.py != self.a.py && self.b.lon != self.a.lon && self.b.lat != self.a.lat
    }

    /// Lat/lon to overlay pixels using two anchors (north-up images, no rotation).
    pub fn to_pixel(&self, lat: f64, lon: f64) -> Option<(f64, f64)> {
        if !self.geo_ok() {
            return None;
        }
        let px = self.a.px + (lon - self.a.lon) / (self.b.lon - self.a.lon) * (self.b.px - self.a.px);
        let py = self.a.py + (lat - self.a.lat) / (self.b.lat - self.a.lat) * (self.b.py - self.a.py);
        Some((px, py))
    }

    pub fn to_lat_lon(&self, px: f64, py: f64) -> Option<(f64, f64)> {
        if !self.geo_ok() {
            return None;
        }
        let lon = self.a.lon + (px - self.a.px) / (self.b.px - self.a.px) * (self.b.lon - self.a.lon);
        let lat = self.a.lat + (py - self.a.py) / (self.b.py - self.a.py) * (self.b.lat - self.a.lat);
        Some((lat, lon))
    }
}

/// Straight-line travel time at the group's effective speed. Obstacle routing comes later,
/// so this is a lower bound. Infinite when the group has no usable speed.
pub fn eta_seconds(dist_m: f64, g: &Group) -> f64 {
    let v = g.max_mph * g.adjust * MPS_PER_MPH;
    if v <= 0.0 {
        f64::INFINITY
    } else {
        dist_m / v
    }
}
