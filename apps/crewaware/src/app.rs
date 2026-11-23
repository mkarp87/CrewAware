use crewaware_core::{Change, Engine, Snapshot};
use crewaware_store::Store;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio::sync::{broadcast, mpsc, oneshot};

enum Msg {
    Changes(Vec<Change>),
    Flush(oneshot::Sender<()>),
}

/// Shared application state. The engine is the live source of truth; every mutation's changes
/// are queued, in order, to a single task that writes them to PostgreSQL.
pub struct App {
    engine: Mutex<Engine>,
    pub store: Arc<Store>,
    tx: mpsc::UnboundedSender<Msg>,
    notify: broadcast::Sender<()>,
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    pub cookie_secure: bool,
}

impl App {
    pub fn new(engine: Engine, store: Arc<Store>, cookie_secure: bool) -> Arc<App> {
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        let (notify, _) = broadcast::channel(16);
        let s = store.clone();
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                match m {
                    Msg::Changes(cs) => {
                        for c in cs {
                            if let Err(e) = s.apply(&c).await {
                                tracing::error!("saving change failed: {e:#}");
                            }
                        }
                    }
                    Msg::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        });
        Arc::new(App {
            engine: Mutex::new(engine),
            store,
            tx,
            notify,
            failures: Mutex::new(HashMap::new()),
            cookie_secure,
        })
    }

    /// Runs `f` on the engine, queues whatever it changed for saving, and wakes live viewers.
    /// The lock is never held across an await.
    pub fn with<R>(&self, f: impl FnOnce(&mut Engine) -> R) -> R {
        let (r, changes) = {
            let mut e = self.engine.lock().unwrap();
            let r = f(&mut e);
            (r, e.drain_changes())
        };
        if !changes.is_empty() {
            let _ = self.tx.send(Msg::Changes(changes));
            let _ = self.notify.send(());
        }
        r
    }

    /// Read-only access, for snapshots and ranking.
    pub fn read<R>(&self, f: impl FnOnce(&Engine) -> R) -> R {
        f(&self.engine.lock().unwrap())
    }

    pub fn snapshot(&self) -> Snapshot {
        self.read(|e| e.snapshot())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.notify.subscribe()
    }

    /// Waits until everything queued so far has been written.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Msg::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    // ---- login throttle: 5 failures per username in 5 minutes ----

    pub fn login_blocked(&self, user: &str) -> bool {
        let mut m = self.failures.lock().unwrap();
        match m.get(user) {
            Some((n, t)) if t.elapsed().as_secs() < 300 => *n >= 5,
            Some(_) => {
                m.remove(user);
                false
            }
            None => false,
        }
    }

    pub fn login_failed(&self, user: &str) {
        let mut m = self.failures.lock().unwrap();
        let e = m.entry(user.to_string()).or_insert((0, Instant::now()));
        if e.1.elapsed().as_secs() >= 300 {
            *e = (0, Instant::now());
        }
        e.0 += 1;
        if m.len() > 1000 {
            m.retain(|_, (_, t)| t.elapsed().as_secs() < 300);
        }
    }

    pub fn login_ok(&self, user: &str) {
        self.failures.lock().unwrap().remove(user);
    }
}
