use crate::app::App;
use crewaware_core::Position;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct SimUnit {
    id: &'static str,
    cx: f64,
    cy: f64,
    rx: f64,
    ry: f64,
    phase: f64,
    rate: f64,
    parked: bool,
    silent_after: Option<Duration>,
    mute: bool,
    last_parked: Option<Instant>,
}

fn su(id: &'static str, cx: f64, cy: f64) -> SimUnit {
    SimUnit {
        id,
        cx,
        cy,
        rx: 0.0,
        ry: 0.0,
        phase: 0.0,
        rate: 0.0,
        parked: false,
        silent_after: None,
        mute: false,
        last_parked: None,
    }
}

/// A fake radio transport: moving units, parked units on the slow cadence, one unit that goes
/// silent, one that never reports, and a repeater that never checks in.
pub async fn run(app: Arc<App>, parked_every: Duration) {
    let mut units = [
        SimUnit {
            rx: 0.0012,
            ry: 0.0020,
            rate: 0.05,
            ..su("u1", 39.0000, -104.0000)
        },
        SimUnit {
            rx: 0.0008,
            ry: 0.0012,
            rate: 0.07,
            phase: 2.0,
            ..su("u2", 39.0005, -103.9990)
        },
        SimUnit {
            rx: 0.0006,
            ry: 0.0010,
            rate: 0.04,
            phase: 4.0,
            ..su("u3", 38.9990, -104.0010)
        },
        SimUnit {
            parked: true,
            ..su("u4", 39.0010, -104.0015)
        },
        SimUnit {
            parked: true,
            silent_after: Some(Duration::from_secs(45)),
            ..su("u5", 38.9988, -103.9985)
        },
        SimUnit {
            rx: 0.0004,
            ry: 0.0006,
            rate: 0.06,
            phase: 1.0,
            ..su("c1", 39.0003, -103.9985)
        },
        SimUnit {
            mute: true,
            ..su("u6", 0.0, 0.0)
        },
    ];
    let start = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tick.tick().await;
        let el = start.elapsed();
        for u in units.iter_mut() {
            if u.mute || u.silent_after.is_some_and(|s| el > s) {
                continue;
            }
            let battery = 60 + (rand::random::<u32>() % 30) as i32;
            if u.parked {
                if u.last_parked.is_none_or(|t| t.elapsed() >= parked_every) {
                    u.last_parked = Some(Instant::now());
                    let p = Position {
                        unit_id: u.id.into(),
                        lat: u.cx,
                        lon: u.cy,
                        speed_mph: Some(0.0),
                        battery,
                        age_secs: 0,
                    };
                    app.with(|e| e.report_position(p));
                }
                continue;
            }
            let a = u.phase + u.rate * el.as_secs_f64();
            let lat = u.cx + u.rx * a.sin();
            let lon = u.cy + u.ry * a.cos();
            let vx = u.rx * u.rate * a.cos() * 111_320.0;
            let vy = u.ry * u.rate * a.sin() * 111_320.0 * (lat.to_radians()).cos();
            let mph = vx.hypot(vy) / crewaware_core::geo::MPS_PER_MPH;
            let p = Position {
                unit_id: u.id.into(),
                lat,
                lon,
                speed_mph: Some(mph),
                battery,
                age_secs: 0,
            };
            app.with(|e| e.report_position(p));
        }
        app.with(|e| {
            e.report_repeater("r1");
            e.report_repeater("r2");
        });
    }
}
