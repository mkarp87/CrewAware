use crate::app::{App, RefreshError};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::Engine as _;
use crewaware_core::{Anchor, EngineError, Group, Obstacle, Position, Settings, SiteData, Status};
use futures_util::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

const INDEX: &str = include_str!("../web/index.html");
const SESSION_COOKIE: &str = "crewaware_session";

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        let code = match e {
            EngineError::NotFound => StatusCode::NOT_FOUND,
            EngineError::Conflict(_) => StatusCode::CONFLICT,
            EngineError::Bad(_) => StatusCode::BAD_REQUEST,
        };
        ApiError(code, e.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("request failed: {e:#}");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

fn bad(m: &str) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, m.into())
}

type Res = Result<Json<Value>, ApiError>;
type Shared = State<Arc<App>>;

fn ok() -> Res {
    Ok(Json(json!({ "ok": true })))
}

fn seat(h: &HeaderMap) -> String {
    let v = h.get("x-seat").and_then(|v| v.to_str().ok()).unwrap_or("").trim();
    if v.is_empty() {
        "unknown".into()
    } else {
        v.chars().take(40).collect()
    }
}

fn cookie_token(h: &HeaderMap) -> Option<String> {
    let raw = h.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|p| p.trim().split_once('='))
        .find(|(k, _)| *k == SESSION_COOKIE)
        .map(|(_, v)| v.to_string())
}

async fn admin_user(app: &App, h: &HeaderMap) -> Option<String> {
    let t = cookie_token(h)?;
    app.store.session_user(&t).await.ok().flatten()
}

async fn require_admin(app: &App, h: &HeaderMap) -> Result<String, ApiError> {
    admin_user(app, h)
        .await
        .ok_or(ApiError(StatusCode::UNAUTHORIZED, "admin login required".into()))
}

pub fn router(app: Arc<App>) -> Router {
    let upload = Router::new()
        .route("/api/site/overlay", post(upload_overlay))
        .route("/api/site/logo", post(upload_logo))
        .layer(DefaultBodyLimit::max(25 << 20));
    let import = Router::new()
        .route("/api/admin/profiles/import", post(import_profile))
        .layer(DefaultBodyLimit::max(70 << 20));
    Router::new()
        .route("/", get(index))
        .route("/files/{name}", get(file))
        .route("/api/public/health", get(health))
        .route("/api/public/ready", get(ready))
        .route("/api/state", get(state))
        .route("/api/events", get(events))
        .route("/api/nearest", get(nearest))
        .route("/api/units/{id}/status", post(set_status))
        .route("/api/incidents", post(open_incident))
        .route("/api/incidents/{id}/dispatch", post(dispatch))
        .route("/api/incidents/{id}/arrive", post(arrive))
        .route("/api/incidents/{id}/close", post(close_incident))
        .route("/api/admin/login", post(login))
        .route("/api/admin/logout", post(logout))
        .route("/api/admin/me", get(me))
        .route("/api/units", post(add_unit))
        .route("/api/units/{id}", delete(remove_unit))
        .route("/api/units/{id}/node", put(set_unit_node))
        .route("/api/units/{id}/group", post(assign_group))
        .route("/api/groups", post(set_group))
        .route("/api/repeaters", post(set_repeater))
        .route("/api/repeaters/{id}", delete(remove_repeater))
        .route("/api/repeaters/{id}/node", put(set_repeater_node))
        .route("/api/repeaters/{id}/credentials", post(set_credentials))
        .route("/api/repeaters/{id}/refresh", post(refresh_repeater))
        .route("/api/site", post(set_site))
        .route("/api/settings", post(set_settings))
        .route("/api/position", post(inject_position))
        .route("/api/obstacles", post(add_obstacle))
        .route("/api/obstacles/{id}", put(update_obstacle).delete(delete_obstacle))
        .route("/api/obstacles/{id}/gate", post(set_gate))
        .route("/api/admin/discovered/{key}/adopt", post(adopt))
        .route("/api/admin/radio/advert", post(radio_advert))
        .route("/api/admin/profiles", get(list_profiles))
        .route("/api/admin/profiles/save", post(save_profile))
        .route("/api/admin/profiles/{id}", delete(delete_profile))
        .route("/api/admin/profiles/{id}/activate", post(activate_profile))
        .route("/api/admin/profiles/{id}/export", get(export_profile))
        .route("/api/admin/site/export", get(export_current))
        .route("/api/incidents/{id}/report", get(incident_report))
        .route("/api/reports/incidents.csv", get(incidents_csv))
        .merge(import)
        .merge(upload)
        .layer(axum::middleware::map_response(security_headers))
        .with_state(app)
}

