use crate::app::App;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub type ReadFuture<'a> = Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + 'a>>;

/// Reads a repeater over the mesh: log in with its password, then ask for its status. The real
/// implementation belongs to the MeshCore USB gateway and is not written yet; the simulator
/// implements this for development.
pub trait RepeaterReader: Send + Sync {
    fn read<'a>(&'a self, repeater_id: &'a str, password: &'a str) -> ReadFuture<'a>;
}

/// Fake reader: answers after a short delay, and never answers for repeater `r3`.
pub struct SimReader;

impl RepeaterReader for SimReader {
    fn read<'a>(&'a self, repeater_id: &'a str, _password: &'a str) -> ReadFuture<'a> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_millis(800)).await;
            if repeater_id == "r3" {
                return Err("no response (timed out)".into());
            }
            let n = (rand::random::<u32>() % 200) as i64;
            Ok(serde_json::json!({
                "battery_mv": 3900 + n,
                "uptime_s": 86_400 + n * 60,
                "noise_floor_dbm": -115 + (n % 8),
                "last_rssi_dbm": -90 + (n % 20),
                "last_snr_db": 4 + (n % 6),
                "tx_airtime_s": 1200 + n,
            }))
        })
    }
}

/// How long one login-and-status attempt may take. The mesh is slow and multi-hop.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Reads repeaters one at a time: every `every` for all repeaters with a stored password, plus
/// any a dispatcher or admin asks for. One at a time keeps repeater traffic from crowding out
/// position reports on the shared channel.
pub async fn poll(
    app: Arc<App>,
    reader: Arc<dyn RepeaterReader>,
    mut rx: mpsc::UnboundedReceiver<String>,
    every: Duration,
) {
    let mut tick = tokio::time::interval(every);
    loop {
        let ids: Vec<String> = tokio::select! {
            _ = tick.tick() => app.read(|e| e.credentialed_repeaters()),
            Some(id) = rx.recv() => vec![id],
            else => return,
        };
        for id in ids {
            read_one(&app, reader.as_ref(), &id).await;
        }
    }
}

async fn read_one(app: &App, reader: &dyn RepeaterReader, id: &str) {
    let pw = match app.store.get_repeater_password(id).await {
        Ok(Some(p)) => p,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("repeater {id}: {e:#}");
            let msg = e.to_string();
            let _ = app.with(|e| e.record_reading(id, false, &msg, serde_json::Value::Null));
            return;
        }
    };
    let result = match tokio::time::timeout(READ_TIMEOUT, reader.read(id, &pw)).await {
        Ok(r) => r,
        Err(_) => Err("no response (timed out)".into()),
    };
    let _ = app.with(|e| match result {
        Ok(data) => e.record_reading(id, true, "", data),
        Err(msg) => e.record_reading(id, false, &msg, serde_json::Value::Null),
    });
}
