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

## Install

**Prebuilt binaries:** every [release](../../releases) has archives with `imsg-extract` and `imsg-server` for
macOS (universal, Apple silicon, Intel) and Linux (static x86_64 and arm64 builds that run on any distribution),
plus a `SHA256SUMS` file. The macOS binaries aren't notarized. If Gatekeeper blocks them, run
`xattr -d com.apple.quarantine imsg-extract imsg-server`.

**Docker image** (server only, amd64 and arm64): `ghcr.io/astr0n8t/imessage-backup`. See [Docker](#docker).

**From source** (Rust 1.88 or newer):

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
For an encrypted backup, set `IMSG_BACKUP_PASSWORD` or enter the password when prompted. (When setting the
variable in a shell, use single quotes so characters like `$` or `!` in the password aren't expanded.) If a password
you know is correct is rejected, run `imsg-extract keybag` and include its output in a bug report. It prints the
structure of the backup's key bag without any password, salts or keys.

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

### Accounts

One archive can hold several people's phones. Each login account is tied to the **devices** and/or
**iMessage IDs** whose messages it may see. Accounts live in a TOML file:

```toml
[[account]]
name = "alice"
password = "$argon2id$v=19$..."            # from `imsg-server hash-password`
devices = ["00008110-000A1B2C3D4E5F6A"]    # backups of these devices (UDIDs); "*" = every device
imessage_ids = ["+15550109999", "alice@icloud.com"]   # her numbers / Apple IDs, on any device
admin = true                               # admins can also switch to other accounts' views

[[account]]
name = "bob"
password = "$argon2id$v=19$..."
devices = ["00008030-001122334455667E"]
```

* **`devices`** grants everything in that device's backups. Use it for "this person's phone(s)".
* **`imessage_ids`** grants every message sent or received *as* that phone number or Apple ID, from any
  archived device (matched on the message's `destination_caller_id`, with phone numbers normalized). A new
  phone with the same number is covered without editing the file. Older iOS versions sometimes leave this
  field empty, especially for SMS, so `devices` is the more complete grant.
* An account sees a message if either rule matches. When several visible devices hold the same message (an old
  and a new phone), it's shown once.
* Each device's data is kept separately, so archiving two people who text each other never mixes their copies:
  the same iMessage has the same GUID on the sender's and recipient's phones. Conversation names, participants and
  contact names also come only from devices the account can see.
* An admin gets a **Viewing** picker in the sidebar to look at any account's view. Granting one account several
  people's devices (`"*"`) works, but those people's threads with the same contact then appear as one
  conversation, so prefer one account per person.

To find device UDIDs and the iMessage IDs each device used:

```sh
imsg-server identities --data ~/MessagesArchive     # also in the UI: ⓘ (admins)
```

To create an entry, run `hash-password`. It prompts for the password and prints an `[[account]]` block to fill in:

```sh
imsg-server hash-password --user alice >> accounts.toml
```

### Two-factor authentication (TOTP)

Any account can also require a 6-digit code from an authenticator app (1Password, Google Authenticator, Authy,
Bitwarden, …):

```sh
imsg-server totp-secret --user alice
```

This shows a QR code to scan (use `--invert` on a light-background terminal; the key is also printed for
manual entry). It then asks for a code from the app to confirm the setup and prints a line to add to that
account's entry:

```toml
totp_secret = "QJ3ROH2XANMMT4OTTO2FJBTNJ7NCGA2M"
```

After a restart, that account signs in with password and then code. Details:

* Codes are standard RFC 6238 (SHA-1, 30 s, 6 digits), and one step of clock drift either way is accepted. Keep
  the server's clock in sync (NTP).
* Each code works only once. After the password is accepted you have 5 minutes and 5 tries to enter a code, after which the password must be
  entered again. Wrong codes count towards the per-IP lockout, and a correct password doesn't reset that count.
* `--require-totp` (or `IMSG_REQUIRE_TOTP=1`) makes the server refuse to start while any account has no
  `totp_secret`. Without it, accounts lacking a secret are listed in the startup log.
* To reset someone's second factor, generate a new secret and replace the line. To turn it off, delete the line.
* The secret is a shared key, so treat the accounts file like a password file (e.g. `chmod 600`).

The older one-line `username:hash` file format is still accepted. Those users are admins who see everything.
Restart the server after editing the accounts file. Removing an account ends its sessions immediately.

### Run

```sh
imsg-server serve --data ~/MessagesArchive --accounts accounts.toml --listen 127.0.0.1:8080
```

* The index, sessions and transcode cache go in `<data>/.imsg-server/`. Use `--state-dir` if the data folder is
  read-only.
* New snapshots are picked up every 5 minutes (`--scan-interval`), or immediately via **ⓘ → Scan for new
  snapshots** in the UI.
* To reach it from outside your network, put a TLS reverse proxy (Caddy, nginx, Tailscale Serve, …) in front of it
  and add `--secure-cookies`. Add `--trust-forwarded-for` only if the proxy sets `X-Forwarded-For`.
* Every option also has an environment variable (`IMSG_DATA`, `IMSG_AUTH_FILE` for the accounts file,
  `IMSG_LISTEN`, …); see
  `imsg-server serve --help`.

### Docker

```sh
docker run -d --name imsg -p 8080:8080 \
  -v /srv/imsg/archive:/data:ro -v /srv/imsg/state:/state -v /srv/imsg/accounts.toml:/accounts.toml:ro \
  ghcr.io/astr0n8t/imessage-backup:latest
```

Images are tagged with each release's version (`1.2.3`, `1.2`) and `latest`. To build the image yourself from
source instead, run `docker build -t imsg-server .`. The image runs as an unprivileged user. Keep the state volume
(index, sessions, transcode cache) on persistent storage.

The image includes `heif-convert` and `ffmpeg`, so HEIC photos and CAF/AMR voice messages are converted to
JPEG and M4A for browsers that can't play them natively. Outside Docker, install `libheif-examples` (or
ImageMagick; `sips` is used automatically on macOS) and `ffmpeg` for the same behaviour.

### What the viewer shows

* Every conversation from every snapshot of the devices / iMessage IDs your account is granted. A contact's
  iMessage and SMS threads are merged.
* Names from the Contacts of those devices (the most recently backed-up Contacts DB wins).
* Text formatting, links and mentions; photos, videos, audio (with transcriptions) and other files.
* Tapbacks and emoji reactions, threaded replies, edit history, unsent messages, message effects,
  link previews, and group events (renames, people added or removed).
* Full-text search across everything, which jumps to the message in context.
* Light and dark mode, and a mobile layout.

Messages deleted from the phone stay in the archive, because older snapshots are never rewritten.

### On your phone: install it as an app

The web UI is built for phones as well as desktops. It works one-handed, has large touch targets, respects the
notch and home indicator, and opens photos full-screen (the back gesture closes them). Tap any message to see
when it was sent, delivered and read, its edit history, and who reacted.

It is also a Progressive Web App, so you can install it to the home screen and open it like a native app,
without browser chrome:

* **iPhone / iPad:** open the site in Safari → Share → **Add to Home Screen**.
* **Android / Chrome / Edge:** use the **Install** button in the sidebar, or the browser menu's *Install app*.

Notes:

* Installing needs HTTPS (or `localhost`). Behind a TLS reverse proxy it just works. Use `--secure-cookies` there.
* On iOS, the installed app has its own cookies, so you sign in once more inside it (including the two-factor
  code). Sessions last `--session-days` (30 by default).
* The service worker caches only the app's code, styles and icons, for fast start-up and an "offline" screen.
  **Messages and attachments are never stored on the device by the app.** They always come from the server, and
  API responses are marked `no-store`.
* Updates roll out automatically: asset URLs carry a content hash, and a new server build replaces the cached
  shell on the next launch.



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

* Passwords are stored as argon2id hashes. Optional TOTP two-factor authentication is set per account. Sessions are random 256-bit tokens; only their SHA-256 is stored.
  Cookies are `HttpOnly` and `SameSite=Lax` (add `--secure-cookies` behind HTTPS).
* After 10 failed logins from one IP address, logins from it are blocked for 15 minutes.
* Account scoping is enforced on the server for every API call and attachment download. Attachments are served
  only when a message the account can see references them. Non-admins can't use `?as=`.
* The UI runs under a strict Content-Security-Policy and inserts all message content as text, never as HTML.
  Attachments are served with `nosniff` and a sandbox CSP; anything other than images, audio and video is
  always downloaded, never rendered.
* The archive holds your full message history in plain form. Keep it on encrypted storage.

## Development

```sh
cargo test --workspace
# A synthetic backup to try the whole flow without a phone:
cargo run -p imsg-core --example sample_backup -- /tmp/demo-backup/00008110-SAMPLE 1
# (optional args: password or "-", and the phone's own number, to simulate several people's phones)
cargo run -p imsg-extract -- extract --backup /tmp/demo-backup/00008110-SAMPLE --out /tmp/demo-archive
```

Crates: `imsg-core` (backup reading and decryption, archive layout, state file, contacts),
`imsg-extract` (CLI), `imsg-server` (indexer and web app).

### Making a release

1. Bump `version` in the workspace `Cargo.toml` (binaries report it with `--version`), and commit.
2. Tag and push: `git tag v1.2.3 && git push origin v1.2.3`. Tags with a suffix such as `v1.2.3-rc.1` become
   pre-releases and don't move the `latest` image tag.

[`.github/workflows/release.yml`](.github/workflows/release.yml) then:

* runs the test suite on Linux and macOS;
* builds static Linux binaries (x86_64, arm64) with `cross`, and macOS binaries (Apple silicon, Intel, plus a
  universal `lipo` build), then smoke-tests and packages them with checksums;
* builds a multi-arch image from those binaries (no compilation under emulation) and pushes it to GitHub
  Container Registry;
* publishes a GitHub Release with the archives, `SHA256SUMS` and generated notes.

The first time the image is pushed, GitHub creates the `imessage-backup` package. If the repository is private, the
package is private too. Change its visibility under the package's settings if you want to pull it without logging
in.

### Limitations

* Only iOS 10+ backups (`Manifest.db`) are supported.
* The test suite uses synthetic backups that follow the documented formats (including the encrypted-backup
  keybag scheme). It has not yet been run against a real device backup.
* Formats that imessage-database can parse but this UI doesn't specifically render (polls, Digital Touch,
  handwriting, Apple Cash amounts, …) appear as a labelled card, plus any text.