async fn security_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    r
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        INDEX,
    )
}

async fn file(State(app): Shared, Path(name): Path<String>) -> Result<Response, ApiError> {
    match app.store.get_asset(&name).await? {
        Some((ct, data)) => Ok((
            [
                (header::CONTENT_TYPE, ct),
                // Asset names are unique per upload, so they can be cached for good.
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable".to_string()),
            ],
            data,
        )
            .into_response()),
        None => Err(ApiError(StatusCode::NOT_FOUND, "not found".into())),
    }
}

async fn health(State(app): Shared) -> Json<Value> {
    let db = app.store.ping().await;
    Json(json!({ "status": if db { "ok" } else { "degraded" }, "db": db }))
}

async fn ready(State(app): Shared) -> Response {
    if app.store.ping().await {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "database unreachable").into_response()
    }
}

async fn state(State(app): Shared) -> Json<crate::app::StateView> {
    Json(app.view())
}

/// Live updates over Server-Sent Events: a full snapshot on every change and every 2 s, so ages
/// stay fresh without a change.
async fn events(State(app): Shared) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = app.subscribe();
    let iv = tokio::time::interval(Duration::from_secs(2));
    let s = futures_util::stream::unfold((app, rx, iv), |(app, mut rx, mut iv)| async move {
        tokio::select! { _ = rx.recv() => {}, _ = iv.tick() => {} }
        let json = serde_json::to_string(&app.view()).ok()?;
        Some((Ok(Event::default().data(json)), (app, rx, iv)))
    });
    Sse::new(s).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct LatLon {
    lat: f64,
    lon: f64,
}

async fn nearest(State(app): Shared, Query(q): Query<LatLon>) -> Json<Vec<crewaware_core::Candidate>> {
    Json(app.read(|e| e.nearest(q.lat, q.lon)))
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

async fn set_status(State(app): Shared, h: HeaderMap, Path(id): Path<String>, Json(b): Json<StatusBody>) -> Res {
    let st = Status::parse(&b.status).ok_or_else(|| bad("unknown status"))?;
    let seat = seat(&h);
    app.with(|e| e.set_status(&id, st, &seat))?;
    ok()
}

#[derive(Deserialize)]
struct OpenBody {
    lat: f64,
    lon: f64,
    #[serde(default)]
    label: String,
}

async fn open_incident(
    State(app): Shared,
    h: HeaderMap,
    Json(b): Json<OpenBody>,
) -> Result<Json<crewaware_core::Incident>, ApiError> {
    if !(b.lat.is_finite() && b.lon.is_finite()) || b.label.chars().count() > 80 {
        return Err(bad("bad location or label"));
    }
    let seat = seat(&h);
    Ok(Json(app.with(|e| e.open_incident(b.lat, b.lon, &b.label, &seat))))
}

#[derive(Deserialize)]
struct DispatchBody {
    units: Vec<String>,
}

async fn dispatch(State(app): Shared, h: HeaderMap, Path(id): Path<i32>, Json(b): Json<DispatchBody>) -> Res {
    let seat = seat(&h);
    app.with(|e| e.dispatch(id, &b.units, &seat))?;
    ok()
}

#[derive(Deserialize)]
struct ArriveBody {
    unit: String,
}

async fn arrive(State(app): Shared, h: HeaderMap, Path(id): Path<i32>, Json(b): Json<ArriveBody>) -> Res {
    let seat = seat(&h);
    app.with(|e| e.arrive(id, &b.unit, &seat))?;
    ok()
}

async fn close_incident(State(app): Shared, h: HeaderMap, Path(id): Path<i32>) -> Res {
    let seat = seat(&h);
    app.with(|e| e.close_incident(id, &seat))?;
    ok()
}

// ---- admin ----

#[derive(Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

async fn login(State(app): Shared, Json(b): Json<LoginBody>) -> Result<Response, ApiError> {
    let user = b.username.trim().to_string();
    if app.login_blocked(&user) {
        return Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "too many failed logins, wait a few minutes".into(),
        ));
    }
    match app.store.verify_login(&user, &b.password).await? {
        Some(id) => {
            app.login_ok(&user);
            let token = app.store.create_session(id).await?;
            let secure = if app.cookie_secure { "; Secure" } else { "" };
            let cookie = format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200{secure}");
            Ok((
                [(header::SET_COOKIE, cookie)],
                Json(json!({ "ok": true, "username": user })),
            )
                .into_response())
        }
        None => {
            app.login_failed(&user);
            tokio::time::sleep(Duration::from_millis(400)).await;
            Err(ApiError(StatusCode::UNAUTHORIZED, "wrong username or password".into()))
        }
    }
}

