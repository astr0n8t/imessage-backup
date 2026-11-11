//! Reading Finder / iTunes style iOS backups (`MobileSync/Backup/<UDID>`).

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use plist::Value;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::crypto::{self, Keybag};

/// Domain/relative path of the Messages database inside a backup.
pub const SMS_DB: (&str, &str) = ("HomeDomain", "Library/SMS/sms.db");
/// Domain/relative path of the Contacts database inside a backup.
pub const ADDRESS_BOOK_DB: (&str, &str) = ("HomeDomain", "Library/AddressBook/AddressBook.sqlitedb");

/// The default backup location used by Finder on macOS.
pub fn default_backup_root() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library/Application Support/MobileSync/Backup")
}

/// `fileID` of a backup entry: SHA-1 of `"<domain>-<relativePath>"`.
pub fn file_id(domain: &str, relative_path: &str) -> String {
    hex::encode(Sha1::digest(format!("{domain}-{relative_path}").as_bytes()))
}

/// Map an attachment `filename` from `sms.db` to its backup `fileID`.
///
/// iOS stores attachment paths as `~/Library/SMS/Attachments/..` (or the
/// absolute `/var/mobile/Library/...`); in the backup these live in
/// `MediaDomain` with the `~/` prefix removed. This matches the hashing used
/// by imessage-database for iOS backups.
pub fn attachment_file_id(filename: &str) -> Option<String> {
    let rel = filename
        .strip_prefix("~/")
        .or_else(|| filename.strip_prefix("/var/mobile/"))
        .or_else(|| filename.strip_prefix("/private/var/mobile/"))?;
    Some(file_id("MediaDomain", rel))
}

/// Path at which a `fileID` is stored relative to some root (`ab/abcdef...`).
pub fn hashed_path(root: &Path, file_id: &str) -> PathBuf {
    root.join(&file_id[..2]).join(file_id)
}

/// Basic device/backup metadata from `Info.plist` and `Manifest.plist`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BackupInfo {
    pub udid: String,
    pub device_name: Option<String>,
    pub product_type: Option<String>,
    pub product_version: Option<String>,
    pub phone_number: Option<String>,
    pub last_backup_date: Option<DateTime<Utc>>,
    pub encrypted: bool,
}

fn plist_str(dict: &plist::Dictionary, key: &str) -> Option<String> {
    dict.get(key).and_then(Value::as_string).map(str::to_owned)
}

fn plist_date(dict: &plist::Dictionary, key: &str) -> Option<DateTime<Utc>> {
    let date = dict.get(key)?.as_date()?;
    let st: std::time::SystemTime = date.into();
    Some(DateTime::<Utc>::from(st))
}

fn read_dict(path: &Path) -> Result<plist::Dictionary> {
    let value = Value::from_file(path).with_context(|| format!("reading {}", path.display()))?;
    value
        .into_dictionary()
        .ok_or_else(|| anyhow!("{} is not a dictionary plist", path.display()))
}

impl BackupInfo {
    /// Read metadata without unlocking anything.
    pub fn read(root: &Path) -> Result<Self> {
        let manifest = read_dict(&root.join("Manifest.plist"))?;
        let encrypted = manifest
            .get("IsEncrypted")
            .and_then(Value::as_boolean)
            .unwrap_or(false);
        let info = read_dict(&root.join("Info.plist")).unwrap_or_default();
        let lockdown = manifest.get("Lockdown").and_then(Value::as_dictionary);

        let udid = plist_str(&info, "Unique Identifier")
            .or_else(|| plist_str(&info, "Target Identifier"))
            .or_else(|| lockdown.and_then(|l| plist_str(l, "UniqueDeviceID")))
            .or_else(|| root.file_name().map(|n| n.to_string_lossy().into_owned()))
            .ok_or_else(|| anyhow!("cannot determine device UDID"))?;

        let last_backup_date = plist_date(&info, "Last Backup Date")
            .or_else(|| {
                read_dict(&root.join("Status.plist"))
                    .ok()
                    .and_then(|s| plist_date(&s, "Date"))
            })
            .or_else(|| plist_date(&manifest, "Date"));

        Ok(Self {
            udid: udid.to_uppercase(),
            device_name: plist_str(&info, "Device Name")
                .or_else(|| lockdown.and_then(|l| plist_str(l, "DeviceName"))),
            product_type: plist_str(&info, "Product Type")
                .or_else(|| lockdown.and_then(|l| plist_str(l, "ProductType"))),
            product_version: plist_str(&info, "Product Version")
                .or_else(|| lockdown.and_then(|l| plist_str(l, "ProductVersion"))),
            phone_number: plist_str(&info, "Phone Number"),
            last_backup_date,
            encrypted,
        })
    }
}

