mod api;
mod app;
mod sim;

use anyhow::{Context, Result};
use crewaware_core::Engine;
use crewaware_store::Store;
use std::sync::Arc;
use std::time::Duration;

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

async fn connect() -> Result<Store> {
    let url = env("DATABASE_URL").context("DATABASE_URL is not set")?;
    // The database container may still be starting.
    let mut last = None;
    for _ in 0..30 {
        match Store::connect(&url).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = Some(e);
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(last.unwrap()).context("could not reach the database")
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
        let store = connect().await?;
        store.migrate().await?;
        store
            .set_admin_password(user, pw.trim_end_matches(['\r', '\n']))
            .await?;
        println!("password set for {user}; existing sessions ended");
        return Ok(());
    }

    let store = Arc::new(connect().await?);
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

    let simulate = env("CREWAWARE_SIM").is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));
    let engine = Engine::new(store.load().await.context("loading state")?);
    let secure = env("CREWAWARE_COOKIE_SECURE").is_some_and(|v| v == "1");
    let app = app::App::new(engine, store.clone(), secure);
    if simulate {
        app.with(|e| e.seed_demo());
    }
    if let Err(e) = store.prune_history(30).await {
        tracing::warn!("pruning history failed: {e:#}");
    }

    let sim_task = simulate.then(|| {
        let secs = env("CREWAWARE_SIM_PARKED_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(900);
        tokio::spawn(sim::run(app.clone(), Duration::from_secs(secs)))
    });

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
    app.flush().await;
    tracing::info!("shut down cleanly");
    Ok(())
}