async fn logout(State(app): Shared, h: HeaderMap) -> Result<Response, ApiError> {
    if let Some(t) = cookie_token(&h) {
        app.store.delete_session(&t).await?;
    }
    let cookie = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({ "ok": true }))).into_response())
}

async fn me(State(app): Shared, h: HeaderMap) -> Json<Value> {
    match admin_user(&app, &h).await {
        Some(u) => Json(json!({ "admin": true, "username": u })),
        None => Json(json!({ "admin": false })),
    }
}

#[derive(Deserialize)]
struct AddUnitBody {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    group: String,
}

async fn add_unit(State(app): Shared, h: HeaderMap, Json(b): Json<AddUnitBody>) -> Res {
    require_admin(&app, &h).await?;
    app.with(|e| e.add_unit(&b.id, &b.name, &b.group))?;
    ok()
}

#[derive(Deserialize)]
struct GroupBody {
    group: String,
}

async fn assign_group(State(app): Shared, h: HeaderMap, Path(id): Path<String>, Json(b): Json<GroupBody>) -> Res {
    require_admin(&app, &h).await?;
    app.with(|e| e.assign_group(&id, &b.group))?;
    ok()
}

async fn set_group(State(app): Shared, h: HeaderMap, Json(g): Json<Group>) -> Res {
    require_admin(&app, &h).await?;
    app.with(|e| e.set_group(g))?;
    ok()
}

#[derive(Deserialize)]
struct RepeaterBody {
    id: String,
    #[serde(default)]
    name: String,
    lat: f64,
    lon: f64,
    #[serde(default = "yes")]
    placed: bool,
}

fn yes() -> bool {
    true
}

async fn set_repeater(State(app): Shared, h: HeaderMap, Json(b): Json<RepeaterBody>) -> Res {
    require_admin(&app, &h).await?;
    app.with(|e| e.set_repeater(&b.id, &b.name, b.lat, b.lon, b.placed))?;
    ok()
}

#[derive(Deserialize)]
struct CredentialsBody {
    /// Empty removes the stored password.
    password: String,
}

/// Stores (or removes) the password the app uses to log in to a repeater. It is never sent back.
async fn set_credentials(
    State(app): Shared,
    h: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<CredentialsBody>,
) -> Res {
    require_admin(&app, &h).await?;
    if !app.read(|e| e.repeater_exists(&id)) {
        return Err(EngineError::NotFound.into());
    }
    if b.password.chars().count() > 128 {
        return Err(bad("password is too long"));
    }
    // A repeater added a moment ago may still be waiting to be saved, and passwords reference it.
    app.flush().await;
    if b.password.is_empty() {
        app.store.delete_repeater_password(&id).await?;
    } else {
        if !app.store.credentials_enabled() {
            return Err(ApiError(
                StatusCode::SERVICE_UNAVAILABLE,
                "CREWAWARE_SECRET_KEY is not set, so repeater passwords cannot be stored".into(),
            ));
        }
        app.store.set_repeater_password(&id, &b.password).await?;
    }
    let has = !b.password.is_empty();
    app.with(|e| e.set_has_credentials(&id, has))?;
    app.touch();
    ok()
}

