//! Decryption support for encrypted Finder/iTunes backups.
//!
//! The scheme (documented by the community, e.g. on The Apple Wiki and in
//! projects such as `iphone_backup_decrypt` and MVT):
//!
//! 1. `Manifest.plist` holds a `BackupKeyBag` (TLV records) and a `ManifestKey`.
//! 2. The backup password is stretched with PBKDF2-SHA256 (`DPSL`/`DPIC`)
//!    and then PBKDF2-SHA1 (`SALT`/`ITER`) into a key-encryption key.
//! 3. That key unwraps (RFC 3394) each protection-class key in the keybag.
//! 4. `ManifestKey` = 4-byte little-endian class + wrapped key; it decrypts
//!    `Manifest.db` with AES-256-CBC and a zero IV.
//! 5. Every file row in `Manifest.db` carries its own wrapped `EncryptionKey`
//!    and `ProtectionClass`; the file body is AES-256-CBC (zero IV, PKCS#7).

use std::collections::HashMap;
use std::io::{Read, Write};

use aes::Aes256;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit, generic_array::GenericArray};
use anyhow::{Context, Result, anyhow, bail};
use sha1::Sha1;
use sha2::Sha256;

const WRAP_PASSCODE: u32 = 2;

/// A parsed backup keybag with (after [`Keybag::unlock`]) its class keys.
#[derive(Debug, Default)]
pub struct Keybag {
    attrs: HashMap<String, Vec<u8>>,
    class_keys: HashMap<u32, ClassKey>,
}

#[derive(Debug, Default, Clone)]
struct ClassKey {
    wrap: u32,
    wrapped: Vec<u8>,
    key: Option<[u8; 32]>,
}

fn be_u32(data: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(data.try_into().ok()?))
}

impl Keybag {
    /// Parse the TLV-encoded `BackupKeyBag` blob.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let mut bag = Keybag::default();
        let mut current: Option<(Option<u32>, ClassKey)> = None;
        let mut seen_uuid = false;
        let mut pos = 0;
        while pos + 8 <= data.len() {
            let tag = String::from_utf8_lossy(&data[pos..pos + 4]).to_string();
            let len = be_u32(&data[pos + 4..pos + 8]).unwrap() as usize;
            let start = pos + 8;
            let end = start
                .checked_add(len)
                .filter(|e| *e <= data.len())
                .ok_or_else(|| anyhow!("truncated keybag record {tag}"))?;
            let value = &data[start..end];
            pos = end;

            match tag.as_str() {
                "UUID" if !seen_uuid => {
                    seen_uuid = true;
                    bag.attrs.insert(tag, value.to_vec());
                }
                "UUID" => {
                    if let Some((Some(class), key)) = current.take() {
                        bag.class_keys.insert(class, key);
                    }
                    current = Some((None, ClassKey::default()));
                }
                "CLAS" | "WRAP" | "WPKY" | "KTYP" | "PBKY" if current.is_some() => {
                    let (class, key) = current.as_mut().unwrap();
                    match tag.as_str() {
                        "CLAS" => *class = be_u32(value),
                        "WRAP" => key.wrap = be_u32(value).unwrap_or(0),
                        "WPKY" => key.wrapped = value.to_vec(),
                        _ => {}
                    }
                }
                _ => {
                    bag.attrs.insert(tag, value.to_vec());
                }
            }
        }
        if let Some((Some(class), key)) = current.take() {
            bag.class_keys.insert(class, key);
        }
        if bag.class_keys.is_empty() {
            bail!("keybag contains no class keys");
        }
        Ok(bag)
    }

    fn attr(&self, name: &str) -> Result<&[u8]> {
        self.attrs
            .get(name)
            .map(Vec::as_slice)
            .ok_or_else(|| anyhow!("keybag is missing {name}"))
    }

    fn attr_u32(&self, name: &str) -> Result<u32> {
        be_u32(self.attr(name)?).ok_or_else(|| anyhow!("keybag attribute {name} is not a u32"))
    }

    /// Derive the passcode key from the backup password and unwrap every class key.
    pub fn unlock(&mut self, password: &[u8]) -> Result<()> {
        let mut stage1 = [0u8; 32];
        // Newer backups (iOS 10.2+) add a PBKDF2-SHA256 pre-stretch.
        let pre: &[u8] = match (self.attrs.get("DPSL"), self.attrs.get("DPIC")) {
            (Some(salt), Some(_)) => {
                pbkdf2::pbkdf2_hmac::<Sha256>(password, salt, self.attr_u32("DPIC")?, &mut stage1);
                &stage1
            }
            _ => password,
        };
        let mut passcode_key = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha1>(
            pre,
            self.attr("SALT")?,
            self.attr_u32("ITER")?,
            &mut passcode_key,
        );

        let mut unlocked = 0;
        for key in self.class_keys.values_mut() {
            if key.wrap & WRAP_PASSCODE == 0 || key.wrapped.len() != 40 {
                continue;
            }
            let k = aes_unwrap(&passcode_key, &key.wrapped)
                .context("incorrect backup password (could not unwrap class key)")?;
            key.key = Some(
                k.try_into()
                    .map_err(|_| anyhow!("unexpected class key length"))?,
            );
            unlocked += 1;
        }
        if unlocked == 0 {
            bail!("no class keys could be unlocked");
        }
        Ok(())
    }

    /// Unwrap a per-file (or manifest) key protected by `class`.
    pub fn unwrap_key(&self, class: u32, wrapped: &[u8]) -> Result<[u8; 32]> {
        let ck = self
            .class_keys
            .get(&class)
            .and_then(|c| c.key)
            .ok_or_else(|| anyhow!("no unlocked key for protection class {class}"))?;
        let key = aes_unwrap(&ck, wrapped)?;
        key.try_into()
            .map_err(|_| anyhow!("unexpected unwrapped key length"))
    }
}

