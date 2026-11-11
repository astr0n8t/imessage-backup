use chrono::{Duration, TimeZone, Utc};
use imsg_core::archive::{self, SnapshotKind};
use imsg_core::backup::attachment_file_id;
use imsg_core::fixture::{build_sample_backup, guid};
use imsg_core::state::State;
use imsg_extract::{Options, run};
use rusqlite::Connection;

fn guids(db: &std::path::Path) -> Vec<String> {
    let c = Connection::open(db).unwrap();
    let mut s = c
        .prepare("SELECT guid FROM message ORDER BY ROWID")
        .unwrap();
    s.query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn scenario(password: Option<&str>) {
    let tmp = tempfile::tempdir().unwrap();
    let backup = tmp.path().join("backup/00008110-AAAA");
    let out = tmp.path().join("archive");
    let state_path = tmp.path().join("state.json");
    let d1 = Utc.with_ymd_and_hms(2024, 5, 1, 20, 0, 0).unwrap();
    build_sample_backup(&backup, "00008110-AAAA", password, 1, d1).unwrap();

    let opts = |now| Options {
        backup: backup.clone(),
        out: out.clone(),
        state: Some(state_path.clone()),
        password: password.map(str::to_owned),
        full: false,
        force: false,
        attachments: true,
        now,
    };
    let mut log = |l: String| eprintln!("{l}");

    // First run: full snapshot.
    let r1 = run(&opts(d1), &mut log).unwrap();
    let s1 = r1.snapshot.unwrap();
    assert_eq!(s1.kind, SnapshotKind::Full);
    assert_eq!(s1.message_count, 6);
    assert!(s1.has_file(archive::CONTACTS_FILE));
    let dir1 = r1.snapshot_dir.unwrap();
    assert_eq!(
        guids(&dir1.join("sms.db")),
        ["M1", "M2", "M3", "M4", "M5", "M5B"].map(guid)
    );
    assert!(s1.verify(&dir1, true).unwrap());
    let dev_dir = archive::device_dir(&out, "00008110-AAAA");
    let photo = archive::attachment_path(
        &dev_dir,
        &attachment_file_id("~/Library/SMS/Attachments/0a/10/AT-GUID-1/IMG_0001.JPG").unwrap(),
    );
    assert_eq!(std::fs::read(photo).unwrap(), [0xFF, 0xD8, 0xFF, 0xD9]);
    assert_eq!(r1.attachments_pending, 1);

    // Same backup again: still retries the pending attachment, but no new snapshot.
    let r1b = run(&opts(d1 + Duration::minutes(5)), &mut log).unwrap();
    assert!(r1b.snapshot.is_none());

    // Second backup: only new + edited messages, contacts changed.
    let d2 = d1 + Duration::days(2);
    std::fs::remove_dir_all(&backup).unwrap();
    build_sample_backup(&backup, "00008110-AAAA", password, 2, d2).unwrap();
    let r2 = run(&opts(d2), &mut log).unwrap();
    let s2 = r2.snapshot.unwrap();
    assert_eq!(s2.kind, SnapshotKind::Incremental);
    assert_eq!(s2.after_rowid, 6);
    let dir2 = r2.snapshot_dir.unwrap();
    assert_eq!(guids(&dir2.join("sms.db")), ["M2", "M6", "M7"].map(guid));
    assert!(r2.contacts_updated);
    // Chats and handles are always complete.
    let c = Connection::open(dir2.join("sms.db")).unwrap();
    let chats: i64 = c
        .query_row("SELECT count(*) FROM chat", [], |r| r.get(0))
        .unwrap();
    assert_eq!(chats, 2);
    let atts: i64 = c
        .query_row("SELECT count(*) FROM attachment", [], |r| r.get(0))
        .unwrap();
    assert_eq!(atts, 0);

    // Unchanged backup is skipped.
    let r3 = run(&opts(d2 + Duration::hours(1)), &mut log).unwrap();
    assert!(r3.snapshot.is_none());

    let state = State::load(&state_path).unwrap();
    let dev = &state.devices["00008110-AAAA"];
    assert_eq!(dev.last_rowid, 8);
    assert_eq!(dev.snapshots.len(), 2);
    assert_eq!(dev.pending_attachments.len(), 1);
    assert_eq!(archive::discover_snapshots(&out).unwrap().len(), 2);
}

#[test]
fn incremental_unencrypted() {
    scenario(None);
}

#[test]
fn incremental_encrypted() {
    scenario(Some("s3cret"));
}

#[test]
fn restored_phone_falls_back_to_full() {
    let tmp = tempfile::tempdir().unwrap();
    let backup = tmp.path().join("b");
    let out = tmp.path().join("archive");
    let d1 = Utc.with_ymd_and_hms(2024, 5, 1, 20, 0, 0).unwrap();
    build_sample_backup(&backup, "DEV", None, 2, d1).unwrap();
    let mk = |now| Options {
        backup: backup.clone(),
        out: out.clone(),
        state: None,
        password: None,
        full: false,
        force: false,
        attachments: false,
        now,
    };
    run(&mk(d1), &mut |_| {}).unwrap();
    // Corrupt the anchors as if the database had been rebuilt.
    let sp = imsg_extract::default_state_path(&out);
    let mut st = State::load(&sp).unwrap();
    for a in &mut st.devices.get_mut("DEV").unwrap().anchors {
        a.guid = "something-else".into();
    }
    st.save(&sp).unwrap();
    let mut o = mk(d1 + Duration::days(1));
    o.force = true;
    let r = run(&o, &mut |_| {}).unwrap();
    assert!(r.fell_back_to_full);
    assert_eq!(r.snapshot.unwrap().kind, SnapshotKind::Full);
}
