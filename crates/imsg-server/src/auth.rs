//! Accounts (argon2 password hashes plus what each account may see),
//! database-backed sessions and login throttling.
//!
//! The accounts file is TOML:
//!
//! ```toml
//! [[account]]
//! name = "alice"
//! password = "$argon2id$v=19$..."      # from `imsg-server hash-password`
//! devices = ["00008110-000A1B2C3D4E5F6A"] # device UDIDs whose backups she sees ("*" = all)
//! imessage_ids = ["+15550109999", "alice@icloud.com"]  # her numbers / Apple IDs, on any device
//! admin = false                           # admins may also view other accounts
//! ```
//!
//! The older one-line-per-user format (`username:$argon2id$...`) is still
//! accepted; such users are admins who see every device.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use argon2::Argon2;
use argon2::password_hash::{
    PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng,
};
use rand::RngCore;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::view::View;

pub const COOKIE: &str = "imsg_session";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    pub name: String,
    pub password: String,
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub imessage_ids: Vec<String>,
    #[serde(default)]
    pub admin: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountsFile {
    #[serde(default)]
    account: Vec<AccountConfig>,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    hash: String,
    pub admin: bool,
    pub devices: Vec<String>,
    pub imessage_ids: Vec<String>,
    pub view: View,
}

/// All configured accounts.
pub struct Accounts {
    accounts: BTreeMap<String, Account>,
    dummy: String,
}

fn check_hash(hash: &str, what: &str) -> Result<()> {
    PasswordHash::new(hash).map_err(|e| anyhow::anyhow!("{what}: invalid password hash: {e}"))?;
    Ok(())
}

