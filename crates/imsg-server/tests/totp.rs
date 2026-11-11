//! Two-factor (TOTP) login flow over HTTP.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use imsg_server::auth::{Accounts, hash_password};
use imsg_server::index::Index;
use imsg_server::media::Tools;
use imsg_server::totp::{self, STEP_SECS, Secret};
use imsg_server::web::{AppState, router};
use tower::ServiceExt;

fn app(tmp: &std::path::Path, secret: &Secret) -> Router {
    let data = tmp.join("archive");
    std::fs::create_dir_all(&data).unwrap();
    let h = hash_password("pw").unwrap();
    let accounts = Accounts::parse(&format!(
        "[[account]]\nname = \"alice\"\npassword = \"{h}\"\ndevices = [\"*\"]\ntotp_secret = \"{}\"\n\n\
         [[account]]\nname = \"bob\"\npassword = \"{h}\"\ndevices = [\"*\"]\n",
        secret.to_base32()
    ))
    .unwrap();
    let index = Index::open(&tmp.join(format!("state-{}/index.sqlite", rand_suffix()))).unwrap();
    let st = Arc::new(AppState::new(
        index,
        data,
        tmp.join("cache"),
        accounts,
        Tools::default(),
        false,
        false,
        Duration::from_secs(3600),
    ));
    router(st).layer(MockConnectInfo(SocketAddr::from(([10, 0, 0, 1], 1))))
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

async fn post(app: &Router, uri: &str, cookie: Option<&str>, body: String) -> Response<Body> {
    let mut req =
        Request::post(uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    app.clone()
        .oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

async fn get(app: &Router, uri: &str, cookie: Option<&str>) -> Response<Body> {
    let mut req = Request::get(uri);
    if let Some(c) = cookie {
        req = req.header(header::COOKIE, c);
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn location(r: &Response<Body>) -> &str {
    r.headers()[header::LOCATION].to_str().unwrap()
}

/// `name=value` of the cookie called `name` in the response, if set non-empty.
fn cookie(r: &Response<Body>, name: &str) -> Option<String> {
    r.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap().to_owned())
        .find(|kv| kv.starts_with(&format!("{name}=")) && kv.len() > name.len() + 1)
}

fn current_code(secret: &Secret) -> String {
    format!("{:06}", secret.code_at_step(totp::now() / STEP_SECS))
}

async fn password_step(app: &Router, user: &str) -> Response<Body> {
    post(app, "/login", None, format!("username={user}&password=pw")).await
}

#[tokio::test]
async fn totp_login_flow() {
    let tmp = tempfile::tempdir().unwrap();
    let secret = Secret::generate();
    let app = app(tmp.path(), &secret);

    // No secret: straight in.
    let r = password_step(&app, "bob").await;
    assert_eq!(location(&r), "/");
    assert!(cookie(&r, "imsg_session").is_some());

    // Secret: the password only gets you to the code page.
    let r = password_step(&app, "alice").await;
    assert_eq!(location(&r), "/login/totp");
    assert!(cookie(&r, "imsg_session").is_none());
    let pending = cookie(&r, "imsg_pending").unwrap();
    assert!(
        r.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("Path=/login")
    );
    assert_eq!(
        get(&app, "/api/me", Some(&pending)).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        get(&app, "/login/totp", Some(&pending)).await.status(),
        StatusCode::OK
    );
    assert_eq!(location(&get(&app, "/login/totp", None).await), "/login");

    // Wrong code, then the right one.
    let r = post(&app, "/login/totp", Some(&pending), "code=000000".into()).await;
    assert_eq!(location(&r), "/login/totp?error=1");
    let code = current_code(&secret);
    let r = post(&app, "/login/totp", Some(&pending), format!("code={code}")).await;
    assert_eq!(location(&r), "/");
    let session = cookie(&r, "imsg_session").unwrap();
    let me = get(&app, "/api/me", Some(&session)).await;
    assert_eq!(me.status(), StatusCode::OK);
    let me: serde_json::Value =
        serde_json::from_slice(&me.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(me["totp"], true);

    // The pending token is single-use, and the same code can't be replayed.
    let r = post(&app, "/login/totp", Some(&pending), format!("code={code}")).await;
    assert_eq!(location(&r), "/login?error=expired");
    let pending2 = cookie(&password_step(&app, "alice").await, "imsg_pending").unwrap();
    let r = post(&app, "/login/totp", Some(&pending2), format!("code={code}")).await;
    assert_eq!(
        location(&r),
        "/login/totp?error=1",
        "replayed code rejected"
    );
}

#[tokio::test]
async fn totp_attempts_are_limited_and_throttled() {
    let tmp = tempfile::tempdir().unwrap();
    let secret = Secret::generate();
    let app = app(tmp.path(), &secret);
    let wrong = |s: &Secret| {
        let c = s.code_at_step(totp::now() / STEP_SECS);
        format!("code={:06}", (c + 500_000) % 1_000_000)
    };

    // Five wrong codes use up a pending login; the password must be re-entered.
    let pending = cookie(&password_step(&app, "alice").await, "imsg_pending").unwrap();
    for _ in 0..5 {
        let r = post(&app, "/login/totp", Some(&pending), wrong(&secret)).await;
        assert_eq!(location(&r), "/login/totp?error=1");
    }
    let r = post(
        &app,
        "/login/totp",
        Some(&pending),
        format!("code={}", current_code(&secret)),
    )
    .await;
    assert_eq!(location(&r), "/login?error=expired");

    // Re-entering the correct password does not reset the failure counter, so
    // after 10 wrong codes the address is locked out, even with right answers.
    let pending = cookie(&password_step(&app, "alice").await, "imsg_pending").unwrap();
    for _ in 0..5 {
        post(&app, "/login/totp", Some(&pending), wrong(&secret)).await;
    }
    let r = password_step(&app, "alice").await;
    assert_eq!(location(&r), "/login?error=locked");
}
