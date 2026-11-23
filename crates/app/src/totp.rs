//! Two-factor (TOTP, RFC 6238) accounts for the Servers tab.
//!
//! Metadata (label, issuer, digits, period, algorithm) lives in `<state dir>/totp.json`;
//! the shared secrets live in the macOS Keychain (generic passwords, service
//! `juan-terminal-totp`, account = entry id), never in plain files.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

const KEYCHAIN_SERVICE: &str = "juan-terminal-totp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algorithm {
    Sha1,
    Sha256,
    Sha512,
}

impl Algorithm {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().replace('-', "").as_str() {
            "SHA1" => Some(Self::Sha1),
            "SHA256" => Some(Self::Sha256),
            "SHA512" => Some(Self::Sha512),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "SHA1",
            Self::Sha256 => "SHA256",
            Self::Sha512 => "SHA512",
        }
    }
}

/// A TOTP account without its secret (that one stays in the Keychain).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub label: String,
    pub issuer: String,
    pub digits: u32,
    pub period: u64,
    pub algorithm: Algorithm,
}

/// Parsed from user input (a base32 secret or an `otpauth://totp/...` URI).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub secret: Vec<u8>,
    pub label: String,
    pub issuer: String,
    pub digits: u32,
    pub period: u64,
    pub algorithm: Algorithm,
}

// ---- codes ---------------------------------------------------------------------------

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn hmac(alg: Algorithm, key: &[u8], msg: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    let block = if alg == Algorithm::Sha512 { 128 } else { 64 };
    let hash = |data: &[u8]| -> Vec<u8> {
        match alg {
            Algorithm::Sha1 => sha1_smol::Sha1::from(data).digest().bytes().to_vec(),
            Algorithm::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Algorithm::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    };
    let mut k = if key.len() > block { hash(key) } else { key.to_vec() };
    k.resize(block, 0);
    let mut inner: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    inner.extend_from_slice(msg);
    let mut outer: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    outer.extend_from_slice(&hash(&inner));
    hash(&outer)
}

/// The code for `secret` at Unix time `t`.
pub fn code(secret: &[u8], alg: Algorithm, digits: u32, period: u64, t: u64) -> String {
    let counter = t / period.max(1);
    let mac = hmac(alg, secret, &counter.to_be_bytes());
    let off = (mac[mac.len() - 1] & 0x0f) as usize;
    let bin = u32::from_be_bytes([mac[off] & 0x7f, mac[off + 1], mac[off + 2], mac[off + 3]]);
    let digits = digits.clamp(6, 10);
    let m = 10u64.pow(digits) as u32;
    let v = if digits >= 10 { bin } else { bin % m };
    format!("{:0width$}", v, width = digits as usize)
}

/// Seconds until the current code expires.
pub fn remaining(period: u64, t: u64) -> u64 {
    let p = period.max(1);
    p - t % p
}

// ---- parsing -------------------------------------------------------------------------

pub fn base32_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u64, 0u32);
    for c in s.chars() {
        if c.is_whitespace() || c == '-' || c == '=' {
            continue;
        }
        let v = match c.to_ascii_uppercase() {
            c @ 'A'..='Z' => c as u64 - 'A' as u64,
            c @ '2'..='7' => c as u64 - '2' as u64 + 26,
            _ => bail!("Not a base32 secret (invalid character '{c}')"),
        };
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    if out.is_empty() {
        bail!("Empty secret");
    }
    Ok(out)
}