/// Queues an immediate read of one repeater. Admin only, and rate limited per repeater.
async fn refresh_repeater(State(app): Shared, h: HeaderMap, Path(id): Path<String>) -> Res {
    require_admin(&app, &h).await?;
    match app.read(|e| e.repeater_exists(&id).then(|| e.credentialed_repeaters().contains(&id))) {
        None => return Err(EngineError::NotFound.into()),
        Some(false) => return Err(bad("no password is stored for this repeater")),
        Some(true) => {}
    }
    match app.request_refresh(&id) {
        Ok(()) => Ok(Json(json!({ "ok": true, "queued": true }))),
        Err(RefreshError::NoGateway) => Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no radio gateway is configured yet, so repeaters cannot be read".into(),
        )),
        Err(RefreshError::TooSoon(secs)) => Err(ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            format!("this repeater was read a moment ago, try again in {secs} s"),
        )),
    }
}

#[derive(Deserialize)]
struct SiteBody {
    name: String,
    a: Anchor,
    b: Anchor,
    #[serde(default)]
    slogan: String,
}

async fn set_site(State(app): Shared, h: HeaderMap, Json(b): Json<SiteBody>) -> Res {
    require_admin(&app, &h).await?;
    if b.slogan.chars().count() > 60 {
        return Err(bad("slogan is over 60 characters"));
    }
    if b.name.trim().is_empty() || b.name.chars().count() > 80 {
        return Err(bad("site name must be 1 to 80 characters"));
    }
    let ok_num = [b.a.px, b.a.py, b.a.lat, b.a.lon, b.b.px, b.b.py, b.b.lat, b.b.lon]
        .iter()
        .all(|v| v.is_finite());
    if !ok_num {
        return Err(bad("anchors must be numbers"));
    }
    app.with(|e| {
        e.update_site(|s| {
            s.name = b.name.trim().to_string();
            s.a = b.a;
            s.b = b.b;
            s.slogan = b.slogan.trim().to_string();
        })
    });
    ok()
}

async fn set_settings(State(app): Shared, h: HeaderMap, Json(s): Json<Settings>) -> Res {
    require_admin(&app, &h).await?;
    app.with(|e| e.set_settings(s))?;
    ok()
}

#[derive(Deserialize)]
struct PositionBody {
    unit: String,
    lat: f64,
    lon: f64,
    speed_mph: Option<f64>,
    battery: Option<i32>,
}

/// For tools and tests. A real radio gateway calls the engine directly, so this is admin-only.
async fn inject_position(State(app): Shared, h: HeaderMap, Json(b): Json<PositionBody>) -> Res {
    require_admin(&app, &h).await?;
    let p = Position {
        unit_id: b.unit,
        lat: b.lat,
        lon: b.lon,
        speed_mph: b.speed_mph,
        battery: b.battery.unwrap_or(-1),
        age_secs: 0,
    };
    app.with(|e| e.report_position(p));
    ok()
}

// ---- uploads: raster images only, sniffed from the bytes ----

fn sniff(b: &[u8]) -> Option<(&'static str, &'static str)> {
    if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(("image/png", "png"))
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(("image/jpeg", "jpg"))
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(("image/gif", "gif"))
    } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

#[derive(Deserialize)]
struct UploadQ {
    w: Option<f64>,
    h: Option<f64>,
}