const KW_IV: [u8; 8] = [0xA6; 8];

/// RFC 3394 AES key unwrap with the default IV.
pub fn aes_unwrap(kek: &[u8; 32], wrapped: &[u8]) -> Result<Vec<u8>> {
    if wrapped.len() < 24 || !wrapped.len().is_multiple_of(8) {
        bail!("wrapped key has invalid length {}", wrapped.len());
    }
    let cipher = Aes256::new(GenericArray::from_slice(kek));
    let n = wrapped.len() / 8 - 1;
    let mut a: [u8; 8] = wrapped[..8].try_into().unwrap();
    let mut r: Vec<[u8; 8]> = wrapped[8..]
        .chunks(8)
        .map(|c| c.try_into().unwrap())
        .collect();
    for j in (0..6).rev() {
        for i in (1..=n).rev() {
            let t = (n * j + i) as u64;
            let mut block = [0u8; 16];
            let a_xor = u64::from_be_bytes(a) ^ t;
            block[..8].copy_from_slice(&a_xor.to_be_bytes());
            block[8..].copy_from_slice(&r[i - 1]);
            let mut ga = GenericArray::from(block);
            cipher.decrypt_block(&mut ga);
            a.copy_from_slice(&ga[..8]);
            r[i - 1].copy_from_slice(&ga[8..]);
        }
    }
    if a != KW_IV {
        bail!("AES key unwrap integrity check failed");
    }
    Ok(r.concat())
}

