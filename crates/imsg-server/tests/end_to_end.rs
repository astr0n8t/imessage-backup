//! Extract two synthetic backups, ingest them and exercise the HTTP API.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode, header};
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use imsg_core::backup::attachment_file_id;
use imsg_core::fixture::{build_sample_backup, guid};
use imsg_server::auth::{Accounts, hash_password};
use imsg_server::index::Index;
use imsg_server::media::Tools;
use imsg_server::queries::{self, Cursor, Names};
use imsg_server::view::View;
use imsg_server::web::{AppState, router};
use serde_json::Value;
use tower::ServiceExt;

fn extract(backup: &std::path::Path, out: &std::path::Path, stage: u8) {
    let date =
        Utc.with_ymd_and_hms(2024, 5, 1, 20, 0, 0).unwrap() + chrono::Duration::days(stage as i64);
    let _ = std::fs::remove_dir_all(backup);
    build_sample_backup(backup, "PHONE-1", None, stage, date).unwrap();
    imsg_extract::run(
        &imsg_extract::Options {
            backup: backup.to_path_buf(),
            out: out.to_path_buf(),
            state: None,
            password: None,
            full: false,
            force: false,
            attachments: true,
            now: date,
        },
        &mut |_| {},
    )
    .unwrap();
}

fn body_json(page: &queries::Page) -> Value {
    serde_json::to_value(page).unwrap()
}

