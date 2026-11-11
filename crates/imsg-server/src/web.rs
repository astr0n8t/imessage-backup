//! HTTP routes.

use std::collections::HashMap;
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

use crate::auth::{
    self, Accounts, COOKIE, PENDING_COOKIE, PENDING_TTL, PendingCheck, PendingLogins, Throttle,
};
use crate::index::{Index, IngestReport};
use crate::media::{self, ServeRequest, Tools};
use crate::queries::{self, Cursor, Names};
use crate::totp::ReplayGuard;
use crate::view::View;

pub struct AppState {
    pub index: Index,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub accounts: Accounts,
    pub throttle: Throttle,
    pub pending: PendingLogins,
    pub totp_replay: ReplayGuard,
    /// Contact names per account view, rebuilt lazily after each ingest.
    pub names: RwLock<HashMap<String, Arc<Names>>>,
    pub tools: Tools,
    pub secure_cookies: bool,
    pub trust_forwarded_for: bool,
    pub session_ttl: Duration,
    pub ingest_lock: tokio::sync::Mutex<()>,
    pub last_ingest: RwLock<Option<(chrono::DateTime<chrono::Utc>, IngestReport)>>,
}

type St = State<Arc<AppState>>;

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        index: Index,
        data: PathBuf,
        cache: PathBuf,
        accounts: Accounts,
        tools: Tools,
        secure_cookies: bool,
        trust_forwarded_for: bool,
        session_ttl: Duration,
    ) -> Self {
        Self {
            index,
            data,
            cache,
            accounts,
            throttle: Throttle::default(),
            pending: PendingLogins::default(),
            totp_replay: ReplayGuard::default(),
            names: RwLock::new(HashMap::new()),
            tools,
            secure_cookies,
            trust_forwarded_for,
            session_ttl,
            ingest_lock: tokio::sync::Mutex::new(()),
            last_ingest: RwLock::new(None),
        }
    }

    /// Ingest new snapshots (serialized) and drop cached contact names.
    pub async fn ingest(self: &Arc<Self>) -> anyhow::Result<IngestReport> {
        let _guard = self.ingest_lock.lock().await;
        let me = self.clone();
        let report = tokio::task::spawn_blocking(move || me.index.ingest_all(&me.data)).await??;
        self.names.write().unwrap().clear();
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

/// Run a blocking closure with a plain connection to the index.
async fn with_conn<T: Send + 'static>(
    st: &Arc<AppState>,
    f: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, AppError> {
    let st = st.clone();
    Ok(tokio::task::spawn_blocking(move || f(&st.index.read()?)).await??)
}

/// Run a blocking closure with a connection restricted to the viewer's view.
async fn with_view<T: Send + 'static>(
    st: &Arc<AppState>,
    viewer: &Viewer,
    f: impl FnOnce(&rusqlite::Connection, &View, &Names) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, AppError> {
    let st = st.clone();
    let (key, view) = (viewer.viewing.clone(), viewer.view.clone());
    Ok(tokio::task::spawn_blocking(move || {
        let conn = st.index.read()?;
        view.install(&conn)?;
        let cached = st.names.read().unwrap().get(&key).cloned();
        let names = match cached {
            Some(n) => n,
            None => {
                let n = Arc::new(Names::load(&conn, &view)?);
                st.names.write().unwrap().insert(key, n.clone());
                n
            }
        };
        f(&conn, &view, &names)
    })
    .await??)
}

/// The signed-in account and the account whose data it is looking at
/// (admins may pass `?as=<account>` to look at another account's view).
pub struct Viewer {
    pub account: String,
    pub admin: bool,
    pub viewing: String,
    pub view: View,
}

impl FromRequestParts<Arc<AppState>> for Viewer {
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
        let user = with_conn(st, move |c| auth::session_user(c, &token)).await?;
        // Accounts removed from the accounts file lose access immediately.
        let Some(account) = user.and_then(|u| st.accounts.get(&u)) else {
            return Err(AppError::Status(
                StatusCode::UNAUTHORIZED,
                "session expired",
            ));
        };
        let wanted = parts.uri.query().and_then(|q| {
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "as")
                .map(|(_, v)| v.to_owned())
        });
        let target = match wanted.as_deref() {
            None | Some("") => account,
            Some(name) if name == account.name => account,
            Some(name) if account.admin => st
                .accounts
                .get(name)
                .ok_or(AppError::Status(StatusCode::NOT_FOUND, "no such account"))?,
            Some(_) => return Err(AppError::Status(StatusCode::FORBIDDEN, "admins only")),
        };
        Ok(Viewer {
            account: account.name.clone(),
            admin: account.admin,
            viewing: target.name.clone(),
            view: target.view.clone(),
        })
    }
}