/// RFC 3394 AES key wrap (used to build test fixtures).
pub fn aes_wrap(kek: &[u8; 32], key: &[u8]) -> Vec<u8> {
    assert!(key.len().is_multiple_of(8) && key.len() >= 16);
    let cipher = Aes256::new(GenericArray::from_slice(kek));
    let n = key.len() / 8;
    let mut a = KW_IV;
    let mut r: Vec<[u8; 8]> = key.chunks(8).map(|c| c.try_into().unwrap()).collect();
    for j in 0..6 {
        for i in 1..=n {
            let mut block = [0u8; 16];
            block[..8].copy_from_slice(&a);
            block[8..].copy_from_slice(&r[i - 1]);
            let mut ga = GenericArray::from(block);
            cipher.encrypt_block(&mut ga);
            let t = (n * j + i) as u64;
            a = (u64::from_be_bytes(ga[..8].try_into().unwrap()) ^ t).to_be_bytes();
            r[i - 1].copy_from_slice(&ga[8..]);
        }
    }
    let mut out = a.to_vec();
    for b in r {
        out.extend_from_slice(&b);
    }
    out
}

/// Stream-decrypt AES-256-CBC (zero IV) from `input` to `output`, removing
/// PKCS#7 padding and truncating to `size` when the manifest records it.
/// Returns the number of plaintext bytes written.
pub fn decrypt_stream(
    key: &[u8; 32],
    input: &mut impl Read,
    output: &mut impl Write,
    size: Option<u64>,
) -> Result<u64> {
    let cipher = Aes256::new(GenericArray::from_slice(key));
    let mut prev = [0u8; 16];
    let mut buf = vec![0u8; 64 * 1024];
    let mut filled = 0usize;
    // Hold back the last decrypted block so padding can be stripped at EOF.
    let mut pending: Option<[u8; 16]> = None;
    let mut written: u64 = 0;
    let limit = size.unwrap_or(u64::MAX);

    let emit = |data: &[u8], written: &mut u64, output: &mut dyn Write| -> Result<()> {
        let remaining = limit.saturating_sub(*written);
        let take = (data.len() as u64).min(remaining) as usize;
        output.write_all(&data[..take])?;
        *written += take as u64;
        Ok(())
    };

    loop {
        let n = input.read(&mut buf[filled..])?;
        filled += n;
        let eof = n == 0;
        let usable = filled - filled % 16;
        if usable > 0 {
            let mut plain = Vec::with_capacity(usable);
            for chunk in buf[..usable].chunks(16) {
                let c: [u8; 16] = chunk.try_into().unwrap();
                let mut ga = GenericArray::from(c);
                cipher.decrypt_block(&mut ga);
                let mut p = [0u8; 16];
                for i in 0..16 {
                    p[i] = ga[i] ^ prev[i];
                }
                prev = c;
                if let Some(last) = pending.replace(p) {
                    plain.extend_from_slice(&last);
                }
            }
            emit(&plain, &mut written, output)?;
            buf.copy_within(usable..filled, 0);
            filled -= usable;
        }
        if eof {
            if filled != 0 {
                bail!("ciphertext is not a multiple of the AES block size");
            }
            break;
        }
    }
    if let Some(last) = pending {
        let pad = last[15] as usize;
        let valid = (1..=16).contains(&pad) && last[16 - pad..].iter().all(|b| *b as usize == pad);
        let body = if valid && size.is_none_or(|s| written + (16 - pad) as u64 <= s) {
            &last[..16 - pad]
        } else {
            &last[..]
        };
        emit(body, &mut written, output)?;
    }
    output.flush()?;
    Ok(written)
}

/// AES-256-CBC (zero IV) with PKCS#7 padding (used to build test fixtures).
pub fn encrypt_bytes(key: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let cipher = Aes256::new(GenericArray::from_slice(key));
    let pad = 16 - data.len() % 16;
    let mut padded = data.to_vec();
    padded.extend(std::iter::repeat_n(pad as u8, pad));
    let mut prev = [0u8; 16];
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.chunks(16) {
        let mut block = [0u8; 16];
        for i in 0..16 {
            block[i] = chunk[i] ^ prev[i];
        }
        let mut ga = GenericArray::from(block);
        cipher.encrypt_block(&mut ga);
        prev.copy_from_slice(&ga);
        out.extend_from_slice(&ga);
    }
    out
}

