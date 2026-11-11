//! HTTP routes.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::extract::{ConnectInfo, FromRequestParts, Path as UrlPath, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use tracing::{info, warn};

use crate::auth::{self, COOKIE, Throttle, Users};
use crate::index::{Index, IngestReport};
use crate::media::{self, ServeRequest, Tools};
use crate::queries::{self, Cursor, Names};

pub struct AppState {
    pub index: Index,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub users: Users,
    pub throttle: Throttle,
    pub names: RwLock<Arc<Names>>,
    pub tools: Tools,
    pub secure_cookies: bool,
    pub trust_forwarded_for: bool,
    pub session_ttl: Duration,
    pub ingest_lock: tokio::sync::Mutex<()>,
    pub last_ingest: RwLock<Option<(chrono::DateTime<chrono::Utc>, IngestReport)>>,
}

type St = State<Arc<AppState>>;

impl AppState {
    pub fn names(&self) -> Arc<Names> {
        self.names.read().unwrap().clone()
    }

    /// Ingest new snapshots (serialized) and refresh cached contact names.
    pub async fn ingest(self: &Arc<Self>) -> anyhow::Result<IngestReport> {
        let _guard = self.ingest_lock.lock().await;
        let me = self.clone();
        let report = tokio::task::spawn_blocking(move || -> anyhow::Result<IngestReport> {
            let r = me.index.ingest_all(&me.data)?;
            let names = Names::load(&me.index.read()?)?;
            *me.names.write().unwrap() = Arc::new(names);
            Ok(r)
        })
        .await??;
        *self.last_ingest.write().unwrap() = Some((chrono::Utc::now(), report.clone()));
        Ok(report)
    }
}

pub enum AppError {
    Status(StatusCode, &'static str),
    Internal(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError::Internal(e.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            AppError::Status(code, msg) => {
                (code, Json(serde_json::json!({ "error": msg }))).into_response()
            }
            AppError::Internal(e) => {
                warn!("request failed: {e:#}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "internal error" })),
                )
                    .into_response()
            }
        }
    }
}

/// Run a blocking closure with a read connection to the index.
async fn with_db<T: Send + 'static>(
    st: &Arc<AppState>,
    f: impl FnOnce(&rusqlite::Connection, &Names) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, AppError> {
    let st = st.clone();
    Ok(tokio::task::spawn_blocking(move || {
        let conn = st.index.read()?;
        f(&conn, &st.names())
    })
    .await??)
}

/// The authenticated user, from the session cookie.
pub struct User(#[allow(dead_code)] pub String);

impl FromRequestParts<Arc<AppState>> for User {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        st: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(auth::cookie_token)
            .map(str::to_owned);
        let Some(token) = token else {
            return Err(AppError::Status(StatusCode::UNAUTHORIZED, "not signed in"));
        };
        let user = with_db(st, move |c, _| auth::session_user(c, &token)).await?;
        user.map(User).ok_or(AppError::Status(
            StatusCode::UNAUTHORIZED,
            "session expired",
        ))
    }
}

const INDEX_HTML: &str = include_str!("../static/index.html");
const LOGIN_HTML: &str = include_str!("../static/login.html");
const APP_JS: &str = include_str!("../static/app.js");
const APP_CSS: &str = include_str!("../static/app.css");

async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.entry(header::CONTENT_SECURITY_POLICY).or_insert(HeaderValue::from_static(
        "default-src 'self'; img-src 'self' data: blob:; media-src 'self' blob:; style-src 'self'; script-src 'self'; \
         object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
    ));
    h.entry("x-content-type-options")
        .or_insert(HeaderValue::from_static("nosniff"));
    h.entry("referrer-policy")
        .or_insert(HeaderValue::from_static("no-referrer"));
    h.entry("x-frame-options")
        .or_insert(HeaderValue::from_static("DENY"));
    resp
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index_page))
        .route("/login", get(login_page).post(login))
        .route("/logout", post(logout))
        .route(
            "/static/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    APP_JS,
                )
            }),
        )
        .route(
            "/static/app.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], APP_CSS) }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/conversations", get(api_conversations))
        .route("/api/conversations/{id}", get(api_conversation))
        .route("/api/conversations/{id}/messages", get(api_messages))
        .route("/api/search", get(api_search))
        .route("/api/status", get(api_status))
        .route("/api/rescan", post(api_rescan))
        .route("/media/{file_id}", get(media_file))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

