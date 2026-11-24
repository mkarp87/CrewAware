use crewaware_core::{Change, Engine, Snapshot};
use crewaware_gateway::client::{Gateway, RadioStatus};
use crewaware_store::Store;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
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
    /// Set once a repeater reader (the radio gateway or the simulator) is running.
    refresh_tx: OnceLock<mpsc::UnboundedSender<String>>,
    last_refresh: Mutex<HashMap<String, Instant>>,
    radio: Mutex<Radio>,
    gateway: OnceLock<Gateway>,
}

/// A node the radio has heard that is not yet a unit or repeater.
#[derive(Clone)]
pub struct Discovered {
    pub key: String,
    pub name: String,
    /// "unit", "repeater" or "room".
    pub kind: &'static str,
    pub lat: f64,
    pub lon: f64,
    pub has_pos: bool,
    pub heard: Instant,
}

#[derive(Default)]
struct Radio {
    /// "none", "sim" or "radio".
    mode: &'static str,
    status: Option<RadioStatus>,
    discovered: HashMap<String, Discovered>,
}

#[derive(Serialize)]
pub struct DiscoveredView {
    key: String,
    name: String,
    kind: &'static str,
    lat: f64,
    lon: f64,
    has_pos: bool,
    age_sec: f64,
}

#[derive(Serialize)]
pub struct RadioView {
    mode: &'static str,
    connected: bool,
    name: String,
    key: String,
    model: String,
    version: String,
    contacts: usize,
    /// Seconds since the last contact sync, if any.
    sync_age_sec: Option<f64>,
    error: String,
    discovered: Vec<DiscoveredView>,
}

/// What /api/state and the live stream carry: the engine snapshot plus the radio's state.
#[derive(Serialize)]
pub struct StateView {
    #[serde(flatten)]
    pub snap: Snapshot,
    pub radio: RadioView,
}

/// Why a manual repeater read was not queued.
pub enum RefreshError {
    NoGateway,
    TooSoon(u64),
}

/// Minimum time between manual reads of the same repeater, to protect shared airtime.
pub const REFRESH_COOLDOWN_SECS: u64 = 60;

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
            refresh_tx: OnceLock::new(),
            last_refresh: Mutex::new(HashMap::new()),
            radio: Mutex::new(Radio::default()),
            gateway: OnceLock::new(),
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

    pub fn set_mode(&self, mode: &'static str) {
        self.radio.lock().unwrap().mode = mode;
    }

    pub fn set_gateway(&self, g: Gateway) {
        let _ = self.gateway.set(g);
    }

    pub fn gateway(&self) -> Option<&Gateway> {
        self.gateway.get()
    }

    pub fn set_radio_status(&self, s: RadioStatus) {
        self.radio.lock().unwrap().status = Some(s);
        self.touch();
    }

    /// Remembers a node the radio heard that nobody has claimed yet, so an admin can adopt it.
    pub fn note_discovered(&self, d: Discovered) {
        let mut r = self.radio.lock().unwrap();
        let new = !r.discovered.contains_key(&d.key);
        r.discovered.insert(d.key.clone(), d);
        if r.discovered.len() > 500 {
            // A busy mesh: keep the most recently heard.
            let mut v: Vec<_> = r.discovered.values().map(|x| (x.heard, x.key.clone())).collect();
            v.sort();
            for (_, k) in v.into_iter().take(100) {
                r.discovered.remove(&k);
            }
        }
        drop(r);
        if new {
            self.touch();
        }
    }

    /// What the radio last told us about a node nobody has claimed yet.
    pub fn discovered(&self, key: &str) -> Option<Discovered> {
        self.radio.lock().unwrap().discovered.get(key).cloned()
    }

    /// The snapshot plus the radio's state; discovered nodes that have since been adopted are left out.
    pub fn view(&self) -> StateView {
        let snap = self.snapshot();
        let claimed: HashSet<&str> = snap
            .units
            .iter()
            .map(|u| u.unit.node_key.as_str())
            .chain(snap.repeaters.iter().map(|r| r.repeater.node_key.as_str()))
            .filter(|k| !k.is_empty())
            .collect();
        let r = self.radio.lock().unwrap();
        let mut discovered: Vec<DiscoveredView> = r
            .discovered
            .values()
            .filter(|d| !claimed.contains(d.key.as_str()))
            .map(|d| DiscoveredView {
                key: d.key.clone(),
                name: d.name.clone(),
                kind: d.kind,
                lat: d.lat,
                lon: d.lon,
                has_pos: d.has_pos,
                age_sec: d.heard.elapsed().as_secs_f64(),
            })
            .collect();
        discovered.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then(a.key.cmp(&b.key))
        });
        let st = r.status.clone().unwrap_or_default();
        let now = crewaware_gateway::client::now_epoch();
        let radio = RadioView {
            mode: r.mode,
            connected: st.connected,
            name: st.name,
            key: st.key,
            model: st.model,
            version: st.version,
            contacts: st.contacts,
            sync_age_sec: st.last_sync.map(|t| now.saturating_sub(t) as f64),
            error: st.error,
            discovered,
        };
        StateView { snap, radio }
    }

    /// Wakes live viewers without changing the engine, for changes made outside it.
    pub fn touch(&self) {
        let _ = self.notify.send(());
    }

    pub fn set_refresh_channel(&self, tx: mpsc::UnboundedSender<String>) {
        let _ = self.refresh_tx.set(tx);
    }

    pub fn request_refresh(&self, repeater_id: &str) -> Result<(), RefreshError> {
        let tx = self.refresh_tx.get().ok_or(RefreshError::NoGateway)?;
        let mut m = self.last_refresh.lock().unwrap();
        if let Some(t) = m.get(repeater_id) {
            let el = t.elapsed().as_secs();
            if el < REFRESH_COOLDOWN_SECS {
                return Err(RefreshError::TooSoon(REFRESH_COOLDOWN_SECS - el));
            }
        }
        m.insert(repeater_id.to_string(), Instant::now());
        let _ = tx.send(repeater_id.to_string());
        Ok(())
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
