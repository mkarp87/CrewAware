//! Integration tests. They need a real PostgreSQL and are skipped unless TEST_DATABASE_URL is set,
//! e.g. TEST_DATABASE_URL=postgres://user:pw@localhost/crewaware_test cargo test -p crewaware-store
//! The database is wiped at the start of the run, so point it at a scratch database.

use crewaware_core::engine::State;
use crewaware_core::*;
use crewaware_store::Store;

async fn fresh() -> Option<Store> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let wipe = sqlx::PgPool::connect(&url).await.unwrap();
    sqlx::query("DROP SCHEMA public CASCADE").execute(&wipe).await.unwrap();
    sqlx::query("CREATE SCHEMA public").execute(&wipe).await.unwrap();
    wipe.close().await;
    let s = Store::connect(&url)
        .await
        .unwrap()
        .with_secret_key(Some("test passphrase one"));
    s.migrate().await.unwrap();
    Some(s)
}

async fn flush(store: &Store, e: &mut Engine) {
    for c in e.drain_changes() {
        store.apply(&c).await.unwrap();
    }
}

// One sequential test: the phases share a scratch database.
#[tokio::test]
async fn store_round_trip_and_auth() {
    let Some(store) = fresh().await else {
        eprintln!("TEST_DATABASE_URL not set, skipping");
        return;
    };

    // migrations are idempotent
    store.migrate().await.unwrap();

    // ---- state survives a reload ----
    let mut e = Engine::new(State::default());
    e.seed_demo();
    e.report_position(Position {
        unit_id: "u1".into(),
        lat: 39.0,
        lon: -104.0,
        speed_mph: Some(7.0),
        battery: 81,
        age_secs: 0,
    });
    e.report_repeater("r1");
    let inc = e.open_incident(39.0005, -104.0005, "Medical", "seat A");
    e.dispatch(inc.id, &["u1".into()], "seat A").unwrap();
    e.update_site(|s| s.slogan = "Know where your crew is".into());
    flush(&store, &mut e).await;

    let loaded = store.load().await.unwrap();
    assert_eq!(loaded.site.name, "Demo site");
    assert_eq!(loaded.site.slogan, "Know where your crew is");
    assert_eq!(loaded.groups.len(), 2);
    assert_eq!(loaded.units.len(), 7);
    let u1 = loaded.units.iter().find(|u| u.id == "u1").unwrap();
    assert_eq!(u1.status, Status::Dispatched);
    assert_eq!(u1.incident_id, Some(inc.id));
    assert!(u1.has_pos && u1.battery == 81);
    assert_eq!(loaded.next_incident, inc.id + 1);
    assert_eq!(loaded.incidents[0].units, vec!["u1".to_string()]);
    assert!(loaded
        .log
        .iter()
        .any(|l| l.text.contains("Dispatched UTV 1 to Medical") && l.seat == "seat A"));
    let log_times: Vec<_> = loaded.log.iter().map(|l| l.t).collect();
    assert!(log_times.windows(2).all(|w| w[0] <= w[1]), "log must load oldest first");

    // a restarted engine shows no contact until the unit reports again
    let e2 = Engine::new(loaded);
    assert_eq!(
        e2.snapshot().units.iter().find(|u| u.unit.id == "u1").unwrap().contact,
        Contact::NoContact
    );

    // closing persists and next_incident keeps counting up
    e.close_incident(inc.id, "seat A").unwrap();
    flush(&store, &mut e).await;
    let loaded = store.load().await.unwrap();
    assert!(!loaded.incidents[0].open && loaded.incidents[0].closed.is_some());
    assert_eq!(
        loaded.units.iter().find(|u| u.id == "u1").unwrap().status,
        Status::Available
    );

    // settings round trip
    let s = Settings {
        parked_late_min: 10.0,
        parked_no_contact_min: 20.0,
        ..Settings::default()
    };
    e.set_settings(s.clone()).unwrap();
    flush(&store, &mut e).await;
    assert_eq!(store.load().await.unwrap().settings, s);

    // ---- radio keys, obstacles, incident log, removal ----
    let key = "ab".repeat(32);
    let adopted = e.adopt_unit(&key, "UTV 9", "utv").unwrap();
    let rkey = "cd".repeat(32);
    let rid = e.adopt_repeater(&rkey, "Rpt North").unwrap();
    e.set_obstacle(
        Obstacle {
            id: 0,
            kind: ObstacleKind::Gate,
            name: "Gate A".into(),
            points: vec![[10.0, 10.0], [10.0, 90.0]],
            open: false,
            factor: 1.0,
        },
        "seat A",
    )
    .unwrap();
    e.set_obstacle(
        Obstacle {
            id: 0,
            kind: ObstacleKind::Slow,
            name: "Mud".into(),
            points: vec![[0.0, 0.0], [50.0, 0.0], [50.0, 50.0]],
            open: false,
            factor: 0.4,
        },
        "seat A",
    )
    .unwrap();
    e.set_gate(1, true, "seat B").unwrap();
    flush(&store, &mut e).await;
    let loaded = store.load().await.unwrap();
    assert_eq!(loaded.units.iter().find(|u| u.id == adopted).unwrap().node_key, key);
    assert_eq!(loaded.repeaters.iter().find(|r| r.id == rid).unwrap().node_key, rkey);
    assert_eq!(loaded.obstacles.len(), 2);
    assert!(loaded.obstacles[0].open && loaded.obstacles[0].kind == ObstacleKind::Gate);
    assert_eq!(loaded.obstacles[1].factor, 0.4);
    assert_eq!(loaded.obstacles[1].points.len(), 3);
    // Two nodes cannot share a key, even if the engine were bypassed.
    let mut dup = loaded.units.iter().find(|u| u.id == adopted).unwrap().clone();
    dup.id = "dup".into();
    assert!(store.apply(&Change::Unit(dup)).await.is_err());

    // The incident report reads its log lines back, oldest first.
    let rep = store.incident_log(inc.id).await.unwrap();
    assert!(rep.len() >= 3, "{rep:?}");
    assert!(rep.first().unwrap().text.starts_with("Opened"));
    assert!(rep.last().unwrap().text.starts_with("Closed"));
    assert!(store.incident_log(9999).await.unwrap().is_empty());

    // Removing a unit or repeater removes it and its rows.
    e.remove_unit(&adopted, "seat A").unwrap();
    e.remove_repeater(&rid, "seat A").unwrap();
    flush(&store, &mut e).await;
    let loaded = store.load().await.unwrap();
    assert!(loaded.units.iter().all(|u| u.id != adopted));
    assert!(loaded.repeaters.iter().all(|r| r.id != rid));

    // ---- saved sites ----
    let mut data = e.export_site();
    data.site.name = "Airshow 2027".into();
    let id = store.save_profile(&data).await.unwrap();
    assert_eq!(store.save_profile(&data).await.unwrap(), id, "same name replaces");
    let listed = store.list_profiles().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1, "Airshow 2027");
    let back = store.get_profile(id).await.unwrap().unwrap();
    assert_eq!(back, data);
    assert!(store.get_profile(id + 100).await.unwrap().is_none());
    assert!(store.delete_profile(id).await.unwrap());
    assert!(!store.delete_profile(id).await.unwrap());

    // ---- position history is throttled per unit ----
    for _ in 0..5 {
        e.report_position(Position {
            unit_id: "u2".into(),
            lat: 39.0,
            lon: -104.0,
            speed_mph: Some(5.0),
            battery: 70,
            age_secs: 0,
        });
    }
    flush(&store, &mut e).await;
    assert_eq!(store.history_count("u2").await.unwrap(), 1);
    assert_eq!(store.prune_history(30).await.unwrap(), 0);

    // ---- assets ----
    store.put_asset("overlay-1.png", "image/png", &[1, 2, 3]).await.unwrap();
    store.put_asset("logo-1.png", "image/png", &[9]).await.unwrap();
    assert_eq!(
        store.get_asset("overlay-1.png").await.unwrap().unwrap().1,
        vec![1, 2, 3]
    );
    assert!(store.get_asset("nope").await.unwrap().is_none());
    assert_eq!(store.prune_assets(&["overlay-1.png"]).await.unwrap(), 1);
    assert!(store.get_asset("logo-1.png").await.unwrap().is_none());

    // ---- repeater passwords: encrypted at rest, tied to the key ----
    let plain = "hunter2-repeater-pw";
    store.set_repeater_password("r1", plain).await.unwrap();
    assert_eq!(store.get_repeater_password("r1").await.unwrap().as_deref(), Some(plain));
    assert!(store.get_repeater_password("r2").await.unwrap().is_none());
    let url = std::env::var("TEST_DATABASE_URL").unwrap();
    let raw = sqlx::PgPool::connect(&url).await.unwrap();
    let ct: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM repeater_credentials WHERE repeater_id = 'r1'")
        .fetch_one(&raw)
        .await
        .unwrap();
    assert!(
        !ct.windows(plain.len()).any(|w| w == plain.as_bytes()),
        "password must not be stored in the clear"
    );
    let wrong = Store::connect(&url)
        .await
        .unwrap()
        .with_secret_key(Some("a different passphrase"));
    assert!(
        wrong.get_repeater_password("r1").await.is_err(),
        "wrong key must fail, not return garbage"
    );
    let nokey = Store::connect(&url).await.unwrap();
    assert!(!nokey.credentials_enabled());
    assert!(nokey.set_repeater_password("r1", "x").await.is_err());
    raw.close().await;

    // readings and the credential flag survive a reload
    e.record_reading(
        "r1",
        true,
        "",
        serde_json::json!({ "battery_mv": 4010, "noise_floor_dbm": -112 }),
    )
    .unwrap();
    e.record_reading("r2", false, "no response", serde_json::Value::Null)
        .unwrap();
    flush(&store, &mut e).await;
    let loaded = store.load().await.unwrap();
    let r1 = loaded.repeaters.iter().find(|r| r.id == "r1").unwrap();
    assert!(r1.has_credentials);
    let rd = r1.reading.as_ref().unwrap();
    assert!(rd.ok && rd.data["battery_mv"] == 4010);
    let r2 = loaded.repeaters.iter().find(|r| r.id == "r2").unwrap();
    assert!(!r2.has_credentials);
    assert!(!r2.reading.as_ref().unwrap().ok && r2.reading.as_ref().unwrap().error == "no response");
    store.delete_repeater_password("r1").await.unwrap();
    assert!(store.get_repeater_password("r1").await.unwrap().is_none());

    // ---- admin accounts ----
    assert!(
        store.ensure_admin("admin", "short").await.is_err(),
        "short password rejected"
    );
    assert!(store.ensure_admin("admin", "correct horse battery").await.unwrap());
    assert!(
        !store.ensure_admin("admin", "a different password!").await.unwrap(),
        "existing account untouched"
    );
    let id = store
        .verify_login("admin", "correct horse battery")
        .await
        .unwrap()
        .expect("login ok");
    assert!(store
        .verify_login("admin", "a different password!")
        .await
        .unwrap()
        .is_none());
    assert!(store
        .verify_login("nobody", "correct horse battery")
        .await
        .unwrap()
        .is_none());

    let token = store.create_session(id).await.unwrap();
    assert_eq!(store.session_user(&token).await.unwrap().as_deref(), Some("admin"));
    assert!(store.session_user("garbage").await.unwrap().is_none());
    store.delete_session(&token).await.unwrap();
    assert!(store.session_user(&token).await.unwrap().is_none());
}
