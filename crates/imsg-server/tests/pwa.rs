//! Installable-app (PWA) assets: public, correctly typed and versioned.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use imsg_server::auth::{Accounts, hash_password};
use imsg_server::index::Index;
use imsg_server::media::Tools;
use imsg_server::web::{AppState, asset_version, router};
use tower::ServiceExt;

#[tokio::test]
async fn pwa_assets() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = Accounts::parse(&format!("a:{}", hash_password("pw").unwrap())).unwrap();
    let st = Arc::new(AppState::new(
        Index::open(&tmp.path().join("index.sqlite")).unwrap(),
        tmp.path().to_path_buf(),
        tmp.path().join("cache"),
        accounts,
        Tools::default(),
        false,
        false,
        Duration::from_secs(60),
    ));
    let app = router(st);
    let get = |uri: &str| {
        let app = app.clone();
        let uri = uri.to_owned();
        async move {
            let r = app
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = r.status();
            let headers = r.headers().clone();
            let body = r.into_body().collect().await.unwrap().to_bytes();
            (status, headers, body)
        }
    };
    let v = asset_version();
    assert_eq!(v.len(), 12);

    // All reachable without signing in.
    let (s, h, body) = get("/manifest.webmanifest").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], "application/manifest+json");
    let m: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(m["display"], "standalone");
    for icon in m["icons"].as_array().unwrap() {
        let (s, h, body) = get(icon["src"].as_str().unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(h[header::CONTENT_TYPE], "image/png");
        assert_eq!(&body[1..4], b"PNG");
    }
    assert!(
        m["icons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["purpose"] == "maskable")
    );

    let (s, h, body) = get("/sw.js").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CACHE_CONTROL], "no-cache");
    let sw = String::from_utf8(body.to_vec()).unwrap();
    assert!(sw.contains(&format!("const VERSION = \"{v}\"")) && !sw.contains("{{v}}"));

    let (s, _, body) = get("/offline.html").await;
    assert_eq!(s, StatusCode::OK);
    let page = String::from_utf8(body.to_vec()).unwrap();
    assert!(page.contains(&format!("/static/app.css?v={v}")) && page.contains("rel=\"manifest\""));
    assert!(!page.contains("{{"));

    let (s, _, body) = get("/login").await;
    assert_eq!(s, StatusCode::OK);
    let page = String::from_utf8(body.to_vec()).unwrap();
    assert!(
        page.contains("apple-touch-icon")
            && page.contains("viewport-fit=cover")
            && !page.contains("{{head}}")
    );

    let (s, h, _) = get(&format!("/static/app.js?v={v}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        h[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    assert_eq!(get("/apple-touch-icon.png").await.0, StatusCode::OK);
    assert_eq!(
        get("/static/icons/../../etc/passwd").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get("/static/icons/nope.png").await.0, StatusCode::NOT_FOUND);
    // The app itself still requires a session.
    assert_eq!(get("/").await.0, StatusCode::SEE_OTHER);
}
