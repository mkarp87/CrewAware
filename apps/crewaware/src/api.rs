use crate::app::{App, RefreshError};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use crewaware_core::{Anchor, EngineError, Group, Position, Settings, Status};
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
        .route("/api/units/{id}/group", post(assign_group))
        .route("/api/groups", post(set_group))
        .route("/api/repeaters", post(set_repeater))
        .route("/api/repeaters/{id}/credentials", post(set_credentials))
        .route("/api/repeaters/{id}/refresh", post(refresh_repeater))
        .route("/api/site", post(set_site))
        .route("/api/settings", post(set_settings))
        .route("/api/position", post(inject_position))
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

async fn state(State(app): Shared) -> Json<crewaware_core::Snapshot> {
    Json(app.snapshot())
}

/// Live updates over Server-Sent Events: a full snapshot on every change and every 2 s, so ages
/// stay fresh without a change.
async fn events(State(app): Shared) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = app.subscribe();
    let iv = tokio::time::interval(Duration::from_secs(2));
    let s = futures_util::stream::unfold((app, rx, iv), |(app, mut rx, mut iv)| async move {
        tokio::select! { _ = rx.recv() => {}, _ = iv.tick() => {} }
        let json = serde_json::to_string(&app.snapshot()).ok()?;
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
    #[serde(default)]
    speed_mph: f64,
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
    let (ov, lg) = app.read(|e| (e.site().overlay.clone(), e.site().logo.clone()));
    app.store.prune_assets(&[ov.as_str(), lg.as_str()]).await?;
    Ok(Json(json!({ "file": name })))
}

async fn upload_overlay(State(app): Shared, h: HeaderMap, Query(q): Query<UploadQ>, body: Bytes) -> Res {
    upload(&app, &h, body, "overlay", q).await
}

async fn upload_logo(State(app): Shared, h: HeaderMap, Query(q): Query<UploadQ>, body: Bytes) -> Res {
    upload(&app, &h, body, "logo", q).await
}