fn no_store(mut r: Response) -> Response {
    r.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

async fn index_page(user: Result<User, AppError>) -> Response {
    match user {
        Ok(_) => no_store(Html(INDEX_HTML).into_response()),
        Err(_) => Redirect::to("/login").into_response(),
    }
}

#[derive(Deserialize)]
struct LoginQuery {
    error: Option<String>,
}

async fn login_page(Query(q): Query<LoginQuery>) -> Response {
    let msg = match q.error.as_deref() {
        Some("locked") => "Too many failed attempts. Try again later.",
        Some(_) => "Incorrect username or password.",
        None => "",
    };
    no_store(Html(LOGIN_HTML.replace("{{error}}", msg)).into_response())
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

fn client_ip(st: &AppState, headers: &HeaderMap, peer: SocketAddr) -> IpAddr {
    if st.trust_forwarded_for
        && let Some(ip) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse().ok())
    {
        return ip;
    }
    peer.ip()
}

fn session_cookie(st: &AppState, value: &str, max_age: u64) -> HeaderValue {
    let secure = if st.secure_cookies { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{COOKIE}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .unwrap()
}

async fn login(
    State(st): St,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> Result<Response, AppError> {
    let ip = client_ip(&st, &headers, peer);
    if st.throttle.blocked(ip) {
        return Ok(Redirect::to("/login?error=locked").into_response());
    }
    let st2 = st.clone();
    let (user, pass) = (form.username.clone(), form.password);
    let ok = tokio::task::spawn_blocking(move || st2.users.verify(&user, &pass)).await?;
    if !ok {
        st.throttle.fail(ip);
        warn!(%ip, user = %form.username, "failed login");
        tokio::time::sleep(Duration::from_millis(500)).await;
        return Ok(Redirect::to("/login?error=1").into_response());
    }
    st.throttle.succeed(ip);
    let ttl = st.session_ttl;
    let user = form.username.clone();
    let token = with_db(&st, move |c, _| auth::create_session(c, &user, ttl)).await?;
    info!(%ip, user = %form.username, "signed in");
    let mut resp = Redirect::to("/").into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        session_cookie(&st, &token, ttl.as_secs()),
    );
    Ok(resp)
}

async fn logout(State(st): St, headers: HeaderMap) -> Result<Response, AppError> {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(auth::cookie_token)
        .map(str::to_owned)
    {
        with_db(&st, move |c, _| auth::delete_session(c, &token)).await?;
    }
    let mut resp = Redirect::to("/login").into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, session_cookie(&st, "", 0));
    Ok(resp)
}

async fn api_conversations(_u: User, State(st): St) -> Result<Response, AppError> {
    let v = with_db(&st, queries::conversations).await?;
    Ok(no_store(Json(v).into_response()))
}

async fn api_conversation(
    _u: User,
    State(st): St,
    UrlPath(id): UrlPath<i64>,
) -> Result<Response, AppError> {
    match with_db(&st, move |c, n| queries::conversation(c, n, id)).await? {
        Some(v) => Ok(no_store(Json(v).into_response())),
        None => Err(AppError::Status(
            StatusCode::NOT_FOUND,
            "no such conversation",
        )),
    }
}

#[derive(Deserialize)]
struct MessagesQuery {
    before: Option<i64>,
    after: Option<i64>,
    around: Option<i64>,
    limit: Option<i64>,
}

async fn api_messages(
    _u: User,
    State(st): St,
    UrlPath(id): UrlPath<i64>,
    Query(q): Query<MessagesQuery>,
) -> Result<Response, AppError> {
    let cursor = match (q.around, q.before, q.after) {
        (Some(a), _, _) => Cursor::Around(a),
        (_, Some(b), _) => Cursor::Before(b),
        (_, _, Some(a)) => Cursor::After(a),
        _ => Cursor::Latest,
    };
    let limit = q.limit.unwrap_or(80);
    let v = with_db(&st, move |c, n| queries::messages(c, n, id, cursor, limit)).await?;
    Ok(no_store(Json(v).into_response()))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    conv: Option<i64>,
    limit: Option<i64>,
}

async fn api_search(
    _u: User,
    State(st): St,
    Query(q): Query<SearchQuery>,
) -> Result<Response, AppError> {
    let v = with_db(&st, move |c, n| {
        queries::search(c, n, &q.q, q.conv, q.limit.unwrap_or(100))
    })
    .await?;
    Ok(no_store(Json(v).into_response()))
}

async fn api_status(_u: User, State(st): St) -> Result<Response, AppError> {
    let mut v = serde_json::to_value(with_db(&st, |c, _| queries::status(c)).await?)?;
    if let Some((at, r)) = st.last_ingest.read().unwrap().clone() {
        v["last_scan"] = serde_json::json!({ "at": at, "report": r });
    }
    v["transcoding"] = st.tools.describe().into();
    Ok(no_store(Json(v).into_response()))
}

async fn api_rescan(_u: User, State(st): St) -> Result<Response, AppError> {
    let r = st.ingest().await?;
    Ok(no_store(Json(r).into_response()))
}

#[derive(Deserialize)]
struct MediaQuery {
    dl: Option<u8>,
    raw: Option<u8>,
}

async fn media_file(
    _u: User,
    State(st): St,
    UrlPath(file_id): UrlPath<String>,
    Query(q): Query<MediaQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if file_id.len() != 40 || !file_id.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(AppError::Status(
            StatusCode::BAD_REQUEST,
            "bad attachment id",
        ));
    }
    let fid = file_id.clone();
    let rows = with_db(&st, move |c, _| queries::attachment(c, &fid)).await?;
    // The same file may exist under several devices; use the first one present.
    let found = rows.into_iter().find_map(|(device, mime, name)| {
        let p = imsg_core::archive::attachment_path(
            &imsg_core::archive::device_dir(&st.data, &device),
            &file_id,
        );
        p.is_file().then_some((p, mime, name))
    });
    let Some((path, mime, name)) = found else {
        return Err(AppError::Status(
            StatusCode::NOT_FOUND,
            "attachment not in archive",
        ));
    };
    let req = ServeRequest {
        path,
        file_id: &file_id,
        mime: mime.as_deref(),
        name: name.as_deref(),
        download: q.dl == Some(1),
        raw: q.raw == Some(1),
    };
    Ok(media::serve(req, &headers, &st.tools, &st.cache).await)
}