#[tokio::test]
async fn ingest_and_serve() {
    let tmp = tempfile::tempdir().unwrap();
    let backup = tmp.path().join("backup");
    let data = tmp.path().join("archive");
    let index = Index::open(&tmp.path().join("state/index.sqlite")).unwrap();

    // ---- First backup ----
    extract(&backup, &data, 1);
    let r = index.ingest_all(&data).unwrap();
    assert_eq!(r.snapshots, 1);
    let conn = index.read().unwrap();
    let view = View::everything();
    view.install(&conn).unwrap();
    let names = Names::load(&conn, &view).unwrap();
    let convs = queries::conversations(&conn, &view, &names).unwrap();
    let titles: Vec<&str> = convs.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(titles, ["Road Trip", "Jane Appleseed"]);
    let jane = convs.iter().find(|c| c.title == "Jane Appleseed").unwrap();
    assert!(!jane.is_group);
    let page = queries::messages(&conn, &view, &names, jane.id, Cursor::Latest, 50).unwrap();
    let v = body_json(&page);
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(
        msgs.len(),
        3,
        "tapback is folded into its target: {msgs:#?}"
    );
    assert_eq!(
        msgs[0]["body"]["parts"][0]["spans"][0]["text"],
        "Hey there! Are we still on for Friday?"
    );
    assert_eq!(msgs[0]["sender_name"], "Jane Appleseed");
    assert_eq!(msgs[1]["from_me"], true);
    assert_eq!(msgs[1]["reactions"][0]["emoji"], "❤️");
    let photo =
        attachment_file_id("~/Library/SMS/Attachments/0a/10/AT-GUID-1/IMG_0001.JPG").unwrap();
    assert_eq!(msgs[2]["body"]["parts"][0]["t"], "attachment");
    assert_eq!(msgs[2]["body"]["parts"][0]["file"], photo.as_str());
    // Bob is not in the first contacts DB.
    let trip = convs.iter().find(|c| c.title == "Road Trip").unwrap();
    assert!(
        trip.participants
            .iter()
            .any(|p| p.name == "+1 (555) 010-0001")
    );
    drop(conn);

    // Re-ingesting is a no-op.
    assert_eq!(index.ingest_all(&data).unwrap().snapshots, 0);

    // ---- Second backup (incremental) ----
    extract(&backup, &data, 2);
    let r = index.ingest_all(&data).unwrap();
    assert_eq!(r.snapshots, 1);
    assert_eq!(r.messages, 3);
    let conn = index.read().unwrap();
    let view = View::everything();
    view.install(&conn).unwrap();
    let names = Names::load(&conn, &view).unwrap();
    let page = queries::messages(&conn, &view, &names, jane.id, Cursor::Latest, 50).unwrap();
    let v = body_json(&page);
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5);
    let edited = msgs
        .iter()
        .find(|m| m["guid"] == guid("M2").as_str())
        .unwrap();
    assert_eq!(
        edited["body"]["parts"][0]["spans"][0]["text"],
        "Yes! See you at 7:30"
    );
    assert!(edited["edited_ms"].is_i64());
    assert_eq!(
        edited["reactions"][0]["emoji"], "❤️",
        "reaction from the first snapshot survives"
    );
    let reply = msgs
        .iter()
        .find(|m| m["guid"] == guid("M7").as_str())
        .unwrap();
    assert_eq!(reply["reply_to"]["name"], "Jane Appleseed");
    assert_eq!(msgs[0]["reply_count"], 1);
    let convs = queries::conversations(&conn, &view, &names).unwrap();
    assert_eq!(
        convs[0].title, "Jane Appleseed",
        "most recent conversation first"
    );
    assert_eq!(convs[0].last_preview.as_deref(), Some("Anytime!"));
    let trip = convs.iter().find(|c| c.title == "Road Trip").unwrap();
    assert!(
        trip.participants.iter().any(|p| p.name == "Bob Builder"),
        "contacts refreshed"
    );

    let hits = queries::search(&conn, &view, &names, "dinn", None, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.contains("\u{2}dinner\u{3}"));
    assert!(
        queries::search(
            &conn,
            &view,
            &names,
            "\"); DROP TABLE messages; --",
            None,
            10
        )
        .unwrap()
        .is_empty()
    );

    // Paging.
    let first = queries::messages(&conn, &view, &names, jane.id, Cursor::Latest, 2).unwrap();
    assert!(first.has_more_before);
    let older = queries::messages(
        &conn,
        &view,
        &names,
        jane.id,
        Cursor::Before(first.messages[0].id),
        10,
    )
    .unwrap();
    assert_eq!(older.messages.len(), 3);
    assert!(!older.has_more_before);
    drop(conn);

    // ---- HTTP ----
    let accounts = Accounts::parse(&format!(
        "alice:{}",
        hash_password("correct-horse").unwrap()
    ))
    .unwrap();
    let state = Arc::new(AppState::new(
        index,
        data.clone(),
        tmp.path().join("state/cache"),
        accounts,
        Tools::default(),
        false,
        false,
        Duration::from_secs(3600),
    ));
    let app = router(state).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4000))));
    let send = |req: Request<Body>| {
        let app = app.clone();
        async move { app.oneshot(req).await.unwrap() }
    };

    let r = send(
        Request::get("/api/conversations")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = send(Request::get("/").body(Body::empty()).unwrap()).await;
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    assert!(r.headers().get(header::CONTENT_SECURITY_POLICY).is_some());

    let login = |pw: &str| {
        Request::post("/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("username=alice&password={pw}")))
            .unwrap()
    };
    let r = send(login("wrong")).await;
    assert_eq!(r.headers()[header::LOCATION], "/login?error=1");
    assert!(r.headers().get(header::SET_COOKIE).is_none());
    let r = send(login("correct-horse")).await;
    assert_eq!(r.headers()[header::LOCATION], "/");
    let cookie = r.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    assert!(
        r.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .contains("HttpOnly")
    );

    let authed = |uri: &str| {
        Request::get(uri)
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap()
    };
    let r = send(authed("/api/conversations")).await;
    assert_eq!(r.status(), StatusCode::OK);
    let list: Value =
        serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
    let id = list[0]["id"].as_i64().unwrap();
    let r = send(authed(&format!("/api/conversations/{id}/messages?limit=2"))).await;
    assert_eq!(r.status(), StatusCode::OK);
    let r = send(authed("/api/search?q=driving")).await;
    let hits: Value =
        serde_json::from_slice(&r.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(hits[0]["conv_title"], "Road Trip");
    let r = send(authed("/api/status")).await;
    assert_eq!(r.status(), StatusCode::OK);

    let r = send(authed(&format!("/media/{photo}"))).await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.headers()[header::CONTENT_TYPE], "image/jpeg");
    assert!(
        r.headers()[header::CONTENT_DISPOSITION]
            .to_str()
            .unwrap()
            .starts_with("inline")
    );
    assert_eq!(
        r.into_body().collect().await.unwrap().to_bytes().as_ref(),
        [0xFF, 0xD8, 0xFF, 0xD9]
    );
    let r = send(
        Request::get(format!("/media/{photo}"))
            .header(header::COOKIE, &cookie)
            .header(header::RANGE, "bytes=1-2")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        r.into_body().collect().await.unwrap().to_bytes().as_ref(),
        [0xD8, 0xFF]
    );
    // Offloaded attachment: known to the index but not in the archive.
    let missing = attachment_file_id("~/Library/SMS/Attachments/0b/11/AT-GUID-2/map.png").unwrap();
    assert_eq!(
        send(authed(&format!("/media/{missing}"))).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(authed("/media/..%2F..%2Fetc%2Fpasswd")).await.status(),
        StatusCode::BAD_REQUEST
    );
    // Without a session, media is refused.
    let r = send(
        Request::get(format!("/media/{photo}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    let r = send(
        Request::post("/logout")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(r.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        send(authed("/api/conversations")).await.status(),
        StatusCode::UNAUTHORIZED
    );
}
