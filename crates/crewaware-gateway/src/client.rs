//! The gateway client: keeps a connection to the companion radio, mirrors its contact list as
//! events, and reads repeaters (log in, ask for status) on request.

use crate::codec::{self, Decoder, FROM_RADIO, TO_RADIO};
use crate::proto::{self, ContactRec, Incoming, RepeaterStatus, Telemetry};
use crate::transport::{self, BoxIo, Target};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio::time::timeout;

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum GwError {
    #[error("radio not connected")]
    NotConnected,
    #[error("radio disconnected")]
    Disconnected,
    #[error("radio did not answer ({0})")]
    Timeout(&'static str),
    #[error("radio refused the command (error {0})")]
    Radio(u8),
    #[error("{0}")]
    Other(String),
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RadioStatus {
    pub connected: bool,
    pub name: String,
    pub key: String,
    pub model: String,
    pub version: String,
    pub contacts: usize,
    /// Unix seconds of the last completed contact sync.
    pub last_sync: Option<u64>,
    /// Why the radio is not connected, or the last problem seen.
    pub error: String,
}

#[derive(Clone, Debug)]
pub enum Event {
    Status(RadioStatus),
    /// A contact is new or changed on the radio. `age_secs` is how long ago the radio recorded
    /// the change, or None if its clock is not trustworthy.
    Contact {
        rec: ContactRec,
        age_secs: Option<u64>,
    },
}

#[derive(Clone, Copy, PartialEq)]
enum WaitKind {
    Login,
    Status,
    Telemetry,
}

struct PushWaiter {
    kind: WaitKind,
    prefix: String,
    tx: oneshot::Sender<Incoming>,
}

pub struct Options {
    /// How often to re-read the contact list even without an advert push.
    pub resync: Duration,
    /// How long to wait for the radio to answer a command.
    pub command_timeout: Duration,
    pub reconnect: Duration,
    /// Bounds on how long to wait for a login or status answer to arrive over the mesh.
    pub mesh_wait_min: Duration,
    pub mesh_wait_max: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            resync: Duration::from_secs(30),
            command_timeout: Duration::from_secs(5),
            reconnect: Duration::from_secs(5),
            mesh_wait_min: Duration::from_secs(10),
            mesh_wait_max: Duration::from_secs(40),
        }
    }
}

struct Inner {
    target: Target,
    opts: Options,
    link: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    waiter: Mutex<Option<mpsc::UnboundedSender<Incoming>>>,
    push_waiters: Mutex<Vec<PushWaiter>>,
    cmd_lock: tokio::sync::Mutex<()>,
    advert: Notify,
    status: Mutex<RadioStatus>,
    events: mpsc::UnboundedSender<Event>,
    /// Last lastmod emitted per contact, so unchanged contacts are not re-reported.
    emitted: Mutex<HashMap<String, u32>>,
}

#[derive(Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// How old a contact change is, by the radio's clock, if the clock looks sane.
pub fn contact_age(lastmod: u32, now: u64) -> Option<u64> {
    let t = lastmod as u64;
    if t < 1_600_000_000 || t > now + 120 {
        None
    } else {
        Some(now.saturating_sub(t))
    }
}

fn key32(hex: &str) -> Result<[u8; 32], GwError> {
    let b = proto::from_hex(hex).ok_or_else(|| GwError::Other("node key is not hex".into()))?;
    b.try_into()
        .map_err(|_| GwError::Other("node key must be 64 hex characters".into()))
}

impl Gateway {
    /// Starts the connection supervisor. Events (status changes and contact updates) arrive on
    /// the returned receiver.
    pub fn start(target: &str, opts: Options) -> Result<(Gateway, mpsc::UnboundedReceiver<Event>), String> {
        let target = transport::parse_target(target)?;
        let (events, rx) = mpsc::unbounded_channel();
        let g = Gateway {
            inner: Arc::new(Inner {
                target,
                opts,
                link: Mutex::new(None),
                waiter: Mutex::new(None),
                push_waiters: Mutex::new(Vec::new()),
                cmd_lock: tokio::sync::Mutex::new(()),
                advert: Notify::new(),
                status: Mutex::new(RadioStatus::default()),
                events,
                emitted: Mutex::new(HashMap::new()),
            }),
        };
        tokio::spawn(g.clone().supervise());
        Ok((g, rx))
    }

    pub fn status(&self) -> RadioStatus {
        self.inner.status.lock().unwrap().clone()
    }

    fn update_status(&self, f: impl FnOnce(&mut RadioStatus)) {
        let s = {
            let mut s = self.inner.status.lock().unwrap();
            f(&mut s);
            s.clone()
        };
        let _ = self.inner.events.send(Event::Status(s));
    }

    async fn supervise(self) {
        loop {
            let why = match transport::open(&self.inner.target).await {
                Ok(io) => self.session(io).await,
                Err(e) => format!("cannot open the radio: {e}"),
            };
            tracing::warn!("radio: {why}");
            self.inner.link.lock().unwrap().take();
            self.inner.waiter.lock().unwrap().take();
            self.inner.push_waiters.lock().unwrap().clear();
            self.update_status(|s| {
                s.connected = false;
                s.error = why;
            });
            tokio::time::sleep(self.inner.opts.reconnect).await;
        }
    }

