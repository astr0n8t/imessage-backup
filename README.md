# imessage-backup

Archive your iPhone's texts and iMessages from local Finder backups, incrementally, and browse every message
from every backup in a password-protected web app.

```
 iPhone ──Finder backup──▶ ~/Library/Application Support/MobileSync/Backup/<UDID>
                                     │
                         imsg-extract │  (only what's new since last run, tracked in a JSON state file)
                                     ▼
                     archive/<UDID>/snapshots/<id>/{sms.db, AddressBook.sqlitedb, snapshot.json}
                     archive/<UDID>/attachments/<xx>/<fileID>
                                     │   (copy / rsync this folder anywhere)
                          imsg-server │  parses each snapshot with imessage-database
                                     ▼  and merges them into one searchable index
                         https://messages.example.com  (login required)
```

There are two binaries:

| Binary | Runs on | Does |
|---|---|---|
| `imsg-extract` | the Mac that makes the Finder backups | Pulls `sms.db`, `AddressBook.sqlitedb` and message attachments out of a backup (decrypting encrypted backups). Each run writes a **snapshot** with only the messages added or edited since the previous run. |
| `imsg-server` | anywhere (Mac, Linux box, NAS, Docker) | Watches the archive folder, parses snapshots with [`imessage-database`](https://crates.io/crates/imessage-database) (the library behind [imessage-exporter](https://github.com/ReagentX/imessage-exporter)), merges them, and serves a web UI with full-text search. |

## Build

Requires Rust 1.88 or newer.

```sh
cargo build --release
# binaries: target/release/imsg-extract, target/release/imsg-server
```

## 1. Make a backup

Connect the iPhone, open it in Finder and click **Back Up Now**. Both unencrypted and encrypted backups work.
Encrypted backups need their password (see below).

Your terminal needs **Full Disk Access** (System Settings → Privacy & Security) to read
`~/Library/Application Support/MobileSync/Backup`.

## 2. Extract

```sh
imsg-extract list                         # show backups on this Mac
imsg-extract extract --out ~/MessagesArchive
```

By default `extract` uses the most recent backup. Use `--udid <UDID>` or `--backup <path>` to pick one.
For an encrypted backup, set `IMSG_BACKUP_PASSWORD` or enter the password when prompted.

Each run:

1. Copies `sms.db` (plus any `-wal` journal) out of the backup and checks it against the state file.
2. Exports messages with `ROWID` greater than the last run's, plus any message edited or unsent since then.
3. Writes a trimmed `sms.db` with the original iOS schema: just those messages, plus every chat and handle row.
4. Copies the attachments those messages reference into `attachments/`, de-duplicated by backup file ID.
5. Copies the Contacts database if it changed.
6. Writes `snapshot.json` last, with SHA-256 checksums, then updates the state file.

If the backup hasn't changed since the last run, nothing is written (`--force` overrides this).

### The state file

`--state <file>` (default `<out>/imsg-state.json`) records, per device:

* the highest exported message `ROWID` and edit timestamp;
* a few `(ROWID, guid)` anchors, used to detect a restored or replaced phone, which renumbers messages.
  When that happens the extractor does a full export, and the server de-duplicates by GUID;
* attachments that weren't in the backup yet (for example, offloaded to iCloud), retried on every run;
* a hash of the last exported Contacts DB, and a list of snapshots written.

**Staging mode:** if `--out` is a temporary folder that you later copy to the server and then empty, keep the state
file somewhere permanent:

```sh
imsg-extract extract --out ~/imsg-staging --state ~/.config/imsg/state.json
rsync -a ~/imsg-staging/ server:/srv/imsg/archive/ && rm -rf ~/imsg-staging/*
```

Snapshots only ever add files, so merging folders with a plain copy is safe. The server ignores a snapshot until
all of its files are present and match their checksums.

Other flags: `--full` (ignore the state file and export everything), `--no-attachments`, `--json` (print the
snapshot metadata).

### Archive layout

```
archive/
  imsg-state.json                       (default state file location)
  <UDID>/
    device.json
    attachments/<xx>/<fileID>           SHA-1("MediaDomain-" + path): the backup's own naming
    snapshots/<YYYYMMDDTHHMMSSZ>/
      snapshot.json                     kind (full|incremental), ROWID range, counts, file checksums
      sms.db                            standard iOS Messages schema, delta contents
      AddressBook.sqlitedb              only when contacts changed
```

Every `sms.db` is a normal Messages database, so `sqlite3` or any imessage-database based tool can open it.

## 3. Serve

Create a password file (one `username:argon2-hash` line per user):

```sh
imsg-server hash-password --user me >> users.txt
```

Run the server:

```sh
imsg-server serve --data ~/MessagesArchive --auth-file users.txt --listen 127.0.0.1:8080
```

* The index, sessions and transcode cache go in `<data>/.imsg-server/`. Use `--state-dir` if the data folder is
  read-only.
* New snapshots are picked up every 5 minutes (`--scan-interval`), or immediately via **ⓘ → Scan for new
  snapshots** in the UI.
* To reach it from outside your network, put a TLS reverse proxy (Caddy, nginx, Tailscale Serve, …) in front of it
  and add `--secure-cookies`. Add `--trust-forwarded-for` only if the proxy sets `X-Forwarded-For`.
* Every option also has an environment variable (`IMSG_DATA`, `IMSG_AUTH_FILE`, `IMSG_LISTEN`, …); see
  `imsg-server serve --help`.

### Docker

```sh
docker build -t imsg-server .
docker run -d -p 8080:8080 \
  -v /srv/imsg/archive:/data:ro -v /srv/imsg/state:/state -v /srv/imsg/users.txt:/users.txt:ro \
  imsg-server
```

The image includes `heif-convert` and `ffmpeg`, so HEIC photos and CAF/AMR voice messages are converted to
JPEG and M4A for browsers that can't play them natively. Outside Docker, install `libheif-examples` (or
ImageMagick; `sips` is used automatically on macOS) and `ffmpeg` for the same behaviour.