async fn upload(app: &App, h: &HeaderMap, body: Bytes, kind: &str, q: UploadQ) -> Res {
    require_admin(app, h).await?;
    let (ct, ext) = sniff(&body).ok_or_else(|| bad("use a PNG, JPEG, GIF or WebP image"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("{kind}-{nanos}.{ext}");
    app.store.put_asset(&name, ct, &body).await?;
    let is_logo = kind == "logo";
    let n = name.clone();
    app.with(|e| {
        e.update_site(|s| {
            if is_logo {
                s.logo = n;
            } else {
                s.overlay = n;
                if let (Some(w), Some(h)) = (q.w, q.h) {
                    if w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite() {
                        s.img_w = w;
                        s.img_h = h;
                    }
                }
            }
        })
    });
    // Drop images the site no longer points at. Wait for the site row to be saved first.
    app.flush().await;
    prune_assets(app).await?;
    Ok(Json(json!({ "file": name })))
}

/// Drops uploaded images that neither the active site nor a saved site uses.
async fn prune_assets(app: &App) -> Result<(), ApiError> {
    let (ov, lg) = app.read(|e| (e.site().overlay.clone(), e.site().logo.clone()));
    let mut keep = app.store.profile_assets().await?;
    keep.push(ov);
    keep.push(lg);
    let keep: Vec<&str> = keep.iter().map(String::as_str).collect();
    app.store.prune_assets(&keep).await?;
    Ok(())
}

async fn upload_overlay(State(app): Shared, h: HeaderMap, Query(q): Query<UploadQ>, body: Bytes) -> Res {
    upload(&app, &h, body, "overlay", q).await
}

async fn upload_logo(State(app): Shared, h: HeaderMap, Query(q): Query<UploadQ>, body: Bytes) -> Res {
    upload(&app, &h, body, "logo", q).await
}

// ---- radio nodes ----

#[derive(Deserialize)]
struct NodeBody {
    key: String,
}

async fn set_unit_node(State(app): Shared, h: HeaderMap, Path(id): Path<String>, Json(b): Json<NodeBody>) -> Res {
    require_admin(&app, &h).await?;
    app.with(|e| e.set_unit_key(&id, &b.key))?;
    ok()
}

async fn set_repeater_node(State(app): Shared, h: HeaderMap, Path(id): Path<String>, Json(b): Json<NodeBody>) -> Res {
    require_admin(&app, &h).await?;
    app.with(|e| e.set_repeater_key(&id, &b.key))?;
    ok()
}

async fn remove_unit(State(app): Shared, h: HeaderMap, Path(id): Path<String>) -> Res {
    let user = require_admin(&app, &h).await?;
    app.with(|e| e.remove_unit(&id, &user))?;
    ok()
}

async fn remove_repeater(State(app): Shared, h: HeaderMap, Path(id): Path<String>) -> Res {
    let user = require_admin(&app, &h).await?;
    app.with(|e| e.remove_repeater(&id, &user))?;
    ok()
}

#[derive(Deserialize)]
struct AdoptBody {
    /// "unit" or "repeater".
    #[serde(rename = "as")]
    kind: String,
    name: String,
    #[serde(default)]
    group: String,
}

/// Turns a node the radio has heard into a unit or a repeater.
async fn adopt(State(app): Shared, h: HeaderMap, Path(key): Path<String>, Json(b): Json<AdoptBody>) -> Res {
    require_admin(&app, &h).await?;
    let name = b.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 60 {
        return Err(bad("name must be 1 to 60 characters"));
    }
    // What the radio already knows about this node, so it appears on the map straight away
    // instead of waiting for its next advert.
    let known = app.discovered(&key);
    let age = known.as_ref().map(|d| d.heard.elapsed().as_secs());
    let id = match b.kind.as_str() {
        "unit" => app.with(|e| -> Result<String, EngineError> {
            let id = e.adopt_unit(&key, &name, &b.group)?;
            if let (Some(d), Some(age)) = (&known, age) {
                if d.has_pos {
                    e.report_position(Position {
                        unit_id: id.clone(),
                        lat: d.lat,
                        lon: d.lon,
                        speed_mph: None,
                        battery: -1,
                        age_secs: age,
                    });
                } else {
                    e.report_checkin(&id, age);
                }
            }
            Ok(id)
        })?,
        "repeater" => app.with(|e| -> Result<String, EngineError> {
            let id = e.adopt_repeater(&key, &name)?;
            if let (Some(d), Some(age)) = (&known, age) {
                if d.has_pos {
                    e.set_repeater(&id, &name, d.lat, d.lon, true)?;
                }
                e.report_repeater_aged(&id, age);
            }
            Ok(id)
        })?,
        _ => return Err(bad("adopt as unit or repeater")),
    };
    Ok(Json(json!({ "ok": true, "id": id })))
}

/// Asks the radio to broadcast its own advert, so nearby nodes learn it is there.
async fn radio_advert(State(app): Shared, h: HeaderMap) -> Res {
    require_admin(&app, &h).await?;
    let gw = app.gateway().ok_or(ApiError(
        StatusCode::SERVICE_UNAVAILABLE,
        "no radio is configured".into(),
    ))?;
    gw.send_advert()
        .await
        .map_err(|e| ApiError(StatusCode::BAD_GATEWAY, e.to_string()))?;
    ok()
}

// ---- obstacles ----

async fn add_obstacle(
    State(app): Shared,
    h: HeaderMap,
    Json(mut o): Json<Obstacle>,
) -> Result<Json<Obstacle>, ApiError> {
    let user = require_admin(&app, &h).await?;
    o.id = 0;
    Ok(Json(app.with(|e| e.set_obstacle(o, &user))?))
}

async fn update_obstacle(
    State(app): Shared,
    h: HeaderMap,
    Path(id): Path<i32>,
    Json(mut o): Json<Obstacle>,
) -> Result<Json<Obstacle>, ApiError> {
    let user = require_admin(&app, &h).await?;
    o.id = id;
    Ok(Json(app.with(|e| e.set_obstacle(o, &user))?))
}

async fn delete_obstacle(State(app): Shared, h: HeaderMap, Path(id): Path<i32>) -> Res {
    let user = require_admin(&app, &h).await?;
    app.with(|e| e.delete_obstacle(id, &user))?;
    ok()
}

#[derive(Deserialize)]
struct GateBody {
    open: bool,
}

/// Dispatchers open and close gates as the event changes, so this does not need an admin login.
async fn set_gate(State(app): Shared, h: HeaderMap, Path(id): Path<i32>, Json(b): Json<GateBody>) -> Res {
    let seat = seat(&h);
    app.with(|e| e.set_gate(id, b.open, &seat))?;
    ok()
}

// ---- saved sites ----

async fn list_profiles(State(app): Shared, h: HeaderMap) -> Res {
    require_admin(&app, &h).await?;
    let active = app.read(|e| e.site().name.clone());
    let list: Vec<Value> = app
        .store
        .list_profiles()
        .await?
        .into_iter()
        .map(|(id, name, at)| json!({ "id": id, "name": name, "saved_at": at, "active": name == active }))
        .collect();
    Ok(Json(json!({ "profiles": list })))
}

/// Saves the active site (overlay pinning, obstacles, repeater positions) under its name.
async fn save_profile(State(app): Shared, h: HeaderMap) -> Res {
    require_admin(&app, &h).await?;
    app.flush().await;
    let data = app.read(|e| e.export_site());
    let id = app.store.save_profile(&data).await?;
    Ok(Json(json!({ "ok": true, "id": id, "name": data.site.name })))
}

async fn activate_profile(State(app): Shared, h: HeaderMap, Path(id): Path<i32>) -> Res {
    let user = require_admin(&app, &h).await?;
    let data = app
        .store
        .get_profile(id)
        .await?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such saved site".into()))?;
    // Switching away from a site that was never saved would lose its obstacles and positions.
    app.with(|e| e.activate_site(data, &user))?;
    ok()
}

async fn delete_profile(State(app): Shared, h: HeaderMap, Path(id): Path<i32>) -> Res {
    require_admin(&app, &h).await?;
    if !app.store.delete_profile(id).await? {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such saved site".into()));
    }
    prune_assets(&app).await?;
    ok()
}

const EXPORT_FORMAT: &str = "crewaware-site/1";

async fn image_for_export(app: &App, name: &str) -> Result<Value, ApiError> {
    if name.is_empty() {
        return Ok(Value::Null);
    }
    Ok(match app.store.get_asset(name).await? {
        Some((ct, data)) => json!({
            "content_type": ct,
            "base64": base64::engine::general_purpose::STANDARD.encode(data),
        }),
        None => Value::Null,
    })
}

async fn export_data(app: &App, data: SiteData) -> Result<Response, ApiError> {
    let overlay = image_for_export(app, &data.site.overlay).await?;
    let logo = image_for_export(app, &data.site.logo).await?;
    let fname: String = data
        .site
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let body = serde_json::to_vec_pretty(&json!({
        "format": EXPORT_FORMAT,
        "data": data,
        "overlay": overlay,
        "logo": logo,
    }))
    .map_err(anyhow::Error::from)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"crewaware-site-{fname}.json\""),
            ),
        ],
        body,
    )
        .into_response())
}

