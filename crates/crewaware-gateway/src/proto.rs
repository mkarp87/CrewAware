//! MeshCore companion-protocol messages, written from the public protocol documentation
//! (docs.meshcore.io/companion_protocol, the MeshCore wiki and the meshcore_py reference client).
//! It has been exercised against the fake radio in this crate, not against real hardware: the
//! places where the documentation is thin are marked UNVERIFIED.

use serde::Serialize;

// Commands (host to radio).
pub const CMD_APP_START: u8 = 1;
pub const CMD_SET_DEVICE_TIME: u8 = 6;
pub const CMD_SEND_SELF_ADVERT: u8 = 7;
pub const CMD_GET_CONTACTS: u8 = 4;
pub const CMD_DEVICE_QUERY: u8 = 22;
pub const CMD_SEND_LOGIN: u8 = 26;
pub const CMD_SEND_STATUS_REQ: u8 = 27;
pub const CMD_SEND_TELEMETRY_REQ: u8 = 39;

// Responses (radio to host, code < 0x80).
pub const RESP_OK: u8 = 0;
pub const RESP_ERR: u8 = 1;
pub const RESP_CONTACTS_START: u8 = 2;
pub const RESP_CONTACT: u8 = 3;
pub const RESP_END_OF_CONTACTS: u8 = 4;
pub const RESP_SELF_INFO: u8 = 5;
pub const RESP_SENT: u8 = 6;
pub const RESP_DEVICE_INFO: u8 = 13;

// Pushes (radio to host, code >= 0x80).
pub const PUSH_ADVERT: u8 = 0x80;
pub const PUSH_PATH_UPDATED: u8 = 0x81;
pub const PUSH_MSG_WAITING: u8 = 0x83;
pub const PUSH_LOGIN_SUCCESS: u8 = 0x85;
pub const PUSH_LOGIN_FAIL: u8 = 0x86;
pub const PUSH_STATUS_RESPONSE: u8 = 0x87;
pub const PUSH_NEW_ADVERT: u8 = 0x8A;
pub const PUSH_TELEMETRY_RESPONSE: u8 = 0x8B;

pub const ADV_TYPE_CHAT: u8 = 1;
pub const ADV_TYPE_REPEATER: u8 = 2;
pub const ADV_TYPE_ROOM: u8 = 3;
pub const ADV_TYPE_SENSOR: u8 = 4;

/// Longest repeater password the radio will send.
pub const MAX_PASSWORD: usize = 15;

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn from_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ---- commands ----

pub fn app_start(app_name: &str) -> Vec<u8> {
    // code, app version, six reserved bytes, then the app name.
    let mut v = vec![CMD_APP_START, 3, b' ', b' ', b' ', b' ', b' ', b' '];
    v.extend_from_slice(app_name.as_bytes());
    v
}

pub fn device_query() -> Vec<u8> {
    vec![CMD_DEVICE_QUERY, 3]
}

pub fn set_device_time(epoch: u32) -> Vec<u8> {
    let mut v = vec![CMD_SET_DEVICE_TIME];
    v.extend_from_slice(&epoch.to_le_bytes());
    v
}

