use crewaware_gateway::client::{contact_age, Event, Gateway, GwError, Options};
use crewaware_gateway::fake::FakeRadio;
use crewaware_gateway::proto::*;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;

fn fast() -> Options {
    Options {
        resync: Duration::from_millis(400),
        command_timeout: Duration::from_secs(2),
        reconnect: Duration::from_millis(200),
        mesh_wait_min: Duration::from_millis(300),
        mesh_wait_max: Duration::from_millis(800),
    }
}

async fn start(radio: &FakeRadio) -> (Gateway, UnboundedReceiver<Event>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let r = radio.clone();
    tokio::spawn(async move {
        let _ = r.serve_listener(l).await;
    });
    let (g, rx) = Gateway::start(&format!("tcp://{addr}"), fast()).unwrap();
    (g, rx)
}

async fn wait_connected(g: &Gateway) {
    for _ in 0..100 {
        if g.status().connected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("gateway never connected: {:?}", g.status());
}

async fn next_contact(rx: &mut UnboundedReceiver<Event>, key: &str) -> (ContactRec, Option<u64>) {
    timeout(Duration::from_secs(5), async {
        loop {
            match rx.recv().await.expect("events closed") {
                Event::Contact { rec, age_secs } if rec.key == key => return (rec, age_secs),
                _ => {}
            }
        }
    })
    .await
    .expect("no contact event")
}

#[tokio::test]
async fn handshake_sets_clock_and_syncs_contacts_with_ages() {
    let radio = FakeRadio::new("Gateway 1");
    radio.set_clock_offset(-86_400 * 3); // radio woke up thinking it is three days ago
    let fresh = radio.add_unit("UTV 1", 39.001, -104.001, 5);
    let old = radio.add_unit("UTV 2", 39.002, -104.002, 1200);
    let (g, mut rx) = start(&radio).await;
    wait_connected(&g).await;

    let st = g.status();
    assert_eq!(st.name, "Gateway 1");
    assert_eq!(st.model, "Fake Radio");
    assert!(radio.device_time().is_some(), "host must set the radio clock");
    let codes = radio.commands_seen();
    assert_eq!(&codes[..3], &[CMD_APP_START, CMD_DEVICE_QUERY, CMD_SET_DEVICE_TIME]);

    // Ages are relative to the (corrected) clock: contacts were recorded under the wrong clock,
    // so their lastmod values are three days in the past or otherwise untrusted, never "fresh".
    let (_, a1) = next_contact(&mut rx, &fresh).await;
    let (_, a2) = next_contact(&mut rx, &old).await;
    assert!(
        a1.is_none_or(|a| a > 3600 * 24),
        "wrong-clock contact must not look fresh: {a1:?}"
    );
    assert!(a2.is_none_or(|a| a > 3600 * 24), "{a2:?}");
}

#[tokio::test]
async fn contact_ages_when_clock_is_right() {
    let radio = FakeRadio::new("G");
    let fresh = radio.add_unit("A", 39.0, -104.0, 5);
    let old = radio.add_unit("B", 39.0, -104.0, 1200);
    let (g, mut rx) = start(&radio).await;
    wait_connected(&g).await;
    let (_, a1) = next_contact(&mut rx, &fresh).await;
    let (_, a2) = next_contact(&mut rx, &old).await;
    assert!(a1.unwrap() < 30);
    assert!((1190..1260).contains(&a2.unwrap()), "{a2:?}");
    assert_eq!(g.status().contacts, 2);
    assert!(g.status().last_sync.is_some());
}

#[tokio::test]
async fn advert_push_delivers_the_new_position_quickly() {
    let radio = FakeRadio::new("G");
    let k = radio.add_unit("UTV 1", 39.0, -104.0, 30);
    let (g, mut rx) = start(&radio).await;
    wait_connected(&g).await;
    next_contact(&mut rx, &k).await;

    radio.advert(&k, Some((39.0021, -104.0033)));
    let (rec, age) = next_contact(&mut rx, &k).await;
    assert!((rec.lat - 39.0021).abs() < 1e-6 && (rec.lon + 104.0033).abs() < 1e-6);
    assert!(age.unwrap() <= 2);

    // The same advert is not reported twice by the periodic resync.
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let mut extra = 0;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, Event::Contact { ref rec, .. } if rec.key == k) {
            extra += 1;
        }
    }
    assert_eq!(extra, 0, "unchanged contacts must not be re-reported");
}

