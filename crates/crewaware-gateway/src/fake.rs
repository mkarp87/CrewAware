//! A fake MeshCore companion radio that speaks the framed protocol over any byte stream. It is
//! the test double for the gateway client and, through the `fake-radio` binary, a demo radio for
//! trying CrewAware without hardware. It models only what CrewAware uses, from the protocol
//! documentation; a real radio may differ in details this does not capture.

use crate::codec::{self, Decoder, FROM_RADIO, TO_RADIO};
use crate::proto::*;
use rand::RngCore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

fn epoch() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as u32)
        .unwrap_or(0)
}

struct FakeRepeater {
    password: String,
    status: RepeaterStatus,
    /// When false the repeater never answers anything, as if out of range.
    responding: bool,
    /// A wrong password normally gets no answer at all; set to get an explicit refusal instead.
    explicit_refusal: bool,
    logged_in: bool,
}

struct Telem {
    voltage: Option<f64>,
    gps: bool,
    responding: bool,
}

struct State {
    key: String,
    name: String,
    contacts: Vec<ContactRec>,
    repeaters: HashMap<String, FakeRepeater>,
    telemetry: HashMap<String, Telem>,
    clock_offset: i64,
    device_time: Option<u32>,
    commands: Vec<u8>,
    subscribers: Vec<mpsc::UnboundedSender<Vec<u8>>>,
    /// Pause before login and status pushes arrive, to imitate mesh latency.
    latency: Duration,
}

#[derive(Clone)]
pub struct FakeRadio {
    st: Arc<Mutex<State>>,
    kick: Arc<tokio::sync::Notify>,
}

/// A stable key derived from the node's name, so a restarted fake radio is the same radio.
fn named_key(name: &str) -> String {
    let mut k = [0u8; 32];
    let mut h: u64 = 0xcbf29ce484222325;
    for (i, chunk) in k.chunks_mut(8).enumerate() {
        for b in name.bytes().chain([i as u8]) {
            h = (h ^ b as u64).wrapping_mul(0x100000001b3);
        }
        chunk.copy_from_slice(&h.to_le_bytes());
    }
    hex(&k)
}

fn random_key() -> String {
    let mut k = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut k);
    hex(&k)
}

pub fn default_status(battery_mv: u16) -> RepeaterStatus {
    RepeaterStatus {
        battery_mv,
        tx_queue_len: 0,
        noise_floor_dbm: -112,
        last_rssi_dbm: -90,
        nb_recv: 1200,
        nb_sent: 800,
        airtime_s: 600,
        uptime_s: 3 * 86_400,
        sent_flood: 100,
        sent_direct: 200,
        recv_flood: 300,
        recv_direct: 400,
        full_evts: 0,
        last_snr_db: 5.5,
        direct_dups: 1,
        flood_dups: 2,
        rx_airtime_s: 900,
        recv_errors: Some(3),
    }
}

impl FakeRadio {
    pub fn new(name: &str) -> FakeRadio {
        FakeRadio {
            st: Arc::new(Mutex::new(State {
                key: random_key(),
                name: name.into(),
                contacts: Vec::new(),
                repeaters: HashMap::new(),
                telemetry: HashMap::new(),
                clock_offset: 0,
                device_time: None,
                commands: Vec::new(),
                subscribers: Vec::new(),
                latency: Duration::from_millis(150),
            })),
            kick: Arc::new(tokio::sync::Notify::new()),
        }
    }

    fn radio_now(&self) -> u32 {
        let st = self.st.lock().unwrap();
        (epoch() as i64 + st.clock_offset) as u32
    }

    pub fn set_latency(&self, d: Duration) {
        self.st.lock().unwrap().latency = d;
    }

    /// Makes the radio's clock wrong by this many seconds until the host sets it.
    pub fn set_clock_offset(&self, secs: i64) {
        self.st.lock().unwrap().clock_offset = secs;
    }

    pub fn device_time(&self) -> Option<u32> {
        self.st.lock().unwrap().device_time
    }

    /// Command codes received so far, in order.
    pub fn commands_seen(&self) -> Vec<u8> {
        self.st.lock().unwrap().commands.clone()
    }