async fn export_profile(State(app): Shared, h: HeaderMap, Path(id): Path<i32>) -> Result<Response, ApiError> {
    require_admin(&app, &h).await?;
    let data = app
        .store
        .get_profile(id)
        .await?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such saved site".into()))?;
    export_data(&app, data).await
}

async fn export_current(State(app): Shared, h: HeaderMap) -> Result<Response, ApiError> {
    require_admin(&app, &h).await?;
    app.flush().await;
    let data = app.read(|e| e.export_site());
    export_data(&app, data).await
}

/// Image kind, file extension and bytes.
type DecodedImage = (&'static str, &'static str, Vec<u8>);

fn decode_image(v: &Value, what: &str) -> Result<Option<DecodedImage>, ApiError> {
    if v.is_null() {
        return Ok(None);
    }
    let b64 = v
        .get("base64")
        .and_then(Value::as_str)
        .ok_or_else(|| bad(&format!("{what} image is malformed")))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| bad(&format!("{what} image is not valid base64")))?;
    if bytes.len() > 25 << 20 {
        return Err(bad(&format!("{what} image is over 25 MB")));
    }
    let (ct, ext) = sniff(&bytes).ok_or_else(|| bad(&format!("{what} is not a PNG, JPEG, GIF or WebP image")))?;
    Ok(Some((ct, ext, bytes)))
}