/// Build a keybag blob for tests: returns (keybag bytes, class key map).
pub fn build_test_keybag(password: &[u8], classes: &[u32]) -> (Vec<u8>, HashMap<u32, [u8; 32]>) {
    fn rec(out: &mut Vec<u8>, tag: &str, value: &[u8]) {
        out.extend_from_slice(tag.as_bytes());
        out.extend_from_slice(&(value.len() as u32).to_be_bytes());
        out.extend_from_slice(value);
    }
    let salt = [7u8; 20];
    let dpsl = [9u8; 20];
    let (iter, dpic) = (10u32, 10u32);
    let mut stage1 = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(password, &dpsl, dpic, &mut stage1);
    let mut kek = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha1>(&stage1, &salt, iter, &mut kek);

    let mut out = Vec::new();
    rec(&mut out, "VERS", &4u32.to_be_bytes());
    rec(&mut out, "TYPE", &1u32.to_be_bytes());
    rec(&mut out, "UUID", &[1u8; 16]);
    rec(&mut out, "HMCK", &[0u8; 40]);
    rec(&mut out, "WRAP", &1u32.to_be_bytes());
    rec(&mut out, "SALT", &salt);
    rec(&mut out, "ITER", &iter.to_be_bytes());
    rec(&mut out, "DPWT", &1u32.to_be_bytes());
    rec(&mut out, "DPIC", &dpic.to_be_bytes());
    rec(&mut out, "DPSL", &dpsl);
    let mut keys = HashMap::new();
    for class in classes {
        let key = [(*class as u8).wrapping_mul(31).wrapping_add(5); 32];
        keys.insert(*class, key);
        rec(&mut out, "UUID", &[*class as u8; 16]);
        rec(&mut out, "CLAS", &class.to_be_bytes());
        rec(&mut out, "WRAP", &3u32.to_be_bytes());
        rec(&mut out, "KTYP", &0u32.to_be_bytes());
        rec(&mut out, "WPKY", &aes_wrap(&kek, &key));
    }
    (out, keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3394_vector() {
        // RFC 3394 section 4.6: 256-bit KEK wrapping 256-bit key.
        let kek: [u8; 32] =
            hex::decode("000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F")
                .unwrap()
                .try_into()
                .unwrap();
        let key = hex::decode("00112233445566778899AABBCCDDEEFF000102030405060708090A0B0C0D0E0F")
            .unwrap();
        let expected = hex::decode(
            "28C9F404C4B810F4CBCCB35CFB87F8263F5786E2D80ED326CBC7F0E71A99F43BFB988B9B7A02DD21",
        )
        .unwrap();
        assert_eq!(aes_wrap(&kek, &key), expected);
        assert_eq!(aes_unwrap(&kek, &expected).unwrap(), key);
    }

    #[test]
    fn keybag_roundtrip_and_stream_decrypt() {
        let (bag, keys) = build_test_keybag(b"hunter2", &[1, 3, 4]);
        let mut kb = Keybag::parse(&bag).unwrap();
        assert!(Keybag::parse(&bag).unwrap().unlock(b"wrong").is_err());
        kb.unlock(b"hunter2").unwrap();

        let file_key = [42u8; 32];
        let wrapped = aes_wrap(&keys[&3], &file_key);
        assert_eq!(kb.unwrap_key(3, &wrapped).unwrap(), file_key);

        for len in [0usize, 1, 15, 16, 17, 70_000, 131_072] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let enc = encrypt_bytes(&file_key, &data);
            let mut out = Vec::new();
            let n =
                decrypt_stream(&file_key, &mut enc.as_slice(), &mut out, Some(len as u64)).unwrap();
            assert_eq!(n as usize, len);
            assert_eq!(out, data);
            let mut out2 = Vec::new();
            decrypt_stream(&file_key, &mut enc.as_slice(), &mut out2, None).unwrap();
            assert_eq!(out2, data);
        }
    }
}