pub fn get_contacts(since: Option<u32>) -> Vec<u8> {
    let mut v = vec![CMD_GET_CONTACTS];
    if let Some(s) = since {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

pub fn send_self_advert(flood: bool) -> Vec<u8> {
    vec![CMD_SEND_SELF_ADVERT, flood as u8]
}

pub fn send_login(key: &[u8; 32], password: &str) -> Vec<u8> {
    let mut v = vec![CMD_SEND_LOGIN];
    v.extend_from_slice(key);
    v.extend_from_slice(&password.as_bytes()[..password.len().min(MAX_PASSWORD)]);
    v
}

pub fn send_status_req(key: &[u8; 32]) -> Vec<u8> {
    let mut v = vec![CMD_SEND_STATUS_REQ];
    v.extend_from_slice(key);
    v
}

pub fn send_telemetry_req(key: &[u8; 32]) -> Vec<u8> {
    let mut v = vec![CMD_SEND_TELEMETRY_REQ, 0, 0, 0];
    v.extend_from_slice(key);
    v
}

// ---- parsed messages ----

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContactRec {
    pub key: String,
    pub kind: u8,
    pub name: String,
    /// Degrees; both zero when the node has not advertised a position.
    pub lat: f64,
    pub lon: f64,
    pub last_advert: u32,
    /// When the radio last changed this contact, by the radio's clock.
    pub lastmod: u32,
}

impl ContactRec {
    pub fn has_position(&self) -> bool {
        !(self.lat == 0.0 && self.lon == 0.0)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelfInfo {
    pub key: String,
    pub name: String,
}

/// Repeater statistics, as returned for a status request.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RepeaterStatus {
    pub battery_mv: u16,
    pub tx_queue_len: u16,
    pub noise_floor_dbm: i16,
    pub last_rssi_dbm: i16,
    pub nb_recv: u32,
    pub nb_sent: u32,
    pub airtime_s: u32,
    pub uptime_s: u32,
    pub sent_flood: u32,
    pub sent_direct: u32,
    pub recv_flood: u32,
    pub recv_direct: u32,
    pub full_evts: u16,
    pub last_snr_db: f64,
    pub direct_dups: u16,
    pub flood_dups: u16,
    pub rx_airtime_s: u32,
    pub recv_errors: Option<u32>,
}

pub const STATUS_MIN_LEN: usize = 52;

impl RepeaterStatus {
    pub fn parse(d: &[u8]) -> Option<RepeaterStatus> {
        if d.len() < STATUS_MIN_LEN {
            return None;
        }
        let u16at = |i: usize| u16::from_le_bytes([d[i], d[i + 1]]);
        let i16at = |i: usize| i16::from_le_bytes([d[i], d[i + 1]]);
        let u32at = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
        Some(RepeaterStatus {
            battery_mv: u16at(0),
            tx_queue_len: u16at(2),
            noise_floor_dbm: i16at(4),
            last_rssi_dbm: i16at(6),
            nb_recv: u32at(8),
            nb_sent: u32at(12),
            airtime_s: u32at(16),
            uptime_s: u32at(20),
            sent_flood: u32at(24),
            sent_direct: u32at(28),
            recv_flood: u32at(32),
            recv_direct: u32at(36),
            full_evts: u16at(40),
            last_snr_db: i16at(42) as f64 / 4.0,
            direct_dups: u16at(44),
            flood_dups: u16at(46),
            rx_airtime_s: u32at(48),
            recv_errors: (d.len() >= 56).then(|| u32at(52)),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(56);
        v.extend_from_slice(&self.battery_mv.to_le_bytes());
        v.extend_from_slice(&self.tx_queue_len.to_le_bytes());
        v.extend_from_slice(&self.noise_floor_dbm.to_le_bytes());
        v.extend_from_slice(&self.last_rssi_dbm.to_le_bytes());
        for x in [self.nb_recv, self.nb_sent, self.airtime_s, self.uptime_s] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        for x in [self.sent_flood, self.sent_direct, self.recv_flood, self.recv_direct] {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v.extend_from_slice(&self.full_evts.to_le_bytes());
        v.extend_from_slice(&((self.last_snr_db * 4.0).round() as i16).to_le_bytes());
        v.extend_from_slice(&self.direct_dups.to_le_bytes());
        v.extend_from_slice(&self.flood_dups.to_le_bytes());
        v.extend_from_slice(&self.rx_airtime_s.to_le_bytes());
        if let Some(e) = self.recv_errors {
            v.extend_from_slice(&e.to_le_bytes());
        }
        v
    }
}

/// What a frame from the radio means.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    Ok,
    Err(u8),
    ContactsStart(u32),
    Contact(ContactRec),
    EndOfContacts(u32),
    SelfInfo(SelfInfo),
    Sent {
        timeout_ms: u32,
    },
    DeviceInfo {
        fw_ver: u8,
        model: String,
        version: String,
    },
    Advert {
        key: String,
    },
    NewAdvert(ContactRec),
    LoginSuccess {
        prefix: String,
        is_admin: bool,
    },
    LoginFail {
        prefix: String,
    },
    Status {
        prefix: String,
        status: RepeaterStatus,
    },
    Telemetry {
        prefix: String,
        lpp: Vec<u8>,
    },
    /// A push or response this gateway does not use (new messages, path updates and so on).
    Other(u8),
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&x| x == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn i32le(b: &[u8]) -> i32 {
    i32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

const CONTACT_LEN: usize = 148;

pub fn parse_contact(d: &[u8]) -> Option<ContactRec> {
    if d.len() < CONTACT_LEN {
        return None;
    }
    Some(ContactRec {
        key: hex(&d[1..33]),
        kind: d[33],
        // d[34] flags, d[35] out_path_len, d[36..100] out_path: routing detail we do not use.
        name: cstr(&d[100..132]),
        last_advert: u32le(&d[132..136]),
        lat: i32le(&d[136..140]) as f64 / 1e6,
        lon: i32le(&d[140..144]) as f64 / 1e6,
        lastmod: u32le(&d[144..148]),
    })
}

pub fn encode_contact(code: u8, c: &ContactRec) -> Vec<u8> {
    let mut v = vec![0u8; CONTACT_LEN];
    v[0] = code;
    if let Some(k) = from_hex(&c.key) {
        v[1..33].copy_from_slice(&k[..32.min(k.len())]);
    }
    v[33] = c.kind;
    v[35] = 255; // out_path_len -1: no known path
    let name = c.name.as_bytes();
    let n = name.len().min(31);
    v[100..100 + n].copy_from_slice(&name[..n]);
    v[132..136].copy_from_slice(&c.last_advert.to_le_bytes());
    v[136..140].copy_from_slice(&((c.lat * 1e6).round() as i32).to_le_bytes());
    v[140..144].copy_from_slice(&((c.lon * 1e6).round() as i32).to_le_bytes());
    v[144..148].copy_from_slice(&c.lastmod.to_le_bytes());
    v
}

pub fn parse(d: &[u8]) -> Option<Incoming> {
    let code = *d.first()?;
    Some(match code {
        RESP_OK => Incoming::Ok,
        RESP_ERR => Incoming::Err(d.get(1).copied().unwrap_or(0)),
        RESP_CONTACTS_START if d.len() >= 5 => Incoming::ContactsStart(u32le(&d[1..5])),
        RESP_CONTACT => Incoming::Contact(parse_contact(d)?),
        RESP_END_OF_CONTACTS if d.len() >= 5 => Incoming::EndOfContacts(u32le(&d[1..5])),
        RESP_SELF_INFO if d.len() >= 58 => Incoming::SelfInfo(SelfInfo {
            key: hex(&d[4..36]),
            name: cstr(&d[58..]),
        }),
        RESP_SENT if d.len() >= 10 => Incoming::Sent {
            timeout_ms: u32le(&d[6..10]),
        },
        RESP_DEVICE_INFO if d.len() >= 3 => Incoming::DeviceInfo {
            fw_ver: d[1],
            model: if d.len() >= 60 { cstr(&d[20..60]) } else { String::new() },
            version: if d.len() >= 80 { cstr(&d[60..80]) } else { String::new() },
        },
        PUSH_ADVERT if d.len() >= 33 => Incoming::Advert { key: hex(&d[1..33]) },
        PUSH_NEW_ADVERT => Incoming::NewAdvert(parse_contact(d)?),
        PUSH_LOGIN_SUCCESS if d.len() >= 8 => Incoming::LoginSuccess {
            is_admin: d[1] & 1 == 1,
            prefix: hex(&d[2..8]),
        },
        PUSH_LOGIN_FAIL if d.len() >= 8 => Incoming::LoginFail { prefix: hex(&d[2..8]) },
        PUSH_STATUS_RESPONSE if d.len() >= 8 => Incoming::Status {
            prefix: hex(&d[2..8]),
            status: RepeaterStatus::parse(&d[8..])?,
        },
        PUSH_TELEMETRY_RESPONSE if d.len() >= 8 => Incoming::Telemetry {
            prefix: hex(&d[2..8]),
            lpp: d[8..].to_vec(),
        },
        c => Incoming::Other(c),
    })
}

// ---- radio-side encoders (used by the fake radio and by tests) ----

pub fn encode_self_info(key: &str, name: &str) -> Vec<u8> {
    let mut v = vec![0u8; 58];
    v[0] = RESP_SELF_INFO;
    v[1] = ADV_TYPE_CHAT;
    v[2] = 22;
    v[3] = 22;
    if let Some(k) = from_hex(key) {
        v[4..36].copy_from_slice(&k[..32.min(k.len())]);
    }
    v[48..52].copy_from_slice(&910_525u32.to_le_bytes());
    v[52..56].copy_from_slice(&62_500u32.to_le_bytes());
    v[56] = 7;
    v[57] = 5;
    v.extend_from_slice(name.as_bytes());
    v
}

pub fn encode_sent(timeout_ms: u32) -> Vec<u8> {
    let mut v = vec![RESP_SENT, 0, 1, 2, 3, 4];
    v.extend_from_slice(&timeout_ms.to_le_bytes());
    v
}

pub fn encode_device_info(model: &str, version: &str) -> Vec<u8> {
    let mut v = vec![0u8; 80];
    v[0] = RESP_DEVICE_INFO;
    v[1] = 9;
    v[2] = 175;
    v[3] = 40;
    let (m, s) = (model.as_bytes(), version.as_bytes());
    v[20..20 + m.len().min(39)].copy_from_slice(&m[..m.len().min(39)]);
    v[60..60 + s.len().min(19)].copy_from_slice(&s[..s.len().min(19)]);
    v
}

pub fn encode_login(code: u8, key: &[u8], admin: bool) -> Vec<u8> {
    let mut v = vec![code, admin as u8];
    v.extend_from_slice(&key[..6]);
    if code == PUSH_LOGIN_SUCCESS {
        v.extend_from_slice(&[0, 0, 0, 0]); // server timestamp
        v.push(admin as u8);
        v.push(9);
    }
    v
}

pub fn encode_status(key: &[u8], s: &RepeaterStatus) -> Vec<u8> {
    let mut v = vec![PUSH_STATUS_RESPONSE, 0];
    v.extend_from_slice(&key[..6]);
    v.extend_from_slice(&s.encode());
    v
}

pub fn encode_telemetry(key: &[u8], lpp: &[u8]) -> Vec<u8> {
    let mut v = vec![PUSH_TELEMETRY_RESPONSE, 0];
    v.extend_from_slice(&key[..6]);
    v.extend_from_slice(lpp);
    v
}

// ---- Cayenne LPP telemetry ----

/// What a node reported in a telemetry response that CrewAware can use.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Telemetry {
    pub voltage: Option<f64>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

/// Cayenne LPP: repeated (channel, type, value). Only voltage (116) and GPS (136) are read;
/// other known types are skipped by size, and an unknown type stops parsing.
pub fn parse_lpp(d: &[u8]) -> Telemetry {
    let mut t = Telemetry::default();
    let mut i = 0;
    while i + 2 <= d.len() {
        let ty = d[i + 1];
        let body = &d[i + 2..];
        let size = match ty {
            0 | 1 => 1,     // digital in/out
            2 | 3 => 2,     // analog in/out
            100..=102 => 4, // generic, illuminance, presence-like
            103 | 104 => {
                if ty == 103 {
                    2
                } else {
                    1
                }
            } // temperature, humidity
            115 => 2,       // barometer
            116 => 2,       // voltage
            117 => 2,       // current
            136 => 9,       // GPS
            _ => break,
        };
        if body.len() < size {
            break;
        }
        match ty {
            116 => t.voltage = Some(u16::from_be_bytes([body[0], body[1]]) as f64 / 100.0),
            136 => {
                let s24 = |a: u8, b: u8, c: u8| {
                    let v = ((a as i32) << 16) | ((b as i32) << 8) | c as i32;
                    if v & 0x80_0000 != 0 {
                        v - 0x100_0000
                    } else {
                        v
                    }
                };
                t.lat = Some(s24(body[0], body[1], body[2]) as f64 / 10_000.0);
                t.lon = Some(s24(body[3], body[4], body[5]) as f64 / 10_000.0);
            }
            _ => {}
        }
        i += 2 + size;
    }
    t
}

pub fn encode_lpp(voltage: Option<f64>, pos: Option<(f64, f64)>) -> Vec<u8> {
    let mut v = Vec::new();
    if let Some(volts) = voltage {
        v.extend_from_slice(&[1, 116]);
        v.extend_from_slice(&((volts * 100.0).round() as u16).to_be_bytes());
    }
    if let Some((lat, lon)) = pos {
        v.extend_from_slice(&[2, 136]);
        for x in [lat, lon] {
            let n = (x * 10_000.0).round() as i32;
            v.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        }
        v.extend_from_slice(&[0, 0, 0]); // altitude
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> String {
        hex(&[n; 32])
    }

    #[test]
    fn contact_round_trip() {
        let c = ContactRec {
            key: key(7),
            kind: ADV_TYPE_CHAT,
            name: "UTV 1".into(),
            lat: 39.001234,
            lon: -104.002345,
            last_advert: 1_700_000_000,
            lastmod: 1_700_000_005,
        };
        let bytes = encode_contact(RESP_CONTACT, &c);
        assert_eq!(bytes.len(), 148);
        assert_eq!(parse(&bytes), Some(Incoming::Contact(c)));
    }

    #[test]
    fn status_round_trip_and_snr_scaling() {
        let s = RepeaterStatus {
            battery_mv: 4012,
            tx_queue_len: 1,
            noise_floor_dbm: -112,
            last_rssi_dbm: -88,
            nb_recv: 100,
            nb_sent: 50,
            airtime_s: 600,
            uptime_s: 86_400,
            sent_flood: 3,
            sent_direct: 4,
            recv_flood: 5,
            recv_direct: 6,
            full_evts: 0,
            last_snr_db: 6.25,
            direct_dups: 1,
            flood_dups: 2,
            rx_airtime_s: 900,
            recv_errors: Some(8),
        };
        let bytes = s.encode();
        assert_eq!(bytes.len(), 56);
        assert_eq!(RepeaterStatus::parse(&bytes), Some(s.clone()));
        // The 52-byte form (older firmware) has no error counter.
        let short = RepeaterStatus::parse(&bytes[..52]).unwrap();
        assert_eq!(short.recv_errors, None);
        assert!(RepeaterStatus::parse(&bytes[..51]).is_none());
        let k = [9u8; 32];
        let push = encode_status(&k, &s);
        assert_eq!(
            parse(&push),
            Some(Incoming::Status {
                prefix: hex(&k[..6]),
                status: s
            })
        );
    }

    #[test]
    fn login_pushes() {
        let k = [3u8; 32];
        assert_eq!(
            parse(&encode_login(PUSH_LOGIN_SUCCESS, &k, true)),
            Some(Incoming::LoginSuccess {
                prefix: hex(&k[..6]),
                is_admin: true
            })
        );
        assert_eq!(
            parse(&encode_login(PUSH_LOGIN_FAIL, &k, false)),
            Some(Incoming::LoginFail { prefix: hex(&k[..6]) })
        );
    }

    #[test]
    fn self_info_sent_and_device_info() {
        assert_eq!(
            parse(&encode_self_info(&key(1), "Gateway")),
            Some(Incoming::SelfInfo(SelfInfo {
                key: key(1),
                name: "Gateway".into()
            }))
        );
        assert_eq!(parse(&encode_sent(4500)), Some(Incoming::Sent { timeout_ms: 4500 }));
        match parse(&encode_device_info("T-Echo", "v1.9.0")) {
            Some(Incoming::DeviceInfo { model, version, .. }) => {
                assert_eq!((model.as_str(), version.as_str()), ("T-Echo", "v1.9.0"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn short_or_unknown_frames_are_not_misread() {
        assert_eq!(parse(&[]), None);
        assert_eq!(parse(&[RESP_CONTACT, 1, 2, 3]), None);
        assert_eq!(parse(&[PUSH_MSG_WAITING]), Some(Incoming::Other(PUSH_MSG_WAITING)));
        assert_eq!(parse(&[PUSH_STATUS_RESPONSE, 0, 1, 2, 3, 4, 5, 6, 7]), None);
    }

    #[test]
    fn lpp_voltage_and_gps() {
        let b = encode_lpp(Some(3.97), Some((39.0012, -104.002)));
        let t = parse_lpp(&b);
        assert_eq!(t.voltage, Some(3.97));
        assert!((t.lat.unwrap() - 39.0012).abs() < 1e-4);
        assert!((t.lon.unwrap() + 104.002).abs() < 1e-4);
        // Garbage stops cleanly.
        assert_eq!(parse_lpp(&[1, 250, 1, 2]), Telemetry::default());
    }

    #[test]
    fn commands() {
        let k = [0xabu8; 32];
        assert_eq!(send_status_req(&k)[0], 27);
        assert_eq!(send_telemetry_req(&k)[..4], [39, 0, 0, 0]);
        let l = send_login(&k, "abcdefghijklmnopqrstuvwxyz");
        assert_eq!(l.len(), 1 + 32 + MAX_PASSWORD);
        assert_eq!(get_contacts(Some(5)), vec![4, 5, 0, 0, 0]);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(from_hex("0aff"), Some(vec![10, 255]));
    }
}