const INDEX_HTML: &str = include_str!("../static/index.html");
const LOGIN_HTML: &str = include_str!("../static/login.html");
const TOTP_HTML: &str = include_str!("../static/totp.html");
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
        .route("/login/totp", get(totp_page).post(login_totp))
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
        .route("/api/me", get(api_me))
        .route("/api/identities", get(api_identities))
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

async fn index_page(user: Result<Viewer, AppError>) -> Response {
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
        Some("expired") => {
            "Your sign-in timed out or had too many wrong codes. Please sign in again."
        }
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
    let ok = tokio::task::spawn_blocking(move || st2.accounts.verify(&user, &pass)).await?;
    if !ok {
        st.throttle.fail(ip);
        warn!(%ip, user = %form.username, "failed login");
        tokio::time::sleep(Duration::from_millis(500)).await;
        return Ok(Redirect::to("/login?error=1").into_response());
    }
    let needs_code = st
        .accounts
        .get(&form.username)
        .is_some_and(|a| a.totp.is_some());
    if needs_code {
        // Don't reset the failure counter yet: the code still has to be right.
        let token = st.pending.start(&form.username);
        info!(%ip, user = %form.username, "password accepted; waiting for TOTP code");
        let mut resp = Redirect::to("/login/totp").into_response();
        resp.headers_mut().insert(
            header::SET_COOKIE,
            pending_cookie(&st, &token, PENDING_TTL.as_secs()),
        );
        return Ok(resp);
    }
    finish_login(&st, ip, form.username).await
}

/// Create the session after every factor checked out.
async fn finish_login(st: &Arc<AppState>, ip: IpAddr, user: String) -> Result<Response, AppError> {
    st.throttle.succeed(ip);
    let ttl = st.session_ttl;
    let u = user.clone();
    let token = with_conn(st, move |c| auth::create_session(c, &u, ttl)).await?;
    info!(%ip, %user, "signed in");
    let mut resp = Redirect::to("/").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        session_cookie(st, &token, ttl.as_secs()),
    );
    resp.headers_mut()
        .append(header::SET_COOKIE, pending_cookie(st, "", 0));
    Ok(resp)
}

fn pending_cookie(st: &AppState, value: &str, max_age: u64) -> HeaderValue {
    let secure = if st.secure_cookies { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{PENDING_COOKIE}={value}; Path=/login; HttpOnly; SameSite=Strict; Max-Age={max_age}{secure}"
    ))
    .unwrap()
}

fn pending_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|h| auth::named_cookie_token(h, PENDING_COOKIE))
        .map(str::to_owned)
}

async fn totp_page(headers: HeaderMap, Query(q): Query<LoginQuery>) -> Response {
    if pending_token(&headers).is_none() {
        return Redirect::to("/login").into_response();
    }
    let msg = match q.error.as_deref() {
        Some("locked") => "Too many failed attempts. Try again later.",
        Some(_) => "That code didn't work. Check your authenticator app and try again.",
        None => "",
    };
    no_store(Html(TOTP_HTML.replace("{{error}}", msg)).into_response())
}

#[derive(Deserialize)]
struct TotpForm {
    code: String,
}