/// Enumerate backups below a backup root, newest first.
pub fn list_backups(root: &Path) -> Result<Vec<(PathBuf, BackupInfo)>> {
    let mut out = Vec::new();
    let entries = fs::read_dir(root).with_context(|| {
        format!(
            "reading {} (on macOS your terminal needs Full Disk Access)",
            root.display()
        )
    })?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.join("Manifest.plist").is_file()
            && let Ok(info) = BackupInfo::read(&path)
        {
            out.push((path, info));
        }
    }
    out.sort_by(|a, b| b.1.last_backup_date.cmp(&a.1.last_backup_date));
    Ok(out)
}

/// One row from `Manifest.db`'s `Files` table.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub file_id: String,
    pub domain: String,
    pub relative_path: String,
    pub flags: i64,
    file_blob: Option<Vec<u8>>,
}

/// Encryption metadata from the NSKeyedArchiver `MBFile` blob.
struct FileCrypto {
    protection_class: u32,
    wrapped_key: Vec<u8>,
    size: Option<u64>,
}

fn parse_mbfile(blob: &[u8]) -> Result<FileCrypto> {
    let value = Value::from_reader(std::io::Cursor::new(blob))?;
    let dict = value.as_dictionary().ok_or_else(|| anyhow!("MBFile is not a dictionary"))?;
    let objects = dict
        .get("$objects")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("MBFile has no $objects"))?;
    let deref = |v: &Value| -> Option<Value> {
        match v {
            Value::Uid(uid) => objects.get(uid.get() as usize).cloned(),
            other => Some(other.clone()),
        }
    };
    let root_idx = dict
        .get("$top")
        .and_then(Value::as_dictionary)
        .and_then(|t| t.get("root"))
        .and_then(|r| match r {
            Value::Uid(u) => Some(u.get() as usize),
            _ => None,
        })
        .unwrap_or(1);
    let root = objects
        .get(root_idx)
        .and_then(Value::as_dictionary)
        .ok_or_else(|| anyhow!("MBFile root missing"))?;
    let protection_class = root
        .get("ProtectionClass")
        .and_then(Value::as_unsigned_integer)
        .ok_or_else(|| anyhow!("MBFile has no ProtectionClass"))? as u32;
    let size = root.get("Size").and_then(Value::as_unsigned_integer);
    let key_obj = root
        .get("EncryptionKey")
        .and_then(deref)
        .ok_or_else(|| anyhow!("MBFile has no EncryptionKey"))?;
    let key_data = match &key_obj {
        Value::Data(d) => d.clone(),
        Value::Dictionary(d) => d
            .get("NS.data")
            .and_then(deref)
            .and_then(|v| v.as_data().map(<[u8]>::to_vec))
            .ok_or_else(|| anyhow!("EncryptionKey has no NS.data"))?,
        _ => bail!("unexpected EncryptionKey type"),
    };
    if key_data.len() < 4 + 24 {
        bail!("EncryptionKey too short");
    }
    Ok(FileCrypto {
        protection_class,
        wrapped_key: key_data[4..].to_vec(),
        size,
    })
}

/// An opened backup with an accessible (decrypted if necessary) manifest.
pub struct Backup {
    pub root: PathBuf,
    pub info: BackupInfo,
    manifest: Connection,
    keybag: Option<Keybag>,
    _tmp: Option<tempfile::TempDir>,
}