    /// Adds a tracker-like contact. Returns its key. `age_secs` is how long ago it last advertised.
    pub fn add_unit(&self, name: &str, lat: f64, lon: f64, age_secs: u32) -> String {
        let key = named_key(name);
        let now = self.radio_now();
        self.st.lock().unwrap().contacts.push(ContactRec {
            key: key.clone(),
            kind: ADV_TYPE_CHAT,
            name: name.into(),
            lat,
            lon,
            last_advert: now.saturating_sub(age_secs),
            lastmod: now.saturating_sub(age_secs),
        });
        key
    }

    pub fn add_repeater(&self, name: &str, password: &str, lat: f64, lon: f64, battery_mv: u16) -> String {
        let key = named_key(name);
        let now = self.radio_now();
        let mut st = self.st.lock().unwrap();
        st.contacts.push(ContactRec {
            key: key.clone(),
            kind: ADV_TYPE_REPEATER,
            name: name.into(),
            lat,
            lon,
            last_advert: now,
            lastmod: now,
        });
        st.repeaters.insert(
            key.clone(),
            FakeRepeater {
                password: password.into(),
                status: default_status(battery_mv),
                responding: true,
                explicit_refusal: false,
                logged_in: false,
            },
        );
        key
    }

    pub fn set_repeater_responding(&self, key: &str, on: bool) {
        if let Some(r) = self.st.lock().unwrap().repeaters.get_mut(key) {
            r.responding = on;
        }
    }

    pub fn set_repeater_refuses_explicitly(&self, key: &str, on: bool) {
        if let Some(r) = self.st.lock().unwrap().repeaters.get_mut(key) {
            r.explicit_refusal = on;
        }
    }

    pub fn set_repeater_status(&self, key: &str, f: impl FnOnce(&mut RepeaterStatus)) {
        if let Some(r) = self.st.lock().unwrap().repeaters.get_mut(key) {
            f(&mut r.status);
        }
    }

    /// Lets a node answer telemetry requests.
    pub fn set_telemetry(&self, key: &str, voltage: Option<f64>, gps: bool, responding: bool) {
        self.st.lock().unwrap().telemetry.insert(
            key.into(),
            Telem {
                voltage,
                gps,
                responding,
            },
        );
    }

    /// Moves a node and has it advertise: the contact changes on the radio and the host is
    /// told with an advert push, as it would be for a real advert heard over the air.
    pub fn advert(&self, key: &str, pos: Option<(f64, f64)>) {
        let now = self.radio_now();
        let mut st = self.st.lock().unwrap();
        let Some(c) = st.contacts.iter_mut().find(|c| c.key == key) else {
            return;
        };
        if let Some((lat, lon)) = pos {
            c.lat = lat;
            c.lon = lon;
        }
        c.last_advert = now;
        c.lastmod = now;
        let kb = from_hex(key).unwrap_or_default();
        let mut push = vec![PUSH_ADVERT];
        push.extend_from_slice(&kb);
        st.subscribers.retain(|s| s.send(push.clone()).is_ok());
    }

    pub fn position_of(&self, key: &str) -> Option<(f64, f64)> {
        self.st
            .lock()
            .unwrap()
            .contacts
            .iter()
            .find(|c| c.key == key)
            .map(|c| (c.lat, c.lon))
    }

