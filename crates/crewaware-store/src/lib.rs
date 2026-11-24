//! PostgreSQL persistence. The engine keeps live state in memory and emits `Change`s; this crate
//! loads state at startup, applies changes, and stores assets and admin accounts.

use aes_gcm::aead::{Aead, AeadCore, KeyInit, OsRng as AeadOsRng};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, Context, Result};
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use chrono::{DateTime, Utc};
use crewaware_core::engine::State;
use crewaware_core::*;
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Position history is throttled per unit so a busy radio does not flood the table.
const HISTORY_EVERY: Duration = Duration::from_secs(30);
const SESSION_HOURS: i64 = 12;

pub struct Store {
    pool: PgPool,
    last_history: Mutex<HashMap<String, Instant>>,
    /// Key for repeater passwords, derived from CREWAWARE_SECRET_KEY. None disables storing them.
    secret: Option<[u8; 32]>,
}

impl Store {
    pub async fn connect(url: &str) -> Result<Store> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(10))
            .connect(url)
            .await?;
        Ok(Store {
            pool,
            last_history: Mutex::new(HashMap::new()),
            secret: None,
        })
    }

    /// Sets the passphrase repeater passwords are encrypted with. Anyone with the database and this
    /// value can read them, which is the accepted trade-off: the app has to log in to repeaters.
    pub fn with_secret_key(mut self, passphrase: Option<&str>) -> Store {
        self.secret = passphrase.filter(|p| !p.is_empty()).map(|p| {
            let mut h = Sha256::new();
            h.update(b"crewaware/repeater-credentials/v1");
            h.update(p.as_bytes());
            h.finalize().into()
        });
        self
    }

    pub fn credentials_enabled(&self) -> bool {
        self.secret.is_some()
    }

    fn cipher(&self) -> Result<Aes256Gcm> {
        let k = self
            .secret
            .as_ref()
            .ok_or_else(|| anyhow!("CREWAWARE_SECRET_KEY is not set, so repeater passwords cannot be stored"))?;
        Ok(Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(k)))
    }

    pub async fn set_repeater_password(&self, repeater_id: &str, password: &str) -> Result<()> {
        let cipher = self.cipher()?;
        let nonce = Aes256Gcm::generate_nonce(&mut AeadOsRng);
        let ct = cipher
            .encrypt(&nonce, password.as_bytes())
            .map_err(|_| anyhow!("encryption failed"))?;
        sqlx::query(
            "INSERT INTO repeater_credentials (repeater_id,nonce,ciphertext) VALUES ($1,$2,$3)
             ON CONFLICT (repeater_id) DO UPDATE SET nonce=$2, ciphertext=$3, updated_at=now()",
        )
        .bind(repeater_id)
        .bind(nonce.as_slice())
        .bind(ct)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_repeater_password(&self, repeater_id: &str) -> Result<Option<String>> {
        let Some(row) = sqlx::query("SELECT nonce, ciphertext FROM repeater_credentials WHERE repeater_id = $1")
            .bind(repeater_id)
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let nonce: Vec<u8> = row.get("nonce");
        let ct: Vec<u8> = row.get("ciphertext");
        if nonce.len() != 12 {
            return Err(anyhow!("stored nonce is the wrong length"));
        }
        let pt = self
            .cipher()?
            .decrypt(Nonce::from_slice(&nonce), ct.as_ref())
            .map_err(|_| anyhow!("could not decrypt the stored password (was CREWAWARE_SECRET_KEY changed?)"))?;
        Ok(Some(String::from_utf8(pt).context("stored password is not text")?))
    }

    pub async fn delete_repeater_password(&self, repeater_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM repeater_credentials WHERE repeater_id = $1")
            .bind(repeater_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("../../migrations").run(&self.pool).await?;
        Ok(())
    }

    pub async fn ping(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }

    pub async fn load(&self) -> Result<State> {
        let mut st = State {
            next_incident: 1,
            ..State::default()
        };

        if let Some(r) = sqlx::query("SELECT * FROM site WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?
        {
            st.site = Site {
                name: r.get("name"),
                overlay: r.get("overlay"),
                img_w: r.get("img_w"),
                img_h: r.get("img_h"),
                a: Anchor {
                    px: r.get("a_px"),
                    py: r.get("a_py"),
                    lat: r.get("a_lat"),
                    lon: r.get("a_lon"),
                },
                b: Anchor {
                    px: r.get("b_px"),
                    py: r.get("b_py"),
                    lat: r.get("b_lat"),
                    lon: r.get("b_lon"),
                },
                logo: r.get("logo"),
                slogan: r.get("slogan"),
            };
        }
        if let Some(r) = sqlx::query("SELECT data FROM settings WHERE id = 1")
            .fetch_optional(&self.pool)
            .await?
        {
            let v: serde_json::Value = r.get("data");
            st.settings = serde_json::from_value(v).unwrap_or_default();
        }
        for r in sqlx::query("SELECT * FROM groups ORDER BY name")
            .fetch_all(&self.pool)
            .await?
        {
            st.groups.push(Group {
                id: r.get("id"),
                name: r.get("name"),
                max_mph: r.get("max_mph"),
                adjust: r.get("adjust"),
            });
        }
        for r in sqlx::query("SELECT * FROM units ORDER BY name, id")
            .fetch_all(&self.pool)
            .await?
        {
            let status: String = r.get("status");
            st.units.push(Unit {
                id: r.get("id"),
                name: r.get("name"),
                group_id: r.get("group_id"),
                status: Status::parse(&status).ok_or_else(|| anyhow!("bad status {status}"))?,
                has_pos: r.get("has_pos"),
                lat: r.get("lat"),
                lon: r.get("lon"),
                speed_mph: r.get("speed_mph"),
                moving: r.get("moving"),
                battery: r.get("battery"),
                last_seen: r.get("last_seen"),
                incident_id: r.get("incident_id"),
                node_key: r.get("node_key"),
                seen_mono: None,
            });
        }
        let rows = sqlx::query(
            "SELECT r.*, (c.repeater_id IS NOT NULL) AS has_credentials,
                    s.read_at, s.ok AS read_ok, s.error AS read_error, s.data AS read_data
             FROM repeaters r
             LEFT JOIN repeater_credentials c ON c.repeater_id = r.id
             LEFT JOIN repeater_status s ON s.repeater_id = r.id
             ORDER BY r.name, r.id",
        )
        .fetch_all(&self.pool)
        .await?;
        for r in rows {
            let read_at: Option<DateTime<Utc>> = r.get("read_at");
            let reading = read_at.map(|read_at| RepeaterReading {
                read_at,
                ok: r.get::<Option<bool>, _>("read_ok").unwrap_or(false),
                error: r.get::<Option<String>, _>("read_error").unwrap_or_default(),
                data: r
                    .get::<Option<serde_json::Value>, _>("read_data")
                    .unwrap_or(serde_json::Value::Null),
            });
            st.repeaters.push(Repeater {
                id: r.get("id"),
                name: r.get("name"),
                lat: r.get("lat"),
                lon: r.get("lon"),
                placed: r.get("placed"),
                node_key: r.get("node_key"),
                last_seen: r.get("last_seen"),
                has_credentials: r.get("has_credentials"),
                reading,
                seen_mono: None,
            });
        }
        for r in sqlx::query("SELECT * FROM incidents ORDER BY id")
            .fetch_all(&self.pool)
            .await?
        {
            let id: i32 = r.get("id");
            st.next_incident = st.next_incident.max(id + 1);
            st.incidents.push(Incident {
                id,
                label: r.get("label"),
                lat: r.get("lat"),
                lon: r.get("lon"),
                opened: r.get("opened"),
                closed: r.get("closed"),
                open: r.get("open"),
                units: r.get("units"),
                opened_by: r.get("opened_by"),
            });
        }
        let mut log: Vec<LogEntry> =
            sqlx::query("SELECT t, seat, text, incident_id FROM activity_log ORDER BY t DESC, id DESC LIMIT 2000")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|r| LogEntry {
                    t: r.get("t"),
                    seat: r.get("seat"),
                    text: r.get("text"),
                    incident: r.get("incident_id"),
                })
                .collect();
        log.reverse();
        st.log = log;
        for r in sqlx::query("SELECT * FROM obstacles ORDER BY id")
            .fetch_all(&self.pool)
            .await?
        {
            let kind: String = r.get("kind");
            let points: serde_json::Value = r.get("points");
            st.obstacles.push(Obstacle {
                id: r.get("id"),
                kind: match kind.as_str() {
                    "wall" => ObstacleKind::Wall,
                    "gate" => ObstacleKind::Gate,
                    "no_go" => ObstacleKind::NoGo,
                    _ => ObstacleKind::Slow,
                },
                name: r.get("name"),
                points: serde_json::from_value(points).unwrap_or_default(),
                open: r.get("open"),
                factor: r.get("factor"),
            });
        }
        Ok(st)
    }

    pub async fn apply(&self, c: &Change) -> Result<()> {
        match c {
            Change::Unit(u) => {
                sqlx::query(
                    "INSERT INTO units (id,name,group_id,status,has_pos,lat,lon,speed_mph,moving,battery,last_seen,incident_id,node_key)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
                     ON CONFLICT (id) DO UPDATE SET name=$2,group_id=$3,status=$4,has_pos=$5,lat=$6,lon=$7,
                       speed_mph=$8,moving=$9,battery=$10,last_seen=$11,incident_id=$12,node_key=$13",
                )
                .bind(&u.id)
                .bind(&u.name)
                .bind(&u.group_id)
                .bind(u.status.as_str())
                .bind(u.has_pos)
                .bind(u.lat)
                .bind(u.lon)
                .bind(u.speed_mph)
                .bind(u.moving)
                .bind(u.battery)
                .bind(u.last_seen)
                .bind(u.incident_id)
                .bind(&u.node_key)
                .execute(&self.pool)
                .await?;
            }
            Change::Repeater(r) => {
                sqlx::query(
                    "INSERT INTO repeaters (id,name,lat,lon,placed,last_seen,node_key) VALUES ($1,$2,$3,$4,$5,$6,$7)
                     ON CONFLICT (id) DO UPDATE SET name=$2,lat=$3,lon=$4,placed=$5,last_seen=$6,node_key=$7",
                )
                .bind(&r.id)
                .bind(&r.name)
                .bind(r.lat)
                .bind(r.lon)
                .bind(r.placed)
                .bind(r.last_seen)
                .bind(&r.node_key)
                .execute(&self.pool)
                .await?;
            }
            Change::Reading { repeater_id, reading } => {
                sqlx::query(
                    "INSERT INTO repeater_status (repeater_id,read_at,ok,error,data) VALUES ($1,$2,$3,$4,$5)
                     ON CONFLICT (repeater_id) DO UPDATE SET read_at=$2,ok=$3,error=$4,data=$5",
                )
                .bind(repeater_id)
                .bind(reading.read_at)
                .bind(reading.ok)
                .bind(&reading.error)
                .bind(&reading.data)
                .execute(&self.pool)
                .await?;
            }
            Change::Incident(i) => {
                sqlx::query(
                    "INSERT INTO incidents (id,label,lat,lon,opened,closed,open,units,opened_by)
                     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
                     ON CONFLICT (id) DO UPDATE SET label=$2,lat=$3,lon=$4,opened=$5,closed=$6,open=$7,units=$8,opened_by=$9",
                )
                .bind(i.id)
                .bind(&i.label)
                .bind(i.lat)
                .bind(i.lon)
                .bind(i.opened)
                .bind(i.closed)
                .bind(i.open)
                .bind(&i.units)
                .bind(&i.opened_by)
                .execute(&self.pool)
                .await?;
            }
            Change::Log(l) => {
                sqlx::query("INSERT INTO activity_log (t,seat,text,incident_id) VALUES ($1,$2,$3,$4)")
                    .bind(l.t)
                    .bind(&l.seat)
                    .bind(&l.text)
                    .bind(l.incident)
                    .execute(&self.pool)
                    .await?;
            }
            Change::Site(s) => {
                sqlx::query(
                    "INSERT INTO site (id,name,overlay,img_w,img_h,a_px,a_py,a_lat,a_lon,b_px,b_py,b_lat,b_lon,logo,slogan)
                     VALUES (1,$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
                     ON CONFLICT (id) DO UPDATE SET name=$1,overlay=$2,img_w=$3,img_h=$4,a_px=$5,a_py=$6,a_lat=$7,a_lon=$8,
                       b_px=$9,b_py=$10,b_lat=$11,b_lon=$12,logo=$13,slogan=$14",
                )
                .bind(&s.name)
                .bind(&s.overlay)
                .bind(s.img_w)
                .bind(s.img_h)
                .bind(s.a.px)
                .bind(s.a.py)
                .bind(s.a.lat)
                .bind(s.a.lon)
                .bind(s.b.px)
                .bind(s.b.py)
                .bind(s.b.lat)
                .bind(s.b.lon)
                .bind(&s.logo)
                .bind(&s.slogan)
                .execute(&self.pool)
                .await?;
            }
            Change::Group(g) => {
                sqlx::query(
                    "INSERT INTO groups (id,name,max_mph,adjust) VALUES ($1,$2,$3,$4)
                     ON CONFLICT (id) DO UPDATE SET name=$2,max_mph=$3,adjust=$4",
                )
                .bind(&g.id)
                .bind(&g.name)
                .bind(g.max_mph)
                .bind(g.adjust)
                .execute(&self.pool)
                .await?;
            }
            Change::Obstacles(list) => {
                let mut tx = self.pool.begin().await?;
                sqlx::query("DELETE FROM obstacles").execute(&mut *tx).await?;
                for o in list {
                    let kind = match o.kind {
                        ObstacleKind::Wall => "wall",
                        ObstacleKind::Gate => "gate",
                        ObstacleKind::NoGo => "no_go",
                        ObstacleKind::Slow => "slow",
                    };
                    sqlx::query("INSERT INTO obstacles (id,kind,name,points,open,factor) VALUES ($1,$2,$3,$4,$5,$6)")
                        .bind(o.id)
                        .bind(kind)
                        .bind(&o.name)
                        .bind(serde_json::to_value(&o.points)?)
                        .bind(o.open)
                        .bind(o.factor)
                        .execute(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
            }
            Change::RemoveUnit(id) => {
                sqlx::query("DELETE FROM position_history WHERE unit_id = $1")
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
                sqlx::query("DELETE FROM units WHERE id = $1")
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
            Change::RemoveRepeater(id) => {
                sqlx::query("DELETE FROM repeaters WHERE id = $1")
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
            Change::Settings(s) => {
                sqlx::query("INSERT INTO settings (id,data) VALUES (1,$1) ON CONFLICT (id) DO UPDATE SET data=$1")
                    .bind(serde_json::to_value(s)?)
                    .execute(&self.pool)
                    .await?;
            }
            Change::Position {
                unit_id,
                lat,
                lon,
                speed_mph,
                battery,
                at,
            } => {
                {
                    let mut m = self.last_history.lock().unwrap();
                    match m.get(unit_id) {
                        Some(t) if t.elapsed() < HISTORY_EVERY => return Ok(()),
                        _ => {
                            m.insert(unit_id.clone(), Instant::now());
                        }
                    }
                }
                sqlx::query(
                    "INSERT INTO position_history (unit_id,at,lat,lon,speed_mph,battery) VALUES ($1,$2,$3,$4,$5,$6)",
                )
                .bind(unit_id)
                .bind(at)
                .bind(lat)
                .bind(lon)
                .bind(speed_mph)
                .bind(battery)
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }

    // ---- saved sites ----

    pub async fn list_profiles(&self) -> Result<Vec<(i32, String, DateTime<Utc>)>> {
        Ok(
            sqlx::query("SELECT id, name, saved_at FROM site_profiles ORDER BY lower(name)")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|r| (r.get("id"), r.get("name"), r.get("saved_at")))
                .collect(),
        )
    }

    /// Saves under the site's name, replacing an earlier save of the same name.
    pub async fn save_profile(&self, data: &SiteData) -> Result<i32> {
        let r = sqlx::query(
            "INSERT INTO site_profiles (name,data) VALUES ($1,$2)
             ON CONFLICT (name) DO UPDATE SET data=$2, saved_at=now() RETURNING id",
        )
        .bind(&data.site.name)
        .bind(serde_json::to_value(data)?)
        .fetch_one(&self.pool)
        .await?;
        Ok(r.get("id"))
    }

    pub async fn get_profile(&self, id: i32) -> Result<Option<SiteData>> {
        match sqlx::query("SELECT data FROM site_profiles WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
        {
            Some(r) => Ok(Some(serde_json::from_value(r.get("data"))?)),
            None => Ok(None),
        }
    }

    /// Image files that saved sites point at, so cleaning up unused uploads leaves them alone.
    pub async fn profile_assets(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for r in
            sqlx::query("SELECT data->'site'->>'overlay' AS overlay, data->'site'->>'logo' AS logo FROM site_profiles")
                .fetch_all(&self.pool)
                .await?
        {
            for col in ["overlay", "logo"] {
                if let Some(n) = r.get::<Option<String>, _>(col).filter(|n| !n.is_empty()) {
                    out.push(n);
                }
            }
        }
        Ok(out)
    }

    pub async fn delete_profile(&self, id: i32) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM site_profiles WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected()
            > 0)
    }

    /// Every log line tied to one incident, oldest first, for its report.
    pub async fn incident_log(&self, incident_id: i32) -> Result<Vec<LogEntry>> {
        Ok(
            sqlx::query("SELECT t, seat, text FROM activity_log WHERE incident_id = $1 ORDER BY t, id")
                .bind(incident_id)
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(|r| LogEntry {
                    t: r.get("t"),
                    seat: r.get("seat"),
                    text: r.get("text"),
                    incident: Some(incident_id),
                })
                .collect(),
        )
    }

    /// Deletes position history older than `days`. Returns the rows removed.
    pub async fn prune_history(&self, days: i64) -> Result<u64> {
        let r = sqlx::query("DELETE FROM position_history WHERE at < now() - make_interval(days => $1::int)")
            .bind(days as i32)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM admin_sessions WHERE expires_at < now()")
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected())
    }

    pub async fn history_count(&self, unit_id: &str) -> Result<i64> {
        Ok(
            sqlx::query("SELECT count(*) AS n FROM position_history WHERE unit_id = $1")
                .bind(unit_id)
                .fetch_one(&self.pool)
                .await?
                .get("n"),
        )
    }

    // ---- assets ----

    pub async fn put_asset(&self, name: &str, content_type: &str, data: &[u8]) -> Result<()> {
        sqlx::query("INSERT INTO assets (name,content_type,data) VALUES ($1,$2,$3) ON CONFLICT (name) DO UPDATE SET content_type=$2,data=$3")
            .bind(name)
            .bind(content_type)
            .bind(data)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_asset(&self, name: &str) -> Result<Option<(String, Vec<u8>)>> {
        Ok(sqlx::query("SELECT content_type, data FROM assets WHERE name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| (r.get("content_type"), r.get("data"))))
    }

    /// Removes assets no longer referenced by the site, so replaced overlays do not pile up.
    pub async fn prune_assets(&self, keep: &[&str]) -> Result<u64> {
        let keep: Vec<String> = keep.iter().map(|s| s.to_string()).collect();
        Ok(sqlx::query("DELETE FROM assets WHERE NOT (name = ANY($1))")
            .bind(&keep)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }

    // ---- admin accounts and sessions ----

    fn hash_password(pw: &str) -> Result<String> {
        let salt = SaltString::generate(&mut OsRng);
        Ok(Argon2::default()
            .hash_password(pw.as_bytes(), &salt)
            .map_err(|e| anyhow!("hash: {e}"))?
            .to_string())
    }

    /// Creates the account if it does not exist yet. An existing account is left alone, so
    /// passwords are changed in the database or a future Admin page, not by editing .env.
    pub async fn ensure_admin(&self, username: &str, password: &str) -> Result<bool> {
        if username.trim().is_empty() {
            return Err(anyhow!("admin username is empty"));
        }
        if password.chars().count() < 12 {
            return Err(anyhow!("admin password must be at least 12 characters"));
        }
        let hash = Self::hash_password(password)?;
        let r = sqlx::query(
            "INSERT INTO admin_users (username,password_hash) VALUES ($1,$2) ON CONFLICT (username) DO NOTHING",
        )
        .bind(username.trim())
        .bind(hash)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    /// Sets (or creates) an account's password. Used by the `set-password` command.
    pub async fn set_admin_password(&self, username: &str, password: &str) -> Result<()> {
        if password.chars().count() < 12 {
            return Err(anyhow!("admin password must be at least 12 characters"));
        }
        let hash = Self::hash_password(password)?;
        sqlx::query(
            "INSERT INTO admin_users (username,password_hash) VALUES ($1,$2)
             ON CONFLICT (username) DO UPDATE SET password_hash = $2",
        )
        .bind(username.trim())
        .bind(hash)
        .execute(&self.pool)
        .await?;
        // A password change ends every open session for that account.
        sqlx::query("DELETE FROM admin_sessions WHERE user_id = (SELECT id FROM admin_users WHERE username = $1)")
            .bind(username.trim())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn verify_login(&self, username: &str, password: &str) -> Result<Option<i64>> {
        let row = sqlx::query("SELECT id, password_hash FROM admin_users WHERE username = $1")
            .bind(username.trim())
            .fetch_optional(&self.pool)
            .await?;
        // Hash a dummy when the user is unknown, so timing does not reveal valid usernames.
        let dummy = "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$Zm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm9vYmFyZm8";
        let (id, hash) = match &row {
            Some(r) => (Some(r.get::<i64, _>("id")), r.get::<String, _>("password_hash")),
            None => (None, dummy.to_string()),
        };
        let ok = PasswordHash::new(&hash)
            .map(|p| Argon2::default().verify_password(password.as_bytes(), &p).is_ok())
            .unwrap_or(false);
        Ok(if ok { id } else { None })
    }

    fn token_hash(token: &str) -> String {
        Sha256::digest(token.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    pub async fn create_session(&self, user_id: i64) -> Result<String> {
        use rand::RngCore;
        let mut raw = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut raw);
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let expires: DateTime<Utc> = Utc::now() + chrono::Duration::hours(SESSION_HOURS);
        sqlx::query("INSERT INTO admin_sessions (token_hash,user_id,expires_at) VALUES ($1,$2,$3)")
            .bind(Self::token_hash(&token))
            .bind(user_id)
            .bind(expires)
            .execute(&self.pool)
            .await?;
        Ok(token)
    }

    /// Returns the username for a live session token.
    pub async fn session_user(&self, token: &str) -> Result<Option<String>> {
        Ok(sqlx::query(
            "SELECT u.username FROM admin_sessions s JOIN admin_users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > now()",
        )
        .bind(Self::token_hash(token))
        .fetch_optional(&self.pool)
        .await?
        .map(|r| r.get("username")))
    }

    pub async fn delete_session(&self, token: &str) -> Result<()> {
        sqlx::query("DELETE FROM admin_sessions WHERE token_hash = $1")
            .bind(Self::token_hash(token))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
