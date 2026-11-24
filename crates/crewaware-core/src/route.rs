//! Routing around obstacles. The overlay is rasterised into a coarse grid, then one
//! shortest-path search from the incident gives a route to every unit at once.
//!
//! Costs are in "effective metres": real distance divided by the local speed factor, so a slow
//! zone makes a path look longer to the search and the ETA, without changing the real distance.

use crate::model::{Obstacle, ObstacleKind, Site};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Cells along the longer side of the image. Enough to keep a gate a few cells wide on a site of
/// a few kilometres, small enough that one search takes a few milliseconds.
const MAX_CELLS: f64 = 320.0;
const NO_PARENT: u32 = u32::MAX;

pub struct Route {
    /// Cost in effective metres (distance with slow zones counted at their reduced speed).
    pub eff_m: f64,
    /// Real distance along the route in metres.
    pub dist_m: f64,
    /// The route as overlay pixels, from the unit to the incident.
    pub path_px: Vec<[f64; 2]>,
}

struct Grid {
    w: usize,
    h: usize,
    /// Overlay pixels per cell.
    cell: f64,
    /// Metres per overlay pixel on each axis.
    mx: f64,
    my: f64,
    blocked: Vec<bool>,
    factor: Vec<f64>,
}

#[derive(PartialEq)]
struct Item(f64, usize);
impl Eq for Item {}
impl Ord for Item {
    fn cmp(&self, o: &Self) -> Ordering {
        o.0.total_cmp(&self.0) // min-heap
    }
}
impl PartialOrd for Item {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

pub struct RouteField {
    grid: Grid,
    eff: Vec<f64>,
    parent: Vec<u32>,
    goal_px: (f64, f64),
}

/// Metres per overlay pixel on each axis, from the two georeferencing anchors.
fn metres_per_px(site: &Site) -> Option<(f64, f64)> {
    if !site.geo_ok() {
        return None;
    }
    let mid_lat = (site.a.lat + site.b.lat) / 2.0;
    let east = (site.b.lon - site.a.lon).abs() * 111_320.0 * mid_lat.to_radians().cos();
    let north = (site.b.lat - site.a.lat).abs() * 110_574.0;
    let (dx, dy) = ((site.b.px - site.a.px).abs(), (site.b.py - site.a.py).abs());
    Some((east / dx, north / dy))
}

impl Grid {
    fn new(site: &Site, obstacles: &[Obstacle]) -> Option<Grid> {
        let (mx, my) = metres_per_px(site)?;
        if !(site.img_w > 0.0 && site.img_h > 0.0) {
            return None;
        }
        let cell = site.img_w.max(site.img_h) / MAX_CELLS;
        let w = (site.img_w / cell).ceil() as usize;
        let h = (site.img_h / cell).ceil() as usize;
        let mut g = Grid {
            w,
            h,
            cell,
            mx,
            my,
            blocked: vec![false; w * h],
            factor: vec![1.0; w * h],
        };
        for o in obstacles {
            match o.kind {
                ObstacleKind::Wall => g.polyline(&o.points, false),
                ObstacleKind::Gate if !o.open => g.polyline(&o.points, false),
                ObstacleKind::Gate => {}
                ObstacleKind::NoGo => {
                    g.polyline(&o.points, true);
                    g.fill(&o.points, |g, i| g.blocked[i] = true);
                }
                ObstacleKind::Slow => {
                    let f = o.factor.clamp(0.05, 1.0);
                    g.fill(&o.points, |g, i| g.factor[i] = g.factor[i].min(f));
                }
            }
        }
        Some(g)
    }

    fn cell_of(&self, px: f64, py: f64) -> Option<(usize, usize)> {
        if !(px.is_finite() && py.is_finite()) {
            return None;
        }
        let (cx, cy) = ((px / self.cell).floor(), (py / self.cell).floor());
        (cx >= 0.0 && cy >= 0.0 && (cx as usize) < self.w && (cy as usize) < self.h)
            .then_some((cx as usize, cy as usize))
    }

    fn idx(&self, c: (usize, usize)) -> usize {
        c.1 * self.w + c.0
    }