pub fn base32_encode(data: &[u8]) -> String {
    const A: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(A[((buf >> bits) & 31) as usize] as char);
        }
        buf &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(A[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Accepts a base32 secret or an `otpauth://totp/Issuer:account?secret=...` URI.
pub fn parse_input(input: &str) -> Result<Parsed> {
    let input = input.trim();
    if input.to_ascii_lowercase().starts_with("otpauth-migration://") {
        bail!("Google Authenticator export QR codes aren't supported; export the account's own QR code instead");
    }
    if !input.to_ascii_lowercase().starts_with("otpauth://") {
        return Ok(Parsed { secret: base32_decode(input)?, label: String::new(), issuer: String::new(), digits: 6, period: 30, algorithm: Algorithm::Sha1 });
    }
    let rest = &input["otpauth://".len()..];
    let (kind, rest) = rest.split_once('/').ok_or_else(|| anyhow!("Malformed otpauth URI"))?;
    if !kind.eq_ignore_ascii_case("totp") {
        bail!("Only time-based codes (totp) are supported, this one is “{kind}”");
    }
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = percent_decode(path);
    let (mut issuer, label) = match path.split_once(':') {
        Some((i, l)) => (i.trim().to_string(), l.trim().to_string()),
        None => (String::new(), path.trim().to_string()),
    };
    let (mut secret, mut digits, mut period, mut algorithm) = (None, 6, 30, Algorithm::Sha1);
    for kv in query.split('&') {
        let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
        let v = percent_decode(v);
        match k.to_ascii_lowercase().as_str() {
            "secret" => secret = Some(base32_decode(&v)?),
            "issuer" if !v.trim().is_empty() => issuer = v.trim().to_string(),
            "digits" => digits = v.parse().ok().filter(|d| (6..=10).contains(d)).unwrap_or(6),
            "period" => period = v.parse().ok().filter(|p| *p > 0).unwrap_or(30),
            "algorithm" => algorithm = Algorithm::parse(&v).ok_or_else(|| anyhow!("Unsupported algorithm {v}"))?,
            _ => {}
        }
    }
    let secret = secret.ok_or_else(|| anyhow!("The otpauth URI has no secret"))?;
    Ok(Parsed { secret, label, issuer, digits, period, algorithm })
}

// ---- QR ------------------------------------------------------------------------------

/// Decode the first QR code in an encoded image (PNG/JPEG/…).
pub fn decode_qr_bytes(bytes: &[u8]) -> Result<String> {
    let img = image::load_from_memory(bytes).context("Couldn't read the image")?.to_luma8();
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(img.width() as usize, img.height() as usize, |x, y| img.get_pixel(x as u32, y as u32).0[0]);
    for grid in prepared.detect_grids() {
        if let Ok((_, content)) = grid.decode() {
            return Ok(content);
        }
    }
    bail!("No QR code found in the image")
}

pub fn decode_qr_file(path: &Path) -> Result<String> {
    decode_qr_bytes(&std::fs::read(path).with_context(|| format!("Couldn't read {}", path.display()))?)
}

/// Let the user drag a rectangle on screen (macOS `screencapture -i`) and decode a QR in it.
/// Returns Ok(None) when the user cancels (Esc).
pub fn scan_screen() -> Result<Option<String>> {
    let tmp = std::env::temp_dir().join(format!("juan-terminal-qr-{}.png", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let st = Command::new("/usr/sbin/screencapture").args(["-i", "-x"]).arg(&tmp).stdin(Stdio::null()).status().context("screencapture failed")?;
    if !st.success() || !tmp.is_file() {
        return Ok(None);
    }
    let r = decode_qr_file(&tmp);
    let _ = std::fs::remove_file(&tmp);
    r.map(Some)
}

// ---- storage -------------------------------------------------------------------------

fn meta_path() -> PathBuf {
    workspace_state::state_dir().join("totp.json")
}

pub fn load_accounts() -> Vec<Account> {
    let Ok(text) = std::fs::read_to_string(meta_path()) else { return Vec::new() };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    items
        .iter()
        .filter_map(|v| {
            Some(Account {
                id: v.get("id")?.as_str()?.to_string(),
                label: v.get("label").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                issuer: v.get("issuer").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                digits: v.get("digits").and_then(|x| x.as_u64()).unwrap_or(6) as u32,
                period: v.get("period").and_then(|x| x.as_u64()).filter(|p| *p > 0).unwrap_or(30),
                algorithm: v.get("algorithm").and_then(|x| x.as_str()).and_then(Algorithm::parse).unwrap_or(Algorithm::Sha1),
            })
        })
        .collect()
}

pub fn save_accounts(accounts: &[Account]) -> Result<()> {
    let arr: Vec<Value> = accounts.iter().map(|a| json!({"id": a.id, "label": a.label, "issuer": a.issuer, "digits": a.digits, "period": a.period, "algorithm": a.algorithm.name()})).collect();
    let path = meta_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Array(arr))?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn store_secret(id: &str, secret: &[u8]) -> Result<()> {
    let out = Command::new("/usr/bin/security")
        .args(["add-generic-password", "-U", "-s", KEYCHAIN_SERVICE, "-a", id, "-l", "juan-terminal 2FA", "-w", &base32_encode(secret)])
        .stdin(Stdio::null())
        .output()
        .context("Couldn't run security")?;
    if !out.status.success() {
        bail!("Keychain: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn load_secret(id: &str) -> Result<Vec<u8>> {
    let out = Command::new("/usr/bin/security").args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-a", id, "-w"]).stdin(Stdio::null()).output().context("Couldn't run security")?;
    if !out.status.success() {
        bail!("Secret not found in the Keychain");
    }
    base32_decode(String::from_utf8_lossy(&out.stdout).trim())
}

pub fn delete_secret(id: &str) {
    let _ = Command::new("/usr/bin/security").args(["delete-generic-password", "-s", KEYCHAIN_SERVICE, "-a", id]).stdin(Stdio::null()).output();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6238_vectors() {
        let s1 = b"12345678901234567890";
        let s256 = b"12345678901234567890123456789012";
        let s512 = b"1234567890123456789012345678901234567890123456789012345678901234";
        assert_eq!(code(s1, Algorithm::Sha1, 8, 30, 59), "94287082");
        assert_eq!(code(s256, Algorithm::Sha256, 8, 30, 59), "46119246");
        assert_eq!(code(s512, Algorithm::Sha512, 8, 30, 59), "90693936");
        assert_eq!(code(s1, Algorithm::Sha1, 8, 30, 1111111109), "07081804");
        assert_eq!(code(s1, Algorithm::Sha1, 6, 30, 59), "287082");
        assert_eq!(remaining(30, 59), 1);
        assert_eq!(remaining(30, 60), 30);
    }

    #[test]
    fn base32_roundtrip_and_uri() {
        let s = b"hello world 2fa!";
        assert_eq!(base32_decode(&base32_encode(s)).unwrap(), s);
        assert_eq!(base32_decode("jbsw y3dp").unwrap(), b"Hello");
        let p = parse_input("otpauth://totp/ACME%20Co:john@example.com?secret=JBSWY3DPEHPK3PXP&issuer=ACME%20Co&algorithm=SHA256&digits=8&period=60").unwrap();
        assert_eq!((p.issuer.as_str(), p.label.as_str(), p.digits, p.period, p.algorithm), ("ACME Co", "john@example.com", 8, 60, Algorithm::Sha256));
        assert!(parse_input("otpauth://hotp/x?secret=JBSWY3DP&counter=1").is_err());
        assert!(parse_input("not a secret!").is_err());
    }
}