impl Backup {
    /// Open a backup. `password` is required for encrypted backups.
    pub fn open(root: &Path, password: Option<&str>) -> Result<Self> {
        let info = BackupInfo::read(root)?;
        let manifest_db = root.join("Manifest.db");
        if !manifest_db.is_file() {
            bail!(
                "{} has no Manifest.db (only iOS 10+ backups are supported)",
                root.display()
            );
        }
        if !info.encrypted {
            let manifest = Connection::open_with_flags(
                &manifest_db,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            return Ok(Self { root: root.to_path_buf(), info, manifest, keybag: None, _tmp: None });
        }

        let password = password.ok_or_else(|| anyhow!("backup is encrypted; a password is required"))?;
        let manifest_plist = read_dict(&root.join("Manifest.plist"))?;
        let keybag_data = manifest_plist
            .get("BackupKeyBag")
            .and_then(Value::as_data)
            .ok_or_else(|| anyhow!("Manifest.plist has no BackupKeyBag"))?;
        let manifest_key = manifest_plist
            .get("ManifestKey")
            .and_then(Value::as_data)
            .ok_or_else(|| anyhow!("Manifest.plist has no ManifestKey"))?;
        let mut keybag = Keybag::parse(keybag_data)?;
        keybag.unlock(password.as_bytes())?;
        if manifest_key.len() < 4 + 24 {
            bail!("ManifestKey too short");
        }
        let class = u32::from_le_bytes(manifest_key[..4].try_into().unwrap());
        let key = keybag.unwrap_key(class, &manifest_key[4..])?;

        let tmp = tempfile::tempdir()?;
        let plain = tmp.path().join("Manifest.db");
        {
            let mut input = BufReader::new(File::open(&manifest_db)?);
            let mut output = BufWriter::new(File::create(&plain)?);
            crypto::decrypt_stream(&key, &mut input, &mut output, None)?;
        }
        let manifest = Connection::open_with_flags(&plain, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        manifest
            .query_row("SELECT count(*) FROM Files", [], |r| r.get::<_, i64>(0))
            .context("decrypted Manifest.db is not readable (wrong password?)")?;
        Ok(Self { root: root.to_path_buf(), info, manifest, keybag: Some(keybag), _tmp: Some(tmp) })
    }

    pub fn is_encrypted(&self) -> bool {
        self.keybag.is_some()
    }

    fn entry_from_row(row: &rusqlite::Row) -> rusqlite::Result<FileEntry> {
        Ok(FileEntry {
            file_id: row.get(0)?,
            domain: row.get(1)?,
            relative_path: row.get(2)?,
            flags: row.get(3)?,
            file_blob: row.get(4)?,
        })
    }

    /// Look up a file by domain and relative path.
    pub fn find(&self, domain: &str, relative_path: &str) -> Result<Option<FileEntry>> {
        Ok(self
            .manifest
            .query_row(
                "SELECT fileID, domain, relativePath, flags, file FROM Files WHERE domain = ?1 AND relativePath = ?2",
                [domain, relative_path],
                Self::entry_from_row,
            )
            .optional()?)
    }

    /// Look up a file by its `fileID`.
    pub fn find_by_id(&self, file_id: &str) -> Result<Option<FileEntry>> {
        Ok(self
            .manifest
            .query_row(
                "SELECT fileID, domain, relativePath, flags, file FROM Files WHERE fileID = ?1",
                [file_id],
                Self::entry_from_row,
            )
            .optional()?)
    }

    /// On-disk location of a backup entry's (possibly encrypted) contents.
    pub fn stored_path(&self, entry: &FileEntry) -> PathBuf {
        hashed_path(&self.root, &entry.file_id)
    }

    /// Copy (decrypting if needed) a backup entry to `dest`, atomically.
    /// Returns the number of bytes written.
    pub fn copy_to(&self, entry: &FileEntry, dest: &Path) -> Result<u64> {
        let src = self.stored_path(entry);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_extension("partial");
        let written = {
            let mut input = BufReader::new(
                File::open(&src).with_context(|| format!("opening {}", src.display()))?,
            );
            let mut output = BufWriter::new(File::create(&tmp)?);
            let n = match &self.keybag {
                None => std::io::copy(&mut input, &mut output)?,
                Some(keybag) => {
                    let blob = entry
                        .file_blob
                        .as_deref()
                        .ok_or_else(|| anyhow!("{} has no encryption metadata", entry.relative_path))?;
                    let fc = parse_mbfile(blob)?;
                    let key = keybag.unwrap_key(fc.protection_class, &fc.wrapped_key)?;
                    crypto::decrypt_stream(&key, &mut input, &mut output, fc.size)?
                }
            };
            output.flush()?;
            n
        };
        fs::rename(&tmp, dest)?;
        Ok(written)
    }

    /// Copy a SQLite database (plus its `-wal` journal, if backed up) into
    /// `dir`, returning the path of the main database file.
    pub fn copy_sqlite(&self, domain: &str, relative_path: &str, dir: &Path) -> Result<Option<PathBuf>> {
        let Some(entry) = self.find(domain, relative_path)? else {
            return Ok(None);
        };
        let name = Path::new(relative_path)
            .file_name()
            .ok_or_else(|| anyhow!("bad path {relative_path}"))?;
        let dest = dir.join(name);
        self.copy_to(&entry, &dest)?;
        // The shared-memory index (`-shm`) is rebuilt by SQLite; only the WAL matters.
        if let Some(e) = self.find(domain, &format!("{relative_path}-wal"))? {
            let mut n = name.to_os_string();
            n.push("-wal");
            self.copy_to(&e, &dir.join(n))?;
        }
        Ok(Some(dest))
    }
}

/// Helpers for building synthetic backups in tests (unencrypted and encrypted).
pub mod fixture {
    use super::*;
    use std::collections::HashMap;

    pub struct FixtureBackup {
        pub root: PathBuf,
        manifest: Connection,
        encryption: Option<(String, HashMap<u32, [u8; 32]>)>,
    }

    impl FixtureBackup {
        pub fn create(root: &Path, udid: &str, password: Option<&str>) -> Result<Self> {
            fs::create_dir_all(root)?;
            let manifest = Connection::open_in_memory()?;
            manifest.execute_batch(
                "CREATE TABLE Files (fileID TEXT PRIMARY KEY, domain TEXT, relativePath TEXT, flags INTEGER, file BLOB);
                 CREATE TABLE Properties (key TEXT PRIMARY KEY, value BLOB);",
            )?;
            let encryption = password.map(|p| {
                let (bag, keys) = crypto::build_test_keybag(p.as_bytes(), &[1, 2, 3, 4]);
                (hex::encode(bag), keys)
            });
            let me = Self { root: root.to_path_buf(), manifest, encryption };
            me.write_info(udid, Utc::now())?;
            Ok(me)
        }

        pub fn write_info(&self, udid: &str, date: DateTime<Utc>) -> Result<()> {
            let mut info = plist::Dictionary::new();
            info.insert("Unique Identifier".into(), Value::String(udid.into()));
            info.insert("Device Name".into(), Value::String("Test iPhone".into()));
            info.insert("Product Type".into(), Value::String("iPhone15,2".into()));
            info.insert("Product Version".into(), Value::String("18.1".into()));
            info.insert(
                "Last Backup Date".into(),
                Value::Date(std::time::SystemTime::from(date).into()),
            );
            Value::Dictionary(info).to_file_xml(self.root.join("Info.plist"))?;
            Ok(())
        }

        pub fn add_file(&self, domain: &str, rel: &str, data: &[u8]) -> Result<String> {
            let id = file_id(domain, rel);
            let path = hashed_path(&self.root, &id);
            fs::create_dir_all(path.parent().unwrap())?;
            let blob = match &self.encryption {
                None => {
                    fs::write(&path, data)?;
                    None
                }
                Some((_, keys)) => {
                    let class = 3u32;
                    let file_key = [id.as_bytes()[0]; 32];
                    let mut key_data = class.to_le_bytes().to_vec();
                    key_data.extend(crypto::aes_wrap(&keys[&class], &file_key));
                    fs::write(&path, crypto::encrypt_bytes(&file_key, data))?;
                    Some(mbfile_blob(class, &key_data, data.len() as u64)?)
                }
            };
            self.manifest.execute(
                "INSERT OR REPLACE INTO Files VALUES (?1, ?2, ?3, 1, ?4)",
                rusqlite::params![id, domain, rel, blob],
            )?;
            Ok(id)
        }

        /// Write `Manifest.db` / `Manifest.plist`. Call after adding files.
        pub fn finish(&self) -> Result<()> {
            let plain = self.root.join("Manifest.db.plain");
            let _ = fs::remove_file(&plain);
            self.manifest.execute("VACUUM INTO ?1", [plain.to_string_lossy()])?;
            let mut mp = plist::Dictionary::new();
            match &self.encryption {
                None => {
                    fs::rename(&plain, self.root.join("Manifest.db"))?;
                    mp.insert("IsEncrypted".into(), Value::Boolean(false));
                }
                Some((bag, keys)) => {
                    let mkey = [0x5Au8; 32];
                    let data = fs::read(&plain)?;
                    fs::write(self.root.join("Manifest.db"), crypto::encrypt_bytes(&mkey, &data))?;
                    fs::remove_file(&plain)?;
                    let mut mk = 4u32.to_le_bytes().to_vec();
                    mk.extend(crypto::aes_wrap(&keys[&4], &mkey));
                    mp.insert("IsEncrypted".into(), Value::Boolean(true));
                    mp.insert("BackupKeyBag".into(), Value::Data(hex::decode(bag)?));
                    mp.insert("ManifestKey".into(), Value::Data(mk));
                }
            }
            Value::Dictionary(mp).to_file_binary(self.root.join("Manifest.plist"))?;
            Ok(())
        }
    }

    fn mbfile_blob(class: u32, key_data: &[u8], size: u64) -> Result<Vec<u8>> {
        let mut root = plist::Dictionary::new();
        root.insert("ProtectionClass".into(), Value::Integer(class.into()));
        root.insert("Size".into(), Value::Integer(size.into()));
        root.insert("EncryptionKey".into(), Value::Uid(plist::Uid::new(2)));
        let mut key = plist::Dictionary::new();
        key.insert("NS.data".into(), Value::Data(key_data.to_vec()));
        let objects = vec![
            Value::String("$null".into()),
            Value::Dictionary(root),
            Value::Dictionary(key),
        ];
        let mut top = plist::Dictionary::new();
        top.insert("root".into(), Value::Uid(plist::Uid::new(1)));
        let mut archive = plist::Dictionary::new();
        archive.insert("$archiver".into(), Value::String("NSKeyedArchiver".into()));
        archive.insert("$objects".into(), Value::Array(objects));
        archive.insert("$top".into(), Value::Dictionary(top));
        let mut out = Vec::new();
        Value::Dictionary(archive).to_writer_binary(&mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::FixtureBackup;
    use super::*;

    #[test]
    fn attachment_ids_match_imessage_database_hashing() {
        // imessage-database hashes "MediaDomain-" + path[2..] for "~/..." paths.
        let p = "~/Library/SMS/Attachments/ab/11/GUID/IMG_0001.HEIC";
        assert_eq!(
            attachment_file_id(p).unwrap(),
            file_id("MediaDomain", "Library/SMS/Attachments/ab/11/GUID/IMG_0001.HEIC")
        );
        assert_eq!(
            attachment_file_id("/var/mobile/Library/SMS/Attachments/x.jpg"),
            attachment_file_id("~/Library/SMS/Attachments/x.jpg")
        );
        assert_eq!(file_id(SMS_DB.0, SMS_DB.1), "3d0d7e5fb2ce288813306e4d4636395e047a3d28");
        assert_eq!(
            file_id(ADDRESS_BOOK_DB.0, ADDRESS_BOOK_DB.1),
            "31bb7ba8914766d4ba40d6dfb6113c8b614be442"
        );
    }

    fn roundtrip(password: Option<&str>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("00008110-TEST");
        let fx = FixtureBackup::create(&root, "00008110-test", password).unwrap();
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 253) as u8).collect();
        fx.add_file("MediaDomain", "Library/SMS/Attachments/a/b/c.jpg", &payload).unwrap();
        fx.finish().unwrap();

        assert!(Backup::open(&root, Some("nope")).is_err() || password.is_none());
        let backup = Backup::open(&root, password).unwrap();
        assert_eq!(backup.info.udid, "00008110-TEST");
        assert_eq!(backup.is_encrypted(), password.is_some());
        let id = attachment_file_id("~/Library/SMS/Attachments/a/b/c.jpg").unwrap();
        let entry = backup.find_by_id(&id).unwrap().unwrap();
        let out = dir.path().join("out.jpg");
        assert_eq!(backup.copy_to(&entry, &out).unwrap(), payload.len() as u64);
        assert_eq!(fs::read(&out).unwrap(), payload);
        assert_eq!(list_backups(dir.path()).unwrap().len(), 1);
    }

    #[test]
    fn unencrypted_backup() {
        roundtrip(None);
    }

    #[test]
    fn encrypted_backup() {
        roundtrip(Some("correct horse"));
    }
}