/// Adds a site exported from another CrewAware as a saved site. It is not activated.
async fn import_profile(State(app): Shared, h: HeaderMap, body: Bytes) -> Res {
    require_admin(&app, &h).await?;
    let v: Value = serde_json::from_slice(&body).map_err(|_| bad("that file is not valid JSON"))?;
    if v.get("format").and_then(Value::as_str) != Some(EXPORT_FORMAT) {
        return Err(bad("that is not a CrewAware site file"));
    }
    let mut data: SiteData = serde_json::from_value(v.get("data").cloned().unwrap_or(Value::Null))
        .map_err(|e| bad(&format!("site data is malformed: {e}")))?;
    if data.site.name.trim().is_empty() || data.site.name.chars().count() > 80 {
        return Err(bad("site name must be 1 to 80 characters"));
    }
    if data.site.slogan.chars().count() > 60 || data.obstacles.len() > 500 || data.repeaters.len() > 200 {
        return Err(bad("site file is out of range"));
    }
    let numbers_ok = [
        data.site.img_w,
        data.site.img_h,
        data.site.a.px,
        data.site.a.py,
        data.site.a.lat,
        data.site.a.lon,
        data.site.b.px,
        data.site.b.py,
        data.site.b.lat,
        data.site.b.lon,
    ]
    .iter()
    .all(|n| n.is_finite())
        && data.site.img_w > 0.0
        && data.site.img_h > 0.0;
    if !numbers_ok {
        return Err(bad("site numbers are out of range"));
    }
    for o in &data.obstacles {
        if o.points.len() > 200 || o.points.iter().any(|p| !(p[0].is_finite() && p[1].is_finite())) {
            return Err(bad("an obstacle has bad points"));
        }
    }
    let overlay = decode_image(v.get("overlay").unwrap_or(&Value::Null), "overlay")?;
    let logo = decode_image(v.get("logo").unwrap_or(&Value::Null), "logo")?;
    // Assets get fresh names, so an import can never overwrite an image another site uses.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    data.site.overlay.clear();
    data.site.logo.clear();
    if let Some((ct, ext, bytes)) = overlay {
        let n = format!("overlay-{nanos}.{ext}");
        app.store.put_asset(&n, ct, &bytes).await?;
        data.site.overlay = n;
    }
    if let Some((ct, ext, bytes)) = logo {
        let n = format!("logo-{}.{ext}", nanos + 1);
        app.store.put_asset(&n, ct, &bytes).await?;
        data.site.logo = n;
    }
    // Never overwrite a saved site of the same name by accident.
    let taken: Vec<String> = app.store.list_profiles().await?.into_iter().map(|p| p.1).collect();
    let base = data.site.name.trim().to_string();
    let mut name = base.clone();
    let mut n = 2;
    while taken.contains(&name) {
        name = format!("{base} ({n})");
        n += 1;
    }
    data.site.name = name.clone();
    let id = app.store.save_profile(&data).await?;
    Ok(Json(json!({ "ok": true, "id": id, "name": name })))
}