    /// Marks the cells a polyline passes through. Consecutive samples that change both cell
    /// coordinates also mark one of the cells between them, so a wall is a 4-connected barrier that
    /// neither a diagonal step nor a sampled sight line can slip through.
    fn polyline(&mut self, pts: &[[f64; 2]], close: bool) {
        let n = pts.len();
        if n < 2 {
            return;
        }
        let segs = if close { n } else { n - 1 };
        let mut prev: Option<(usize, usize)> = None;
        for s in 0..segs {
            let (a, b) = (pts[s], pts[(s + 1) % n]);
            let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
            let steps = ((len / self.cell) * 4.0).ceil().max(1.0) as usize;
            for k in 0..=steps {
                let t = k as f64 / steps as f64;
                let (x, y) = (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t);
                let c = (
                    (x / self.cell).floor().clamp(0.0, self.w as f64 - 1.0) as usize,
                    (y / self.cell).floor().clamp(0.0, self.h as f64 - 1.0) as usize,
                );
                if let Some(p) = prev {
                    if p.0 != c.0 && p.1 != c.1 {
                        let i = self.idx((p.0, c.1));
                        self.blocked[i] = true;
                    }
                }
                let i = self.idx(c);
                self.blocked[i] = true;
                prev = Some(c);
            }
        }
    }

    /// Calls `f` for every cell whose centre is inside the polygon.
    fn fill(&mut self, pts: &[[f64; 2]], f: impl Fn(&mut Grid, usize)) {
        if pts.len() < 3 {
            return;
        }
        let (mut minx, mut maxx, mut miny, mut maxy) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        for p in pts {
            minx = minx.min(p[0]);
            maxx = maxx.max(p[0]);
            miny = miny.min(p[1]);
            maxy = maxy.max(p[1]);
        }
        let x0 = ((minx / self.cell).floor().max(0.0)) as usize;
        let x1 = ((maxx / self.cell).ceil().min(self.w as f64)) as usize;
        let y0 = ((miny / self.cell).floor().max(0.0)) as usize;
        let y1 = ((maxy / self.cell).ceil().min(self.h as f64)) as usize;
        for cy in y0..y1 {
            for cx in x0..x1 {
                let (px, py) = ((cx as f64 + 0.5) * self.cell, (cy as f64 + 0.5) * self.cell);
                if inside(pts, px, py) {
                    let i = self.idx((cx, cy));
                    f(self, i);
                }
            }
        }
    }

    /// The nearest free cell to `c`. A unit parked against a wall can land on a wall cell after
    /// rounding, and an incident may be placed inside a no-go area.
    fn free_near(&self, c: (usize, usize)) -> Option<(usize, usize)> {
        if !self.blocked[self.idx(c)] {
            return Some(c);
        }
        for r in 1..=(self.w.max(self.h) as i64) {
            let mut best: Option<((usize, usize), i64)> = None;
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs().max(dy.abs()) != r {
                        continue;
                    }
                    let (x, y) = (c.0 as i64 + dx, c.1 as i64 + dy);
                    if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
                        continue;
                    }
                    let cc = (x as usize, y as usize);
                    if !self.blocked[self.idx(cc)] {
                        let d = dx * dx + dy * dy;
                        if best.is_none_or(|(_, bd)| d < bd) {
                            best = Some((cc, d));
                        }
                    }
                }
            }
            if let Some((cc, _)) = best {
                return Some(cc);
            }
        }
        None
    }

    fn step_m(&self, dx: f64, dy: f64) -> f64 {
        (dx * self.mx).hypot(dy * self.my)
    }

    /// Whether a straight line between two overlay points crosses no blocked cell, and the
    /// effective and real cost of travelling it.
    fn sight(&self, a: (f64, f64), b: (f64, f64)) -> Option<(f64, f64)> {
        let len_px = (b.0 - a.0).hypot(b.1 - a.1);
        let n = ((len_px / self.cell) * 4.0).ceil().max(1.0) as usize;
        let (sx, sy) = ((b.0 - a.0) / n as f64, (b.1 - a.1) / n as f64);
        let step = self.step_m(sx, sy);
        let (mut eff, mut real) = (0.0, 0.0);
        let mut at = a;
        for _ in 0..n {
            let mid = (at.0 + sx / 2.0, at.1 + sy / 2.0);
            let c = self.cell_of(mid.0, mid.1)?;
            let i = self.idx(c);
            if self.blocked[i] {
                return None;
            }
            eff += step / self.factor[i];
            real += step;
            at = (at.0 + sx, at.1 + sy);
        }
        // The end points themselves must be on free ground too.
        for p in [a, b] {
            let c = self.cell_of(p.0, p.1)?;
            if self.blocked[self.idx(c)] {
                return None;
            }
        }
        Some((eff, real))
    }
}

fn inside(pts: &[[f64; 2]], x: f64, y: f64) -> bool {
    let mut c = false;
    let mut j = pts.len() - 1;
    for i in 0..pts.len() {
        let (a, b) = (pts[i], pts[j]);
        if (a[1] > y) != (b[1] > y) && x < (b[0] - a[0]) * (y - a[1]) / (b[1] - a[1]) + a[0] {
            c = !c;
        }
        j = i;
    }
    c
}

