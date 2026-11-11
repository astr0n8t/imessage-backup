//! Time-based one-time passwords (RFC 6238: HMAC-SHA1, 30-second steps,
//! 6 digits), compatible with Google Authenticator, 1Password, Authy, etc.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Result, bail};
use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha1::Sha1;

pub const STEP_SECS: u64 = 30;
pub const DIGITS: u32 = 6;
/// Accept codes from one step before/after the current one (clock skew).
const SKEW: i64 = 1;

/// A decoded TOTP secret.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(..)")
    }
}

impl Secret {
    /// 160 random bits, the size RFC 4226 recommends.
    pub fn generate() -> Self {
        let mut bytes = vec![0u8; 20];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    /// Parse a base32 secret (case, spaces, dashes and `=` padding ignored).
    pub fn parse(base32: &str) -> Result<Self> {
        let clean: String = base32
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-' && *c != '=')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let bytes = BASE32_NOPAD
            .decode(clean.as_bytes())
            .map_err(|e| anyhow::anyhow!("TOTP secret is not valid base32: {e}"))?;
        if bytes.len() < 10 {
            bail!("TOTP secret is too short (need at least 80 bits)");
        }
        Ok(Self(bytes))
    }

    pub fn to_base32(&self) -> String {
        BASE32_NOPAD.encode(&self.0)
    }

    /// The code for a given time step.
    pub fn code_at_step(&self, step: u64) -> u32 {
        let mut mac = Hmac::<Sha1>::new_from_slice(&self.0).expect("HMAC accepts any key length");
        mac.update(&step.to_be_bytes());
        let digest = mac.finalize().into_bytes();
        let offset = (digest[19] & 0x0f) as usize;
        let bin = u32::from_be_bytes(digest[offset..offset + 4].try_into().unwrap()) & 0x7fff_ffff;
        bin % 10u32.pow(DIGITS)
    }

    /// If `code` is valid at unix time `now`, the time step it matched.
    pub fn matching_step(&self, code: &str, now: u64) -> Option<u64> {
        let code: String = code
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect();
        if code.len() != DIGITS as usize || !code.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        let wanted: u32 = code.parse().ok()?;
        let current = (now / STEP_SECS) as i64;
        let mut found = None;
        // Check every candidate (no early exit) to keep timing uniform.
        for delta in -SKEW..=SKEW {
            let step = current + delta;
            if step >= 0 && constant_eq(self.code_at_step(step as u64), wanted) {
                found = Some(step as u64);
            }
        }
        found
    }

    /// `otpauth://` URI for authenticator apps / QR codes.
    pub fn uri(&self, issuer: &str, account: &str) -> String {
        let enc = |s: &str| -> String {
            s.bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                        (b as char).to_string()
                    }
                    _ => format!("%{b:02X}"),
                })
                .collect()
        };
        format!(
            "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECS}",
            enc(issuer),
            enc(account),
            self.to_base32(),
            enc(issuer)
        )
    }
}

fn constant_eq(a: u32, b: u32) -> bool {
    (a ^ b) == 0
}

/// Terminal-printable QR code for `data`. By default it is drawn for a dark
/// terminal background (light blocks are the QR's white); `invert` swaps
/// that for light backgrounds.
pub fn qr_text(data: &str, invert: bool) -> Result<String> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::new(data.as_bytes())?;
    let (dark, light) = if invert {
        (Dense1x2::Dark, Dense1x2::Light)
    } else {
        (Dense1x2::Light, Dense1x2::Dark)
    };
    Ok(code
        .render::<Dense1x2>()
        .dark_color(dark)
        .light_color(light)
        .quiet_zone(true)
        .build())
}

/// Remembers the last accepted step per account so a code can't be replayed.
#[derive(Default)]
pub struct ReplayGuard {
    last: Mutex<HashMap<String, u64>>,
}

impl ReplayGuard {
    /// Accept `step` for `account` only if it is newer than the last one used.
    pub fn accept(&self, account: &str, step: u64) -> bool {
        let mut map = self.last.lock().unwrap();
        match map.get(account) {
            Some(prev) if *prev >= step => false,
            _ => {
                map.insert(account.to_owned(), step);
                true
            }
        }
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vectors() {
        // RFC 6238 appendix B, SHA-1, truncated to 6 digits.
        let s = Secret(b"12345678901234567890".to_vec());
        for (t, code) in [
            (59u64, 287082u32),
            (1111111109, 81804),
            (1111111111, 50471),
            (1234567890, 5924),
            (2000000000, 279037),
            (20000000000, 353130),
        ] {
            assert_eq!(s.code_at_step(t / STEP_SECS), code, "t={t}");
            assert_eq!(
                s.matching_step(&format!("{code:06}"), t),
                Some(t / STEP_SECS)
            );
        }
    }

    #[test]
    fn window_and_parsing() {
        let s = Secret::generate();
        let t = 1_700_000_000;
        let prev = format!("{:06}", s.code_at_step(t / STEP_SECS - 1));
        let old = format!("{:06}", s.code_at_step(t / STEP_SECS - 3));
        assert!(s.matching_step(&prev, t).is_some());
        assert!(s.matching_step(&old, t).is_none());
        assert!(s.matching_step("12345", t).is_none());
        assert!(s.matching_step("abcdef", t).is_none());
        let spaced = format!("{} {}", &prev[..3], &prev[3..]);
        assert!(s.matching_step(&spaced, t).is_some());

        let b32 = s.to_base32();
        let again = Secret::parse(&b32.to_lowercase()).unwrap();
        assert_eq!(again.code_at_step(5), s.code_at_step(5));
        assert!(Secret::parse("not base32!").is_err());
        assert!(Secret::parse("AAAA").is_err());
        assert!(
            s.uri("Messages Archive", "alice")
                .starts_with("otpauth://totp/Messages%20Archive:alice?secret=")
        );
        assert!(qr_text("otpauth://totp/x", false).unwrap().contains('█'));
    }

    #[test]
    fn replay() {
        let g = ReplayGuard::default();
        assert!(g.accept("a", 10));
        assert!(!g.accept("a", 10));
        assert!(!g.accept("a", 9));
        assert!(g.accept("a", 11));
        assert!(g.accept("b", 10));
    }
}
