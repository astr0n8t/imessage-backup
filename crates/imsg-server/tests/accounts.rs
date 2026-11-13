//! Several people's phones in one archive, with accounts scoped by device
//! and by iMessage ID.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode, header};
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use imsg_core::backup::attachment_file_id;
use imsg_core::fixture::{SAMPLE_OWNER, build_sample_backup_for};
use imsg_server::auth::{Accounts, hash_password};
use imsg_server::index::Index;
use imsg_server::media::Tools;
use imsg_server::web::{AppState, router};
use serde_json::Value;
use tower::ServiceExt;

const BOB: &str = "+15550108888";

fn extract(tmp: &std::path::Path, data: &std::path::Path, udid: &str, owner: &str, stage: u8) {
    let backup = tmp.join("backups").join(udid);
    let date =
        Utc.with_ymd_and_hms(2024, 6, 1, 0, 0, 0).unwrap() + chrono::Duration::days(stage as i64);
    build_sample_backup_for(&backup, udid, None, stage, date, owner).unwrap();
    imsg_extract::run(
        &imsg_extract::Options {
            backup,
            out: data.to_path_buf(),
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

struct Client {
    app: Router,
    cookie: String,
}

impl Client {
    async fn login(app: &Router, user: &str) -> Self {
        let r = app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("username={user}&password=pw-{user}")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.headers()[header::LOCATION], "/", "login as {user}");
        let cookie = r.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        Self {
            app: app.clone(),
            cookie,
        }
    }

    async fn get(&self, uri: &str) -> (StatusCode, Value) {
        let r = self
            .app
            .clone()
            .oneshot(
                Request::get(uri)
                    .header(header::COOKIE, &self.cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = r.status();
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Texts of the conversation titled `title`, oldest first.
    async fn texts(&self, title: &str, view: &str) -> Vec<String> {
        let (_, convs) = self.get(&format!("/api/conversations{view}")).await;
        let Some(conv) = convs
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["title"] == title)
        else {
            return vec![];
        };
        let sep = if view.is_empty() { "?" } else { "&" };
        let (_, page) = self
            .get(&format!(
                "/api/conversations/{}/messages{view}{sep}limit=100",
                conv["id"]
            ))
            .await;
        page["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                m["body"]["parts"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|p| p["spans"].as_array())
                    .flat_map(|s| s.iter().map(|x| x["text"].as_str().unwrap().to_owned()))
                    .collect::<String>()
            })
            .collect()
    }
}

#[tokio::test]
async fn accounts_see_only_their_devices_and_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("archive");
    // Alice upgraded phones: both backups hold the same conversation (same GUIDs).
    extract(tmp.path(), &data, "ALICE-OLD", SAMPLE_OWNER, 1);
    extract(tmp.path(), &data, "ALICE-NEW", SAMPLE_OWNER, 2);
    // Bob's phone talks to the same people; iMessage GUIDs overlap with Alice's.
    extract(tmp.path(), &data, "BOB-PHONE", BOB, 1);

    let index = Index::open(&tmp.path().join("state/index.sqlite")).unwrap();
    assert_eq!(index.ingest_all(&data).unwrap().snapshots, 3);
    // Finding other devices' copies must be an index seek, not a scan of the
    // conversation per message (that made /api/status take minutes).
    {
        let conn = index.read().unwrap();
        let view = imsg_server::view::View::everything();
        view.install(&conn).unwrap();
        let plan: Vec<String> = conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN SELECT m.id FROM messages m WHERE m.conv_id = 1 AND {}",
                view.dedupe("m")
            ))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter().any(|l| l.contains("messages_copies")),
            "{plan:#?}"
        );
        assert!(!plan.iter().any(|l| l.contains("TEMP B-TREE")), "{plan:#?}");
        let status = imsg_server::queries::status(&conn, &view, &Default::default()).unwrap();
        assert_eq!(
            status.messages, 8,
            "copies on two of Alice's phones count once"
        );
    }
    let ids = imsg_server::queries::identities(&index.read().unwrap()).unwrap();
    let bob_ids = &ids
        .iter()
        .find(|d| d.device == "BOB-PHONE")
        .unwrap()
        .imessage_ids;
    assert_eq!(bob_ids[0].0, "5550108888");

    let h = |u: &str| hash_password(&format!("pw-{u}")).unwrap();
    let accounts = Accounts::parse(&format!(
        r#"
[[account]]
name = "alice"
password = "{}"
imessage_ids = ["+1 (555) 010-9999"]

[[account]]
name = "bob"
password = "{}"
devices = ["bob-phone"]

[[account]]
name = "carol"
password = "{}"
devices = ["NOT-ARCHIVED"]

[[account]]
name = "admin"
password = "{}"
devices = ["ALICE-OLD"]
admin = true
"#,
        h("alice"),
        h("bob"),
        h("carol"),
        h("admin")
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
    let app = router(state).layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1))));

    // Alice (by iMessage ID): both of her phones, de-duplicated, nothing of Bob's.
    let alice = Client::login(&app, "alice").await;
    let texts = alice.texts("Jane Appleseed", "").await;
    assert_eq!(
        texts,
        [
            "Hey there! Are we still on for Friday?",
            "Yes! See you at 7:30",
            "",
            "Thanks for dinner 🍝",
            "Anytime!"
        ]
    );
    let (_, me) = alice.get("/api/me").await;
    assert_eq!(me["admin"], false);
    assert!(me["accounts"].as_array().unwrap().is_empty());
    let (_, hits) = alice.get("/api/search?q=phone").await;
    assert!(
        hits.as_array().unwrap().is_empty(),
        "Bob's message is not searchable by Alice"
    );
    let (_, hits) = alice.get("/api/search?q=dinner").await;
    assert_eq!(
        hits.as_array().unwrap().len(),
        1,
        "one hit despite two devices holding it"
    );
    let (_, status) = alice.get("/api/status").await;
    let devices: Vec<&str> = status["snapshots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["device"].as_str().unwrap())
        .collect();
    assert!(
        devices.contains(&"ALICE-NEW")
            && devices.contains(&"ALICE-OLD")
            && !devices.contains(&"BOB-PHONE")
    );
    assert_eq!(alice.get("/api/identities").await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        alice.get("/api/conversations?as=bob").await.0,
        StatusCode::FORBIDDEN
    );

    // Bob (by device): his own copy only.
    let bob = Client::login(&app, "bob").await;
    let texts = bob.texts("Jane Appleseed", "").await;
    assert_eq!(
        texts,
        [
            "Hey there! Are we still on for Friday?",
            "Yes! See you at 7",
            "",
            "Only on the 8888 phone"
        ]
    );
    let (_, hits) = bob.get("/api/search?q=dinner").await;
    assert!(hits.as_array().unwrap().is_empty());
    // He can't page around one of Alice's messages either.
    let (_, convs) = alice.get("/api/conversations").await;
    let jane = convs
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["title"] == "Jane Appleseed")
        .unwrap();
    let (_, page) = alice
        .get(&format!("/api/conversations/{}/messages", jane["id"]))
        .await;
    let alice_only = page["messages"].as_array().unwrap().last().unwrap()["id"]
        .as_i64()
        .unwrap();
    let (_, page) = bob
        .get(&format!(
            "/api/conversations/{}/messages?around={alice_only}",
            jane["id"]
        ))
        .await;
    assert!(page["messages"].as_array().unwrap().is_empty());

    // Carol's device has no snapshots: she sees nothing, including attachments.
    let carol = Client::login(&app, "carol").await;
    let (_, convs) = carol.get("/api/conversations").await;
    assert!(convs.as_array().unwrap().is_empty());
    let photo =
        attachment_file_id("~/Library/SMS/Attachments/0a/10/AT-GUID-1/IMG_0001.JPG").unwrap();
    assert_eq!(
        carol.get(&format!("/media/{photo}")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(bob.get(&format!("/media/{photo}")).await.0, StatusCode::OK);

    // Admin: own view (the old phone only), and can look at anyone else's.
    let admin = Client::login(&app, "admin").await;
    assert_eq!(admin.texts("Jane Appleseed", "").await.len(), 3);
    assert_eq!(
        admin
            .texts("Jane Appleseed", "?as=bob")
            .await
            .last()
            .unwrap(),
        "Only on the 8888 phone"
    );
    assert_eq!(
        admin.get(&format!("/media/{photo}?as=carol")).await.0,
        StatusCode::NOT_FOUND
    );
    let (_, me) = admin.get("/api/me").await;
    assert_eq!(me["accounts"].as_array().unwrap().len(), 4);
    let (status, ids) = admin.get("/api/identities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids.as_array().unwrap().len(), 3);
    assert_eq!(
        admin.get("/api/conversations?as=nobody").await.0,
        StatusCode::NOT_FOUND
    );
}