impl RouteField {
    /// Searches outward from the goal. None when the site cannot be pinned to the map, or the
    /// goal is off the overlay or walled in.
    pub fn build(site: &Site, obstacles: &[Obstacle], goal_px: (f64, f64)) -> Option<RouteField> {
        let grid = Grid::new(site, obstacles)?;
        let goal = grid.free_near(grid.cell_of(goal_px.0, goal_px.1)?)?;
        let n = grid.w * grid.h;
        let mut eff = vec![f64::INFINITY; n];
        let mut parent = vec![NO_PARENT; n];
        let mut heap = BinaryHeap::new();
        let g = grid.idx(goal);
        eff[g] = 0.0;
        heap.push(Item(0.0, g));
        while let Some(Item(d, i)) = heap.pop() {
            if d > eff[i] {
                continue;
            }
            let (cx, cy) = ((i % grid.w) as i64, (i / grid.w) as i64);
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let (nx, ny) = (cx + dx, cy + dy);
                    if nx < 0 || ny < 0 || nx >= grid.w as i64 || ny >= grid.h as i64 {
                        continue;
                    }
                    let j = ny as usize * grid.w + nx as usize;
                    if grid.blocked[j] {
                        continue;
                    }
                    if dx != 0 && dy != 0 {
                        // No cutting corners past a blocked cell.
                        let a = cy as usize * grid.w + nx as usize;
                        let b = ny as usize * grid.w + cx as usize;
                        if grid.blocked[a] || grid.blocked[b] {
                            continue;
                        }
                    }
                    let m = grid.step_m(dx as f64 * grid.cell, dy as f64 * grid.cell);
                    let f = (grid.factor[i] + grid.factor[j]) / 2.0;
                    let nd = d + m / f;
                    if nd < eff[j] {
                        eff[j] = nd;
                        parent[j] = i as u32;
                        heap.push(Item(nd, j));
                    }
                }
            }
        }
        Some(RouteField {
            grid,
            eff,
            parent,
            goal_px,
        })
    }

    /// Route from an overlay point to the goal. None when the point is off the overlay or cannot
    /// reach the goal.
    pub fn route_from(&self, px: f64, py: f64) -> Option<Route> {
        let g = &self.grid;
        let start = g.free_near(g.cell_of(px, py)?)?;
        let si = g.idx(start);
        if !self.eff[si].is_finite() {
            return None;
        }
        // Follow the parents back to the goal, giving the cell path.
        let mut cells = vec![start];
        let mut i = si;
        while self.parent[i] != NO_PARENT {
            i = self.parent[i] as usize;
            cells.push((i % g.w, i / g.w));
        }
        let centre = |c: (usize, usize)| ((c.0 as f64 + 0.5) * g.cell, (c.1 as f64 + 0.5) * g.cell);
        let mut pts: Vec<(f64, f64)> = vec![(px, py)];
        pts.extend(cells.iter().skip(1).map(|&c| centre(c)));
        pts.push(self.goal_px);

        // String pulling: from each point, jump to the furthest later point in clear sight, so the
        // route is not bent by the grid's eight directions.
        // Cost still to go at each point, from the search, so a shortcut is only taken when it is
        // no dearer than the path it replaces. Without this, string pulling would cut straight
        // through a slow zone the search deliberately went round.
        let mut togo: Vec<f64> = cells.iter().map(|&c| self.eff[g.idx(c)]).collect();
        togo.push(0.0);
        let slack = 1.5 * g.step_m(g.cell, g.cell);
        let mut out = vec![pts[0]];
        let (mut eff, mut real) = (0.0, 0.0);
        let mut at = 0;
        while at + 1 < pts.len() {
            let mut next = at + 1;
            let mut best: Option<(f64, f64)> = None;
            // Check furthest first; the cell path guarantees the neighbour is reachable.
            for cand in (at + 1..pts.len()).rev() {
                if let Some(c) = g.sight(pts[at], pts[cand]) {
                    if cand == at + 1 || c.0 <= togo[at] - togo[cand] + slack {
                        next = cand;
                        best = Some(c);
                        break;
                    }
                }
            }
            let (e, r) = match best {
                Some(c) => c,
                None => {
                    // The first or last hop starts or ends in a cell we nudged off a wall.
                    let m = g.step_m(pts[next].0 - pts[at].0, pts[next].1 - pts[at].1);
                    (m, m)
                }
            };
            eff += e;
            real += r;
            out.push(pts[next]);
            at = next;
        }
        Some(Route {
            eff_m: eff,
            dist_m: real,
            path_px: out.into_iter().map(|p| [p.0, p.1]).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Anchor;

    /// A 1000 x 700 px overlay at 1 m per px, so distances read directly in metres.
    fn site() -> Site {
        let mut s = Site {
            img_w: 1000.0,
            img_h: 700.0,
            ..Site::default()
        };
        // ~1 m per pixel: 1 px = 1/111320/cos(lat) deg lon, 1/110574 deg lat.
        let lat0: f64 = 39.0;
        let dlon = 1000.0 / (111_320.0 * lat0.to_radians().cos());
        let dlat = 700.0 / 110_574.0;
        s.a = Anchor {
            px: 0.0,
            py: 0.0,
            lat: lat0 + dlat,
            lon: -104.0,
        };
        s.b = Anchor {
            px: 1000.0,
            py: 700.0,
            lat: lat0,
            lon: -104.0 + dlon,
        };
        s
    }

    fn obs(id: i32, kind: ObstacleKind, pts: &[[f64; 2]]) -> Obstacle {
        Obstacle {
            id,
            kind,
            name: String::new(),
            points: pts.to_vec(),
            open: false,
            factor: 0.5,
        }
    }

    #[test]
    fn open_field_is_a_straight_line() {
        let f = RouteField::build(&site(), &[], (900.0, 350.0)).unwrap();
        let r = f.route_from(100.0, 350.0).unwrap();
        assert!((r.dist_m - 800.0).abs() < 8.0, "{}", r.dist_m);
        assert!((r.eff_m - r.dist_m).abs() < 1e-6);
        let r = f.route_from(100.0, 100.0).unwrap();
        let straight = (800.0f64).hypot(250.0);
        assert!(
            (r.dist_m - straight).abs() / straight < 0.01,
            "{} vs {straight}",
            r.dist_m
        );
        assert_eq!(r.path_px.first().unwrap(), &[100.0, 100.0]);
        assert_eq!(r.path_px.last().unwrap(), &[900.0, 350.0]);
    }

    #[test]
    fn a_wall_forces_a_detour_around_its_end() {
        // A vertical wall from y=0 to y=500 at x=500; the way round is below y=500.
        let wall = obs(1, ObstacleKind::Wall, &[[500.0, 0.0], [500.0, 500.0]]);
        let f = RouteField::build(&site(), &[wall], (900.0, 100.0)).unwrap();
        let r = f.route_from(100.0, 100.0).unwrap();
        // Down to about (500, 505), then back up: far longer than the 800 m direct line.
        assert!(r.dist_m > 1000.0 && r.dist_m < 1250.0, "{}", r.dist_m);
        assert!(r.path_px.iter().any(|p| p[1] > 495.0), "{:?}", r.path_px);
        // A unit on the far side has a clear run.
        let r2 = f.route_from(700.0, 100.0).unwrap();
        assert!((r2.dist_m - 200.0).abs() < 5.0, "{}", r2.dist_m);
    }

    #[test]
    fn diagonal_wall_does_not_leak() {
        // A full-length diagonal wall from corner to corner splits the site in two.
        let wall = obs(1, ObstacleKind::Wall, &[[0.0, 0.0], [1000.0, 700.0]]);
        let f = RouteField::build(&site(), &[wall], (900.0, 100.0)).unwrap();
        // Top-right side is the goal's side; bottom-left is cut off completely.
        assert!(f.route_from(100.0, 600.0).is_none());
        assert!(f.route_from(800.0, 150.0).is_some());
    }

    #[test]
    fn a_closed_gate_blocks_and_an_open_gate_does_not() {
        // A wall across the whole site with a gate in the middle.
        let left = obs(1, ObstacleKind::Wall, &[[500.0, 0.0], [500.0, 300.0]]);
        let right = obs(2, ObstacleKind::Wall, &[[500.0, 400.0], [500.0, 700.0]]);
        let mut gate = obs(3, ObstacleKind::Gate, &[[500.0, 300.0], [500.0, 400.0]]);
        let closed = RouteField::build(&site(), &[left.clone(), right.clone(), gate.clone()], (900.0, 350.0)).unwrap();
        assert!(closed.route_from(100.0, 350.0).is_none());
        gate.open = true;
        let open = RouteField::build(&site(), &[left, right, gate], (900.0, 350.0)).unwrap();
        let r = open.route_from(100.0, 350.0).unwrap();
        assert!((r.dist_m - 800.0).abs() < 10.0, "{}", r.dist_m);
    }

    #[test]
    fn no_go_area_is_avoided_and_cannot_be_entered() {
        let lake = obs(
            1,
            ObstacleKind::NoGo,
            &[[400.0, 200.0], [600.0, 200.0], [600.0, 500.0], [400.0, 500.0]],
        );
        let f = RouteField::build(&site(), &[lake], (900.0, 350.0)).unwrap();
        let r = f.route_from(100.0, 350.0).unwrap();
        assert!(r.dist_m > 850.0, "must go round: {}", r.dist_m);
        // A goal inside the area is moved to the nearest free ground rather than failing.
        let inside = obs(
            1,
            ObstacleKind::NoGo,
            &[[400.0, 200.0], [600.0, 200.0], [600.0, 500.0], [400.0, 500.0]],
        );
        assert!(RouteField::build(&site(), &[inside], (500.0, 350.0)).is_some());
    }

    #[test]
    fn slow_zone_costs_time_not_distance_and_is_avoided_when_cheaper() {
        // A half-speed band across the whole route, narrow: crossing it costs a little extra.
        let band = obs(
            1,
            ObstacleKind::Slow,
            &[[400.0, 0.0], [450.0, 0.0], [450.0, 700.0], [400.0, 700.0]],
        );
        let f = RouteField::build(&site(), &[band], (900.0, 350.0)).unwrap();
        let r = f.route_from(100.0, 350.0).unwrap();
        assert!((r.dist_m - 800.0).abs() < 10.0);
        assert!((r.eff_m - (800.0 + 50.0)).abs() < 12.0, "eff {}", r.eff_m);

        // A big very slow field with a short way round: the route goes round it.
        let mut field = obs(
            2,
            ObstacleKind::Slow,
            &[[300.0, 250.0], [700.0, 250.0], [700.0, 450.0], [300.0, 450.0]],
        );
        field.factor = 0.1;
        let f = RouteField::build(&site(), &[field], (900.0, 350.0)).unwrap();
        let r = f.route_from(100.0, 350.0).unwrap();
        assert!(r.dist_m > 810.0 && r.dist_m < 1000.0, "went round: {}", r.dist_m);
        assert!(r.eff_m < 1000.0, "{}", r.eff_m);
    }

    #[test]
    fn unit_off_the_overlay_or_unpinned_site_gives_no_route() {
        let f = RouteField::build(&site(), &[], (900.0, 350.0)).unwrap();
        assert!(f.route_from(-50.0, 100.0).is_none());
        assert!(f.route_from(f64::NAN, 1.0).is_none());
        let mut unpinned = site();
        unpinned.b = unpinned.a;
        assert!(RouteField::build(&unpinned, &[], (10.0, 10.0)).is_none());
        assert!(RouteField::build(&site(), &[], (5000.0, 10.0)).is_none());
    }

    #[test]
    fn unit_standing_on_a_wall_is_nudged_off_it() {
        let wall = obs(1, ObstacleKind::Wall, &[[500.0, 0.0], [500.0, 700.0]]);
        let f = RouteField::build(&site(), &[wall], (900.0, 350.0)).unwrap();
        // Standing exactly on the wall line, which is an uncrossable full-height wall: the unit is
        // moved to a free cell on one side; whether it can reach depends on the side.
        let r = f.route_from(500.0, 350.0);
        assert!(r.is_none() || r.unwrap().dist_m > 300.0);
    }

    #[test]
    fn one_search_is_fast_enough() {
        let walls: Vec<Obstacle> = (0..20)
            .map(|i| {
                obs(
                    i,
                    ObstacleKind::Wall,
                    &[[50.0 * i as f64, 0.0], [50.0 * i as f64, 600.0]],
                )
            })
            .collect();
        let t = std::time::Instant::now();
        let f = RouteField::build(&site(), &walls, (990.0, 650.0)).unwrap();
        for k in 0..15 {
            let _ = f.route_from(10.0 + k as f64 * 5.0, 100.0);
        }
        // Debug builds are slow; this is a smoke check against accidental quadratic behaviour.
        assert!(t.elapsed().as_millis() < 1500, "{:?}", t.elapsed());
    }

    #[test]
    fn anisotropic_pixels_are_respected() {
        // Wider than tall: 2 m per px horizontally, 1 m vertically.
        let mut s = site();
        s.b.lon = s.a.lon + (s.b.lon - s.a.lon) * 2.0;
        let f = RouteField::build(&s, &[], (900.0, 350.0)).unwrap();
        let r = f.route_from(100.0, 350.0).unwrap();
        assert!((r.dist_m - 1600.0).abs() < 20.0, "{}", r.dist_m);
        let r = f.route_from(900.0, 50.0).unwrap();
        assert!((r.dist_m - 300.0).abs() < 5.0, "{}", r.dist_m);
    }
}
