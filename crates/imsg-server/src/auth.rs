//! Password authentication (argon2 hashes in an htpasswd-style file),
//! database-backed sessions and login throttling.

use std::collections::HashMap;
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
use sha2::{Digest, Sha256};

pub const COOKIE: &str = "imsg_session";

/// `username -> argon2 PHC string`.
pub struct Users {
    users: HashMap<String, String>,
    dummy: String,
}

impl Users {
    /// Parse lines of `username:$argon2id$...` (blank lines and `#` comments ignored).
    pub fn parse(text: &str) -> Result<Self> {
        let mut users = HashMap::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((user, hash)) = line.split_once(':') else {
                bail!("line {}: expected username:hash", n + 1);
            };
            PasswordHash::new(hash)
                .map_err(|e| anyhow::anyhow!("line {}: invalid hash: {e}", n + 1))?;
            users.insert(user.trim().to_owned(), hash.trim().to_owned());
        }
        Ok(Self {
            users,
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
        self.users.is_empty()
    }

    pub fn len(&self) -> usize {
        self.users.len()
    }

    /// Verify credentials. Unknown users still pay for a hash verification so
    /// timing does not reveal which usernames exist.
    pub fn verify(&self, user: &str, password: &str) -> bool {
        let (hash, known) = match self.users.get(user) {
            Some(h) => (h.as_str(), true),
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
    fn users_and_cookies() {
        let h = hash_password("pw").unwrap();
        let users = Users::parse(&format!("# comment\nalice:{h}\n")).unwrap();
        assert!(users.verify("alice", "pw"));
        assert!(!users.verify("alice", "nope"));
        assert!(!users.verify("bob", "pw"));
        let tok = "a".repeat(64);
        assert_eq!(
            cookie_token(&format!("x=1; imsg_session={tok}; y=2")),
            Some(tok.as_str())
        );
        assert_eq!(cookie_token("imsg_session=../../etc"), None);
    }
}