impl Accounts {
    pub fn parse(text: &str) -> Result<Self> {
        let mut accounts = BTreeMap::new();
        let looks_legacy = text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .is_some_and(|l| !l.starts_with('[') && l.contains(":$"));
        if looks_legacy {
            for (n, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let Some((user, hash)) = line.split_once(':') else {
                    bail!("line {}: expected username:hash", n + 1);
                };
                check_hash(hash.trim(), &format!("line {}", n + 1))?;
                let name = user.trim().to_owned();
                accounts.insert(
                    name.clone(),
                    Account {
                        name,
                        hash: hash.trim().to_owned(),
                        admin: true,
                        devices: vec!["*".into()],
                        imessage_ids: vec![],
                        view: View::everything(),
                    },
                );
            }
        } else {
            let file: AccountsFile = toml::from_str(text).context("parsing accounts file")?;
            for a in file.account {
                let what = format!("account {:?}", a.name);
                if a.name.is_empty() || a.name.contains(char::is_whitespace) {
                    bail!("{what}: names must be non-empty and contain no spaces");
                }
                check_hash(&a.password, &what)?;
                let view = View::new(&a.devices, &a.imessage_ids);
                if view.is_empty() && !a.admin {
                    bail!("{what}: grant at least one device or iMessage ID (or make it an admin)");
                }
                if accounts.contains_key(&a.name) {
                    bail!("{what} is defined twice");
                }
                accounts.insert(
                    a.name.clone(),
                    Account {
                        name: a.name,
                        hash: a.password,
                        admin: a.admin,
                        devices: a.devices,
                        imessage_ids: a.imessage_ids,
                        view,
                    },
                );
            }
        }
        Ok(Self {
            accounts,
            dummy: hash_password("not-a-real-password")?,
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        Self::parse(
            &std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?,
        )
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn get(&self, name: &str) -> Option<&Account> {
        self.accounts.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Account> {
        self.accounts.values()
    }

    /// Verify credentials. Unknown users still pay for a hash verification so
    /// timing does not reveal which usernames exist.
    pub fn verify(&self, user: &str, password: &str) -> bool {
        let (hash, known) = match self.accounts.get(user) {
            Some(a) => (a.hash.as_str(), true),
            None => (self.dummy.as_str(), false),
        };
        let ok = PasswordHash::new(hash)
            .map(|parsed| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &parsed)
                    .is_ok()
            })
            .unwrap_or(false);
        ok && known
    }
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_string())
}

fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn create_session(conn: &Connection, user: &str, ttl: Duration) -> Result<String> {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token = hex::encode(bytes);
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute("DELETE FROM sessions WHERE expires_ms < ?1", [now])?;
    conn.execute(
        "INSERT INTO sessions (token_hash, user, expires_ms) VALUES (?1, ?2, ?3)",
        params![token_hash(&token), user, now + ttl.as_millis() as i64],
    )?;
    Ok(token)
}

pub fn session_user(conn: &Connection, token: &str) -> Result<Option<String>> {
    let now = chrono::Utc::now().timestamp_millis();
    Ok(conn
        .query_row(
            "SELECT user FROM sessions WHERE token_hash = ?1 AND expires_ms > ?2",
            params![token_hash(token), now],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn delete_session(conn: &Connection, token: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM sessions WHERE token_hash = ?1",
        [token_hash(token)],
    )?;
    Ok(())
}

/// Extract our session cookie from a `Cookie:` header value.
pub fn cookie_token(header: &str) -> Option<&str> {
    header
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v)
        .filter(|v| v.len() == 64 && v.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Simple per-IP lockout: `MAX_FAILURES` failed logins within `WINDOW` blocks
/// that address until the window expires.
#[derive(Default)]
pub struct Throttle {
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

const MAX_FAILURES: u32 = 10;
const WINDOW: Duration = Duration::from_secs(15 * 60);

impl Throttle {
    pub fn blocked(&self, ip: IpAddr) -> bool {
        let mut map = self.failures.lock().unwrap();
        map.retain(|_, (_, since)| since.elapsed() < WINDOW);
        map.get(&ip).is_some_and(|(n, _)| *n >= MAX_FAILURES)
    }

    pub fn fail(&self, ip: IpAddr) {
        let mut map = self.failures.lock().unwrap();
        let e = map.entry(ip).or_insert((0, Instant::now()));
        e.0 += 1;
    }

    pub fn succeed(&self, ip: IpAddr) {
        self.failures.lock().unwrap().remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_users_and_cookies() {
        let h = hash_password("pw").unwrap();
        let users = Accounts::parse(&format!("# comment\nalice:{h}\n")).unwrap();
        assert!(users.verify("alice", "pw"));
        assert!(!users.verify("alice", "nope"));
        assert!(!users.verify("bob", "pw"));
        assert!(users.get("alice").unwrap().admin);
        assert!(users.get("alice").unwrap().view.all);
        let tok = "a".repeat(64);
        assert_eq!(
            cookie_token(&format!("x=1; imsg_session={tok}; y=2")),
            Some(tok.as_str())
        );
        assert_eq!(cookie_token("imsg_session=../../etc"), None);
    }

    #[test]
    fn toml_accounts() {
        let h = hash_password("pw").unwrap();
        let text = format!(
            "[[account]]\nname = \"alice\"\npassword = \"{h}\"\ndevices = [\"dev-a\"]\n\n\
             [[account]]\nname = \"bob\"\npassword = \"{h}\"\nimessage_ids = [\"+1 555 010 0001\"]\nadmin = true\n"
        );
        let a = Accounts::parse(&text).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a.get("alice").unwrap().view.devices, ["DEV-A"]);
        assert!(!a.get("alice").unwrap().admin);
        assert_eq!(a.get("bob").unwrap().view.owners, ["5550100001"]);
        assert!(a.verify("bob", "pw"));
        let err = Accounts::parse(&format!("[[account]]\nname = \"x\"\npassword = \"{h}\"\n"))
            .err()
            .unwrap();
        assert!(err.to_string().contains("grant at least one"));
        assert!(
            Accounts::parse(&format!(
                "[[account]]\nname = \"x\"\npassword = \"{h}\"\ndevice = [\"typo\"]\n"
            ))
            .is_err()
        );
    }
}