async fn login_totp(
    State(st): St,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(form): Form<TotpForm>,
) -> Result<Response, AppError> {
    let ip = client_ip(&st, &headers, peer);
    if st.throttle.blocked(ip) {
        return Ok(Redirect::to("/login/totp?error=locked").into_response());
    }
    let Some(token) = pending_token(&headers) else {
        return Ok(Redirect::to("/login?error=expired").into_response());
    };
    let user = match st.pending.attempt(&token) {
        PendingCheck::User(u) => u,
        PendingCheck::Missing => {
            let mut resp = Redirect::to("/login?error=expired").into_response();
            resp.headers_mut()
                .insert(header::SET_COOKIE, pending_cookie(&st, "", 0));
            return Ok(resp);
        }
    };
    let step = st
        .accounts
        .get(&user)
        .and_then(|a| a.totp.as_ref())
        .and_then(|secret| secret.matching_step(&form.code, crate::totp::now()));
    match step {
        Some(step) if st.totp_replay.accept(&user, step) => {
            st.pending.finish(&token);
            finish_login(&st, ip, user).await
        }
        _ => {
            st.throttle.fail(ip);
            warn!(%ip, %user, "wrong or reused TOTP code");
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(Redirect::to("/login/totp?error=1").into_response())
        }
    }
}

async fn logout(State(st): St, headers: HeaderMap) -> Result<Response, AppError> {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(auth::cookie_token)
        .map(str::to_owned)
    {
        with_conn(&st, move |c| auth::delete_session(c, &token)).await?;
    }
    let mut resp = Redirect::to("/login").into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, session_cookie(&st, "", 0));
    Ok(resp)
}

async fn api_me(v: Viewer, State(st): St) -> Result<Response, AppError> {
    let accounts: Vec<serde_json::Value> = if v.admin {
        st.accounts
            .iter()
            .map(|a| {
                serde_json::json!({
                    "name": a.name, "admin": a.admin, "devices": a.devices, "imessage_ids": a.imessage_ids,
                    "totp": a.totp.is_some(),
                })
            })
            .collect()
    } else {
        vec![]
    };
    Ok(no_store(
        Json(serde_json::json!({
            "name": v.account, "admin": v.admin, "viewing": v.viewing, "accounts": accounts,
            "totp": st.accounts.get(&v.account).is_some_and(|a| a.totp.is_some()),
        }))
        .into_response(),
    ))
}

async fn api_identities(v: Viewer, State(st): St) -> Result<Response, AppError> {
    if !v.admin {
        return Err(AppError::Status(StatusCode::FORBIDDEN, "admins only"));
    }
    let ids = with_conn(&st, queries::identities).await?;
    Ok(no_store(Json(ids).into_response()))
}

async fn api_conversations(v: Viewer, State(st): St) -> Result<Response, AppError> {
    let out = with_view(&st, &v, queries::conversations).await?;
    Ok(no_store(Json(out).into_response()))
}

async fn api_conversation(
    v: Viewer,
    State(st): St,
    UrlPath(id): UrlPath<i64>,
) -> Result<Response, AppError> {
    match with_view(&st, &v, move |c, view, n| {
        queries::conversation(c, view, n, id)
    })
    .await?
    {
        Some(out) => Ok(no_store(Json(out).into_response())),
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
    v: Viewer,
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
    let out = with_view(&st, &v, move |c, view, n| {
        queries::messages(c, view, n, id, cursor, limit)
    })
    .await?;
    Ok(no_store(Json(out).into_response()))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    conv: Option<i64>,
    limit: Option<i64>,
}

async fn api_search(
    v: Viewer,
    State(st): St,
    Query(q): Query<SearchQuery>,
) -> Result<Response, AppError> {
    let out = with_view(&st, &v, move |c, view, n| {
        queries::search(c, view, n, &q.q, q.conv, q.limit.unwrap_or(100))
    })
    .await?;
    Ok(no_store(Json(out).into_response()))
}

async fn api_status(v: Viewer, State(st): St) -> Result<Response, AppError> {
    let mut out = serde_json::to_value(with_view(&st, &v, queries::status).await?)?;
    if let Some((at, r)) = st.last_ingest.read().unwrap().clone() {
        out["last_scan"] = serde_json::json!({ "at": at, "report": r });
    }
    out["transcoding"] = st.tools.describe().into();
    Ok(no_store(Json(out).into_response()))
}

async fn api_rescan(_v: Viewer, State(st): St) -> Result<Response, AppError> {
    let r = st.ingest().await?;
    Ok(no_store(Json(r).into_response()))
}

#[derive(Deserialize)]
struct MediaQuery {
    dl: Option<u8>,
    raw: Option<u8>,
}

async fn media_file(
    v: Viewer,
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
    let rows = with_view(&st, &v, move |c, view, _| {
        queries::attachment(c, view, &fid)
    })
    .await?;
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
