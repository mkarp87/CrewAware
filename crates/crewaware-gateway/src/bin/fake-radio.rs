//! A demo companion radio on a TCP port, with trackers driving around the demo site and three
//! repeaters. Point CrewAware at it with CREWAWARE_RADIO=tcp://127.0.0.1:7777.
//!
//!   cargo run -p crewaware-gateway --bin fake-radio -- --listen 127.0.0.1:7777
//!
//! Repeater password: demo-password. "Rpt West" never answers, to show a failed read.

use crewaware_gateway::fake::FakeRadio;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args: Vec<String> = std::env::args().collect();
    let listen = args
        .iter()
        .position(|a| a == "--listen")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:7777".into());
    // Seconds between adverts from a parked tracker. Real ones check in every 15 minutes.
    let parked_secs: u64 = std::env::var("FAKE_PARKED_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);

    let radio = FakeRadio::new("Fake gateway");
    let (clat, clon) = (39.0, -104.0);
    let reps = [
        ("Rpt North", 39.0016, -104.0000, true),
        ("Rpt East", 39.0000, -103.9972, true),
        ("Rpt West", 38.9992, -104.0024, false),
    ];
    for (name, lat, lon, up) in reps {
        let k = radio.add_repeater(name, "demo-password", lat, lon, 3950);
        radio.set_repeater_responding(&k, up);
    }

    // (name, radius in degrees, speed in radians per tick, phase); radius 0 means parked.
    let units = [
        ("UTV 1", 0.0012, 0.12, 0.0),
        ("UTV 2", 0.0007, 0.09, 1.5),
        ("UTV 3", 0.0, 0.0, 0.0),
        ("UTV 4", 0.0, 0.0, 0.0),
        ("Cart 1", 0.0015, 0.05, 3.0),
        ("Cart 2", 0.0, 0.0, 0.0),
    ];
    let mut movers = Vec::new();
    for (i, (name, r, w, phase)) in units.iter().enumerate() {
        let (lat, lon) = if *r > 0.0 {
            (clat + r * f64::sin(*phase), clon + r * f64::cos(*phase))
        } else {
            (clat - 0.0010 + 0.0004 * i as f64, clon - 0.0012 + 0.0003 * i as f64)
        };
        let key = radio.add_unit(name, lat, lon, 5);
        radio.set_telemetry(&key, Some(3.8 + 0.05 * i as f64), true, true);
        movers.push((key, *r, *w, *phase, lat, lon));
    }

    let r2 = radio.clone();
    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            tick += 1;
            for (key, r, w, phase, plat, plon) in &movers {
                if *r > 0.0 {
                    // Moving trackers advert every few seconds.
                    if tick.is_multiple_of(3) {
                        let a = phase + w * (tick / 3) as f64;
                        r2.advert(key, Some((clat + r * a.sin(), clon + r * a.cos() * 1.0)));
                    }
                } else if tick.is_multiple_of(parked_secs) {
                    r2.advert(key, Some((*plat, *plon)));
                }
            }
        }
    });

    tracing::info!("fake radio listening on {listen}");
    radio.listen(&listen).await?;
    Ok(())
}