// ---- reports ----

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn csv_cell(s: &str) -> String {
    // Stop spreadsheet programs treating a cell as a formula.
    let s = if s.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s
    }
}

fn fmt_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

fn duration_text(a: chrono::DateTime<chrono::Utc>, b: chrono::DateTime<chrono::Utc>) -> String {
    let s = (b - a).num_seconds().max(0);
    format!("{} min {} s", s / 60, s % 60)
}

/// A printable page for one incident: who opened it, who was sent, and the timeline.
async fn incident_report(State(app): Shared, Path(id): Path<i32>) -> Result<Response, ApiError> {
    let (inc, site, names) = app
        .read(|e| {
            e.incident(id).map(|i| {
                let names: Vec<String> = i.units.iter().map(|u| e.unit_name(u)).collect();
                (i, e.site().name.clone(), names)
            })
        })
        .ok_or(ApiError(StatusCode::NOT_FOUND, "no such incident".into()))?;
    app.flush().await;
    let log = app.store.incident_log(id).await?;
    let mut rows = String::new();
    for l in &log {
        rows.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
            fmt_time(l.t),
            esc(&l.seat),
            esc(&l.text)
        ));
    }
    let closed = match inc.closed {
        Some(c) => format!("{} ({})", fmt_time(c), duration_text(inc.opened, c)),
        None => "still open".into(),
    };
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title><style>\
         body{{font:14px/1.45 system-ui,sans-serif;margin:32px;color:#111}}h1{{font-size:20px;margin:0 0 4px}}\
         .sub{{color:#555;margin-bottom:18px}}dl{{display:grid;grid-template-columns:140px 1fr;gap:4px 12px;margin:0 0 20px}}\
         dt{{color:#555}}dd{{margin:0}}table{{border-collapse:collapse;width:100%}}\
         th,td{{text-align:left;border-bottom:1px solid #ddd;padding:5px 8px;vertical-align:top}}th{{color:#555;font-weight:600}}\
         @media print{{body{{margin:12mm}}}}</style></head><body>\
         <h1>{title}</h1><div class=\"sub\">{site}</div>\
         <dl><dt>Opened</dt><dd>{opened} by {by}</dd><dt>Closed</dt><dd>{closed}</dd>\
         <dt>Location</dt><dd>{lat:.5}, {lon:.5}</dd><dt>Units sent</dt><dd>{units}</dd></dl>\
         <table><thead><tr><th>Time</th><th>Seat</th><th>Event</th></tr></thead><tbody>{rows}</tbody></table>\
         </body></html>",
        title = esc(&inc.label),
        site = esc(&site),
        opened = fmt_time(inc.opened),
        by = esc(&inc.opened_by),
        closed = esc(&closed),
        lat = inc.lat,
        lon = inc.lon,
        units = if names.is_empty() { "none".into() } else { esc(&names.join(", ")) },
        rows = rows,
    );
    Ok((
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        html,
    )
        .into_response())
}

async fn incidents_csv(State(app): Shared) -> Response {
    let (list, site, names) = app.read(|e| {
        let list = e.all_incidents();
        let names: Vec<Vec<String>> = list
            .iter()
            .map(|i| i.units.iter().map(|u| e.unit_name(u)).collect())
            .collect();
        (list, e.site().name.clone(), names)
    });
    let mut out = String::from("id,site,label,lat,lon,opened_utc,closed_utc,minutes_open,opened_by,units\r\n");
    for (i, n) in list.iter().zip(names) {
        let mins = i
            .closed
            .map(|c| format!("{:.1}", (c - i.opened).num_seconds().max(0) as f64 / 60.0))
            .unwrap_or_default();
        out.push_str(&format!(
            "{},{},{},{:.6},{:.6},{},{},{},{},{}\r\n",
            i.id,
            csv_cell(&site),
            csv_cell(&i.label),
            i.lat,
            i.lon,
            i.opened.to_rfc3339(),
            i.closed.map(|c| c.to_rfc3339()).unwrap_or_default(),
            mins,
            csv_cell(&i.opened_by),
            csv_cell(&n.join("; ")),
        ));
    }
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"crewaware-incidents.csv\"",
            ),
        ],
        out,
    )
        .into_response()
}