### What the viewer shows

* Every conversation from every snapshot and device. A contact's iMessage and SMS threads are merged.
* Names from your Contacts (the newest Contacts DB wins).
* Text formatting, links and mentions; photos, videos, audio (with transcriptions) and other files.
* Tapbacks and emoji reactions, threaded replies, edit history, unsent messages, message effects,
  link previews, and group events (renames, people added or removed).
* Full-text search across everything, which jumps to the message in context.
* Light and dark mode, and a mobile layout.

Messages deleted from the phone stay in the archive, because older snapshots are never rewritten.

## Automating the Mac side

Finder can't be scripted to make backups. Once a backup exists, extraction is a single command, which you can
run from `launchd` or cron, e.g.:

```sh
#!/bin/sh
# ~/bin/imsg-sync.sh
export IMSG_BACKUP_PASSWORD="$(security find-generic-password -s imsg-backup -w)"
imsg-extract extract --out ~/imsg-staging --state ~/.config/imsg/state.json &&
  rsync -a ~/imsg-staging/ server:/srv/imsg/archive/ && rm -rf ~/imsg-staging/*
```

## Security notes

* Passwords are stored as argon2id hashes. Sessions are random 256-bit tokens; only their SHA-256 is stored.
  Cookies are `HttpOnly` and `SameSite=Lax` (add `--secure-cookies` behind HTTPS).
* After 10 failed logins from one IP address, logins from it are blocked for 15 minutes.
* The UI runs under a strict Content-Security-Policy and inserts all message content as text, never as HTML.
  Attachments are served with `nosniff` and a sandbox CSP; anything other than images, audio and video is
  always downloaded, never rendered.
* The archive holds your full message history in plain form. Keep it on encrypted storage.

## Development

```sh
cargo test --workspace
# A synthetic backup to try the whole flow without a phone:
cargo run -p imsg-core --example sample_backup -- /tmp/demo-backup/00008110-SAMPLE 1
cargo run -p imsg-extract -- extract --backup /tmp/demo-backup/00008110-SAMPLE --out /tmp/demo-archive
```

Crates: `imsg-core` (backup reading and decryption, archive layout, state file, contacts),
`imsg-extract` (CLI), `imsg-server` (indexer and web app).

### Limitations

* Only iOS 10+ backups (`Manifest.db`) are supported.
* The test suite uses synthetic backups that follow the documented formats (including the encrypted-backup
  keybag scheme). It has not yet been run against a real device backup.
* Formats that imessage-database can parse but this UI doesn't specifically render (polls, Digital Touch,
  handwriting, Apple Cash amounts, …) appear as a labelled card, plus any text.