    /// Serves one host connection until it closes.
    pub async fn serve<IO: AsyncRead + AsyncWrite + Unpin + Send + 'static>(&self, io: IO) {
        let (mut rd, mut wr) = tokio::io::split(io);
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.st.lock().unwrap().subscribers.push(out_tx.clone());
        let writer = tokio::spawn(async move {
            while let Some(f) = out_rx.recv().await {
                if wr.write_all(&codec::encode(FROM_RADIO, &f)).await.is_err() {
                    break;
                }
                let _ = wr.flush().await;
            }
        });
        let mut dec = Decoder::new(TO_RADIO);
        let mut buf = [0u8; 512];
        loop {
            let read = tokio::select! {
                r = rd.read(&mut buf) => r,
                _ = self.kick.notified() => break,
            };
            match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for f in dec.push(&buf[..n]) {
                        self.handle(&f, &out_tx);
                    }
                }
            }
        }
        writer.abort();
    }

    /// Drops the current host connection, as if the USB cable were pulled.
    pub fn disconnect(&self) {
        self.kick.notify_waiters();
    }

    pub async fn listen(&self, addr: &str) -> std::io::Result<()> {
        self.serve_listener(tokio::net::TcpListener::bind(addr).await?).await
    }

    /// Accepts TCP connections one after another, like a radio with a single host.
    pub async fn serve_listener(&self, l: tokio::net::TcpListener) -> std::io::Result<()> {
        loop {
            let (s, _) = l.accept().await?;
            self.serve(s).await;
        }
    }

    fn handle(&self, f: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
        let send = |v: Vec<u8>| {
            let _ = out.send(v);
        };
        let code = f[0];
        let (latency, name, key) = {
            let mut st = self.st.lock().unwrap();
            st.commands.push(code);
            (st.latency, st.name.clone(), st.key.clone())
        };
        match code {
            CMD_APP_START => send(encode_self_info(&key, &name)),
            CMD_DEVICE_QUERY => send(encode_device_info("Fake Radio", "v1.9.0-fake")),
            CMD_SET_DEVICE_TIME if f.len() >= 5 => {
                let t = u32::from_le_bytes([f[1], f[2], f[3], f[4]]);
                let mut st = self.st.lock().unwrap();
                st.device_time = Some(t);
                st.clock_offset = t as i64 - epoch() as i64;
                send(vec![RESP_OK]);
            }
            CMD_SEND_SELF_ADVERT => send(vec![RESP_OK]),
            CMD_GET_CONTACTS => {
                let since = (f.len() >= 5).then(|| u32::from_le_bytes([f[1], f[2], f[3], f[4]]));
                let list: Vec<ContactRec> = self
                    .st
                    .lock()
                    .unwrap()
                    .contacts
                    .iter()
                    .filter(|c| since.is_none_or(|s| c.lastmod > s))
                    .cloned()
                    .collect();
                let mut start = vec![RESP_CONTACTS_START];
                start.extend_from_slice(&(list.len() as u32).to_le_bytes());
                send(start);
                let mut newest = 0;
                for c in &list {
                    newest = newest.max(c.lastmod);
                    send(encode_contact(RESP_CONTACT, c));
                }
                let mut end = vec![RESP_END_OF_CONTACTS];
                end.extend_from_slice(&newest.to_le_bytes());
                send(end);
            }
            CMD_SEND_LOGIN if f.len() >= 33 => {
                send(encode_sent(3000));
                let target = f[1..33].to_vec();
                let pw = String::from_utf8_lossy(&f[33..]).into_owned();
                let k = hex(&target);
                let reply = {
                    let mut st = self.st.lock().unwrap();
                    match st.repeaters.get_mut(&k) {
                        Some(r) if r.responding => {
                            if r.password == pw {
                                r.logged_in = true;
                                Some(encode_login(PUSH_LOGIN_SUCCESS, &target, true))
                            } else if r.explicit_refusal {
                                Some(encode_login(PUSH_LOGIN_FAIL, &target, false))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    }
                };
                if let Some(r) = reply {
                    let out = out.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(latency).await;
                        let _ = out.send(r);
                    });
                }
            }
            CMD_SEND_STATUS_REQ if f.len() >= 33 => {
                send(encode_sent(3000));
                let target = f[1..33].to_vec();
                let reply = {
                    let st = self.st.lock().unwrap();
                    st.repeaters
                        .get(&hex(&target))
                        .filter(|r| r.responding && r.logged_in)
                        .map(|r| encode_status(&target, &r.status))
                };
                if let Some(r) = reply {
                    let out = out.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(latency).await;
                        let _ = out.send(r);
                    });
                }
            }
            CMD_SEND_TELEMETRY_REQ if f.len() >= 36 => {
                send(encode_sent(3000));
                let target = f[4..36].to_vec();
                let k = hex(&target);
                let reply = {
                    let st = self.st.lock().unwrap();
                    st.telemetry.get(&k).filter(|t| t.responding).map(|t| {
                        let pos = if t.gps {
                            st.contacts.iter().find(|c| c.key == k).map(|c| (c.lat, c.lon))
                        } else {
                            None
                        };
                        encode_telemetry(&target, &encode_lpp(t.voltage, pos))
                    })
                };
                if let Some(r) = reply {
                    let out = out.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(latency).await;
                        let _ = out.send(r);
                    });
                }
            }
            _ => send(vec![RESP_ERR, 1]),
        }
    }
}