    async fn session(&self, io: BoxIo) -> String {
        let (mut rd, mut wr) = tokio::io::split(io);
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
        *self.inner.link.lock().unwrap() = Some(tx);
        let writer = tokio::spawn(async move {
            while let Some(f) = rx.recv().await {
                if wr.write_all(&codec::encode(TO_RADIO, &f)).await.is_err() {
                    break;
                }
                let _ = wr.flush().await;
            }
        });
        let me = self.clone();
        let mut reader = tokio::spawn(async move {
            let mut dec = Decoder::new(FROM_RADIO);
            let mut buf = [0u8; 512];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => {
                        for f in dec.push(&buf[..n]) {
                            if let Some(m) = proto::parse(&f) {
                                me.dispatch(m);
                            }
                        }
                    }
                }
            }
        });
        let reason = tokio::select! {
            r = self.drive() => r,
            _ = &mut reader => "the radio closed the connection".to_string(),
        };
        writer.abort();
        reader.abort();
        reason
    }

    fn dispatch(&self, m: Incoming) {
        match &m {
            Incoming::Ok
            | Incoming::Err(_)
            | Incoming::ContactsStart(_)
            | Incoming::Contact(_)
            | Incoming::EndOfContacts(_)
            | Incoming::SelfInfo(_)
            | Incoming::Sent { .. }
            | Incoming::DeviceInfo { .. } => {
                if let Some(w) = self.inner.waiter.lock().unwrap().as_ref() {
                    let _ = w.send(m);
                }
            }
            Incoming::LoginSuccess { prefix, .. } | Incoming::LoginFail { prefix } => {
                self.resolve(WaitKind::Login, prefix, m.clone())
            }
            Incoming::Status { prefix, .. } => self.resolve(WaitKind::Status, prefix, m.clone()),
            Incoming::Telemetry { prefix, .. } => self.resolve(WaitKind::Telemetry, prefix, m.clone()),
            Incoming::Advert { .. } => self.inner.advert.notify_one(),
            Incoming::NewAdvert(rec) => {
                self.emit_contact(rec.clone());
            }
            Incoming::Other(_) => {}
        }
    }

    fn resolve(&self, kind: WaitKind, prefix: &str, m: Incoming) {
        let mut ws = self.inner.push_waiters.lock().unwrap();
        if let Some(i) = ws.iter().position(|w| w.kind == kind && w.prefix == prefix) {
            let w = ws.remove(i);
            let _ = w.tx.send(m);
        } else {
            tracing::debug!("radio: unexpected push for {prefix}");
        }
    }

    fn wait_push(&self, kind: WaitKind, prefix: &str) -> oneshot::Receiver<Incoming> {
        let (tx, rx) = oneshot::channel();
        let mut ws = self.inner.push_waiters.lock().unwrap();
        ws.retain(|w| !w.tx.is_closed());
        ws.push(PushWaiter {
            kind,
            prefix: prefix.into(),
            tx,
        });
        rx
    }

    fn emit_contact(&self, rec: ContactRec) {
        {
            let mut seen = self.inner.emitted.lock().unwrap();
            if seen.get(&rec.key) == Some(&rec.lastmod) {
                return;
            }
            seen.insert(rec.key.clone(), rec.lastmod);
        }
        let age_secs = contact_age(rec.lastmod, now_epoch());
        let _ = self.inner.events.send(Event::Contact { rec, age_secs });
    }

    /// Sends one command and collects the radio's answer, up to and including the first frame
    /// `done` accepts. A radio error frame ends the exchange as an error.
    async fn exchange(&self, frame: Vec<u8>, done: impl Fn(&Incoming) -> bool) -> Result<Vec<Incoming>, GwError> {
        let _guard = self.inner.cmd_lock.lock().await;
        let tx = self.inner.link.lock().unwrap().clone().ok_or(GwError::NotConnected)?;
        let (otx, mut orx) = mpsc::unbounded_channel();
        *self.inner.waiter.lock().unwrap() = Some(otx);
        let result = async {
            tx.send(frame).await.map_err(|_| GwError::Disconnected)?;
            let mut out = Vec::new();
            loop {
                let m = timeout(self.inner.opts.command_timeout, orx.recv())
                    .await
                    .map_err(|_| GwError::Timeout("command"))?
                    .ok_or(GwError::Disconnected)?;
                if let Incoming::Err(c) = m {
                    return Err(GwError::Radio(c));
                }
                let fin = done(&m);
                out.push(m);
                if fin {
                    return Ok(out);
                }
            }
        }
        .await;
        self.inner.waiter.lock().unwrap().take();
        result
    }

    async fn simple(&self, frame: Vec<u8>) -> Result<Incoming, GwError> {
        Ok(self.exchange(frame, |_| true).await?.remove(0))
    }

    /// Handshake, then keep the contact list in step with the radio. Returns why it stopped.
    async fn drive(&self) -> String {
        match self.drive_inner().await {
            Ok(()) => "stopped".into(),
            Err(e) => e.to_string(),
        }
    }

    async fn drive_inner(&self) -> Result<(), GwError> {
        let me = match self.simple(proto::app_start("CrewAware")).await? {
            Incoming::SelfInfo(i) => i,
            other => return Err(GwError::Other(format!("unexpected answer to app start: {other:?}"))),
        };
        let (model, version) = match self.simple(proto::device_query()).await {
            Ok(Incoming::DeviceInfo { model, version, .. }) => (model, version),
            _ => (String::new(), String::new()),
        };
        // The radio has no battery-backed clock. Contact ages depend on it being right.
        if let Err(e) = self.simple(proto::set_device_time(now_epoch() as u32)).await {
            tracing::warn!("radio: could not set the clock: {e}");
        }
        self.inner.emitted.lock().unwrap().clear();
        self.update_status(|s| {
            s.connected = true;
            s.error.clear();
            s.name = me.name.clone();
            s.key = me.key.clone();
            s.model = model;
            s.version = version;
        });
        tracing::info!("radio connected: {} ({})", me.name, &me.key[..12]);

        let mut since: Option<u32> = None;
        let mut last_full = std::time::Instant::now();
        loop {
            let full = since.is_none() || last_full.elapsed() > Duration::from_secs(600);
            let q = if full { None } else { since };
            let frames = self
                .exchange(proto::get_contacts(q), |m| matches!(m, Incoming::EndOfContacts(_)))
                .await?;
            let mut newest = since.unwrap_or(0);
            let mut n = 0usize;
            for f in frames {
                match f {
                    Incoming::Contact(rec) => {
                        n += 1;
                        newest = newest.max(rec.lastmod);
                        self.emit_contact(rec);
                    }
                    Incoming::EndOfContacts(m) => newest = newest.max(m),
                    _ => {}
                }
            }
            if full {
                last_full = std::time::Instant::now();
                self.update_status(|s| s.contacts = n);
            }
            // One second back, because the radio's `since` comparison is strict and two changes can
            // share a second. Duplicates are filtered by `emit_contact`.
            since = Some(newest.saturating_sub(1));
            self.update_status(|s| s.last_sync = Some(now_epoch()));
            tokio::select! {
                _ = tokio::time::sleep(self.inner.opts.resync) => {}
                _ = self.inner.advert.notified() => {
                    // Let a burst of adverts land before asking.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            }
        }
    }

    /// Asks the radio to broadcast its own advert.
    pub async fn send_advert(&self) -> Result<(), GwError> {
        self.simple(proto::send_self_advert(true)).await.map(|_| ())
    }

    async fn sent_timeout(&self, frame: Vec<u8>) -> Result<Duration, GwError> {
        match self.simple(frame).await? {
            Incoming::Sent { timeout_ms } => {
                // The radio's estimate is for one hop; allow for retries but not forever.
                let o = &self.inner.opts;
                Ok(Duration::from_millis(timeout_ms as u64 * 2).clamp(o.mesh_wait_min, o.mesh_wait_max))
            }
            other => Err(GwError::Other(format!("unexpected answer: {other:?}"))),
        }
    }

    /// Logs in to a repeater with its password and reads its status. Read-only: nothing is
    /// written to the repeater.
    pub async fn read_repeater(&self, key_hex: &str, password: &str) -> Result<RepeaterStatus, GwError> {
        let key = key32(key_hex)?;
        let prefix = proto::hex(&key[..6]);
        let login = self.wait_push(WaitKind::Login, &prefix);
        let wait = self.sent_timeout(proto::send_login(&key, password)).await?;
        match timeout(wait, login).await {
            Ok(Ok(Incoming::LoginSuccess { .. })) => {}
            Ok(Ok(Incoming::LoginFail { .. })) => return Err(GwError::Other("login refused (wrong password?)".into())),
            Ok(_) => return Err(GwError::Disconnected),
            Err(_) => return Err(GwError::Timeout("login; repeater out of range or password wrong")),
        }
        let status = self.wait_push(WaitKind::Status, &prefix);
        let wait = self.sent_timeout(proto::send_status_req(&key)).await?;
        match timeout(wait, status).await {
            Ok(Ok(Incoming::Status { status, .. })) => Ok(status),
            Ok(_) => Err(GwError::Disconnected),
            Err(_) => Err(GwError::Timeout("status")),
        }
    }

    /// Asks a node for its telemetry (battery, and GPS if it reports one).
    pub async fn request_telemetry(&self, key_hex: &str) -> Result<Telemetry, GwError> {
        let key = key32(key_hex)?;
        let prefix = proto::hex(&key[..6]);
        let w = self.wait_push(WaitKind::Telemetry, &prefix);
        let wait = self.sent_timeout(proto::send_telemetry_req(&key)).await?;
        match timeout(wait, w).await {
            Ok(Ok(Incoming::Telemetry { lpp, .. })) => Ok(proto::parse_lpp(&lpp)),
            Ok(_) => Err(GwError::Disconnected),
            Err(_) => Err(GwError::Timeout("telemetry")),
        }
    }
}
