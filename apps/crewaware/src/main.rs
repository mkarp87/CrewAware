mod api;
mod app;
mod bridge;
mod reader;
mod sim;

use anyhow::{Context, Result};
use crewaware_core::Engine;
use crewaware_store::Store;
use std::sync::Arc;
use std::time::Duration;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

async fn connect(simulate: bool) -> Result<Store> {
    let url = env("DATABASE_URL").context("DATABASE_URL is not set")?;
    // The database container may still be starting.
    let mut last = None;
    for _ in 0..30 {
        match Store::connect(&url).await {
            Ok(s) => return Ok(s.with_secret_key(secret_key(simulate)?.as_deref())),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last.unwrap()).context("could not reach the database")
}

/// CREWAWARE_SECRET_KEY encrypts repeater passwords. In simulator mode a throwaway key is used
/// when none is set, so the demo works without configuration.
fn secret_key(simulate: bool) -> Result<Option<String>> {
    match env("CREWAWARE_SECRET_KEY") {
        Some(k) if k.chars().count() < 16 => {
            anyhow::bail!("CREWAWARE_SECRET_KEY must be at least 16 characters (try: openssl rand -hex 32)")
        }
        Some(k) => Ok(Some(k)),
        None if simulate => Ok(Some(format!(
            "demo-{:016x}{:016x}",
            rand::random::<u64>(),
            rand::random::<u64>()
        ))),
        None => {
            tracing::warn!(
                "CREWAWARE_SECRET_KEY is not set: repeater passwords cannot be stored, so repeaters cannot be read"
            );
            Ok(None)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // crewaware set-password <username>   (new password read from stdin)
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("set-password") {
        let user = args
            .get(2)
            .context("usage: crewaware set-password <username>  (password on stdin)")?;
        let mut pw = String::new();
        std::io::stdin().read_line(&mut pw)?;
        let store = connect(false).await?;
        store.migrate().await?;
        store
            .set_admin_password(user, pw.trim_end_matches(['\r', '\n']))
            .await?;
        println!("password set for {user}; existing sessions ended");
        return Ok(());
    }

    let simulate = env("CREWAWARE_SIM").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let store = Arc::new(connect(simulate).await?);
    store.migrate().await.context("database migration failed")?;

    if let Some(pw) = env("CREWAWARE_ADMIN_PASSWORD") {
        let user = env("CREWAWARE_ADMIN_USERNAME").unwrap_or_else(|| "admin".into());
        if store
            .ensure_admin(&user, &pw)
            .await
            .context("creating the first admin account")?
        {
            tracing::info!("created admin account {user}");
        }
    }

    let engine = Engine::new(store.load().await.context("loading state")?);
    let secure = env("CREWAWARE_COOKIE_SECURE").is_some_and(|v| v == "1");
    let app = app::App::new(engine, store.clone(), secure);
    if simulate {
        app.with(|e| e.seed_demo());
        // Give the demo repeaters a password so the simulated reader has something to log in with.
        // The repeater rows must be saved first, because passwords reference them.
        app.flush().await;
        for id in ["r1", "r2", "r3"] {
            let known = matches!(store.get_repeater_password(id).await, Ok(Some(_)));
            if !known {
                match store.set_repeater_password(id, "demo-password").await {
                    Ok(()) => app.with(|e| e.set_has_credentials(id, true))?,
                    Err(e) => tracing::warn!("could not store the demo password for {id}: {e:#}"),
                }
            }
        }
    }
    if let Err(e) = store.prune_history(30).await {
        tracing::warn!("pruning history failed: {e:#}");
    }

    let radio_target = env("CREWAWARE_RADIO");
    if simulate && radio_target.is_some() {
        anyhow::bail!("set either CREWAWARE_SIM or CREWAWARE_RADIO, not both");
    }

    let sim_task = simulate.then(|| {
        let secs = env("CREWAWARE_SIM_PARKED_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(900);
        app.set_mode("sim");
        tokio::spawn(sim::run(app.clone(), Duration::from_secs(secs)))
    });

    // Something has to be able to read repeaters: the simulator, or the radio gateway.
    let mut radio_tasks = Vec::new();
    let reader: Option<Arc<dyn reader::RepeaterReader>> = if simulate {
        Some(Arc::new(reader::SimReader))
    } else if let Some(target) = &radio_target {
        let (gw, rx) = crewaware_gateway::client::Gateway::start(target, Default::default())
            .map_err(|e| anyhow::anyhow!("CREWAWARE_RADIO: {e}"))?;
        tracing::info!("using the radio at {target}");
        app.set_mode("radio");
        app.set_gateway(gw.clone());
        radio_tasks.push(tokio::spawn(bridge::run(app.clone(), rx)));
        let unit_poll = env("CREWAWARE_UNIT_POLL_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        radio_tasks.push(tokio::spawn(bridge::poll_silent(
            app.clone(),
            gw.clone(),
            Duration::from_secs(unit_poll),
        )));
        Some(Arc::new(bridge::GatewayReader { app: app.clone(), gw }))
    } else {
        None
    };
    let poll_task = reader.map(|reader| {
        let secs = env("CREWAWARE_REPEATER_POLL_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(900);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        app.set_refresh_channel(tx);
        tokio::spawn(reader::poll(app.clone(), reader, rx, Duration::from_secs(secs)))
    });

    // Notice units and repeaters that go quiet (or come back) and log it once.
    let tick_task = {
        let app = app.clone();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(5));
            loop {
                iv.tick().await;
                app.with(|e| e.tick());
            }
        })
    };

    let bind = env("CREWAWARE_HTTP_BIND").unwrap_or_else(|| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!("CrewAware listening on {bind} (simulator: {simulate})");
    axum::serve(listener, api::router(app.clone()))
        .with_graceful_shutdown(async {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("signal");
                tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            {
                let _ = ctrl_c.await;
            }
        })
        .await?;

    if let Some(t) = sim_task {
        t.abort();
    }
    if let Some(t) = poll_task {
        t.abort();
    }
    tick_task.abort();
    for t in radio_tasks {
        t.abort();
    }
    app.flush().await;
    tracing::info!("shut down cleanly");
    Ok(())
}