#[tokio::test]
async fn reads_a_repeater_after_logging_in() {
    let radio = FakeRadio::new("G");
    let r = radio.add_repeater("Rpt", "secret", 39.0, -104.0, 4012);
    radio.set_repeater_status(&r, |s| s.last_snr_db = 7.25);
    let (g, _rx) = start(&radio).await;
    wait_connected(&g).await;
    let st = g.read_repeater(&r, "secret").await.unwrap();
    assert_eq!(st.battery_mv, 4012);
    assert_eq!(st.last_snr_db, 7.25);
    assert_eq!(st.noise_floor_dbm, -112);
    assert_eq!(st.recv_errors, Some(3));
}

#[tokio::test]
async fn wrong_password_silent_or_refused() {
    let radio = FakeRadio::new("G");
    let r = radio.add_repeater("Rpt", "secret", 39.0, -104.0, 4000);
    let (g, _rx) = start(&radio).await;
    wait_connected(&g).await;

    // Real repeaters usually just do not answer a bad password.
    let e = g.read_repeater(&r, "nope").await.unwrap_err();
    assert!(matches!(e, GwError::Timeout(_)), "{e:?}");

    radio.set_repeater_refuses_explicitly(&r, true);
    let e = g.read_repeater(&r, "nope").await.unwrap_err();
    assert!(e.to_string().contains("refused"), "{e}");

    // And the right password still works afterwards.
    assert!(g.read_repeater(&r, "secret").await.is_ok());
}

#[tokio::test]
async fn out_of_range_repeater_times_out_and_unknown_key_is_rejected() {
    let radio = FakeRadio::new("G");
    let r = radio.add_repeater("Rpt", "secret", 39.0, -104.0, 4000);
    radio.set_repeater_responding(&r, false);
    let (g, _rx) = start(&radio).await;
    wait_connected(&g).await;
    assert!(matches!(g.read_repeater(&r, "secret").await, Err(GwError::Timeout(_))));
    assert!(matches!(g.read_repeater("zz", "x").await, Err(GwError::Other(_))));
    assert!(matches!(
        g.read_repeater(&hex(&[1u8; 16]), "x").await,
        Err(GwError::Other(_))
    ));
    // A well-formed key the radio has never heard of behaves like out of range.
    assert!(matches!(
        g.read_repeater(&hex(&[7u8; 32]), "x").await,
        Err(GwError::Timeout(_))
    ));
}

#[tokio::test]
async fn telemetry_gives_battery_and_gps() {
    let radio = FakeRadio::new("G");
    let k = radio.add_unit("UTV 1", 39.0012, -104.0021, 600);
    radio.set_telemetry(&k, Some(3.91), true, true);
    let (g, _rx) = start(&radio).await;
    wait_connected(&g).await;
    let t = g.request_telemetry(&k).await.unwrap();
    assert_eq!(t.voltage, Some(3.91));
    assert!((t.lat.unwrap() - 39.0012).abs() < 1e-4);

    radio.set_telemetry(&k, None, false, false);
    assert!(matches!(g.request_telemetry(&k).await, Err(GwError::Timeout(_))));
}

#[tokio::test]
async fn reconnects_after_the_cable_is_pulled_and_resyncs() {
    let radio = FakeRadio::new("G");
    let k = radio.add_unit("UTV 1", 39.0, -104.0, 10);
    let (g, mut rx) = start(&radio).await;
    wait_connected(&g).await;
    next_contact(&mut rx, &k).await;

    radio.disconnect();
    // Commands while down fail cleanly rather than hanging.
    for _ in 0..50 {
        if !g.status().connected {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!g.status().connected);
    assert!(g.status().error.contains("closed"), "{:?}", g.status());

    wait_connected(&g).await;
    // The contact list is re-read after a reconnect, so the old position comes through again,
    // and a new advert after that is picked up as usual.
    let (again, _) = next_contact(&mut rx, &k).await;
    assert!((again.lat - 39.0).abs() < 1e-6);
    radio.advert(&k, Some((39.5, -104.5)));
    let (rec, _) = next_contact(&mut rx, &k).await;
    assert!((rec.lat - 39.5).abs() < 1e-6);
    assert!(g.read_repeater(&hex(&[7u8; 32]), "x").await.is_err());
}

#[tokio::test]
async fn not_connected_errors_immediately() {
    // Nothing is listening on this port.
    let (g, _rx) = Gateway::start("tcp://127.0.0.1:1", fast()).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!g.status().connected);
    assert!(g.status().error.contains("cannot open"));
    assert_eq!(
        g.read_repeater(&hex(&[1u8; 32]), "x").await.unwrap_err(),
        GwError::NotConnected
    );
}

#[test]
fn contact_age_distrusts_a_bad_clock() {
    let now = 1_800_000_000;
    assert_eq!(contact_age(1_799_999_000, now), Some(1000));
    assert_eq!(contact_age(now as u32 + 10, now), Some(0));
    assert_eq!(contact_age(now as u32 + 5000, now), None);
    assert_eq!(contact_age(1000, now), None);
}
