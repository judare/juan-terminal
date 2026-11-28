//! Google sign-in for the Calendar + Gmail plugin: OAuth 2.0 for installed apps (browser, a
//! loopback redirect to 127.0.0.1 and PKCE). The OAuth client (id + secret, type "Desktop
//! app", created once in Google Cloud Console) and the refresh token live in the Keychain;
//! only the account's address is kept in `<state>/google.json`. Read-only scopes.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serde_json::Value;

const SERVICE: &str = "Fragua Google";
const SCOPES: &str = "openid email https://www.googleapis.com/auth/calendar.readonly https://www.googleapis.com/auth/gmail.readonly";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Cached so renders never shell out to `security`.
static CONNECTED: Mutex<Option<bool>> = Mutex::new(None);
static TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);
/// A sign-in waiting for the browser (cancel it from the settings).
static CANCEL: AtomicBool = AtomicBool::new(false);

fn keychain_set(account: &str, value: &str) -> Result<()> {
    let out = Command::new("/usr/bin/security")
        .args(["add-generic-password", "-U", "-s", SERVICE, "-a", account, "-l", "Fragua Google", "-w", value])
        .stdin(Stdio::null())
        .output()
        .context("No se pudo ejecutar security")?;
    if !out.status.success() {
        bail!("Llavero: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn keychain_get(account: &str) -> Option<String> {
    let out = Command::new("/usr/bin/security").args(["find-generic-password", "-s", SERVICE, "-a", account, "-w"]).stdin(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

fn keychain_delete(account: &str) {
    let _ = Command::new("/usr/bin/security").args(["delete-generic-password", "-s", SERVICE, "-a", account]).stdin(Stdio::null()).output();
}

fn state_file() -> std::path::PathBuf {
    workspace_state::state_dir().join("google.json")
}

/// Signed in (a refresh token is stored).
pub fn connected() -> bool {
    if let Some(v) = *CONNECTED.lock().unwrap() {
        return v;
    }
    let v = keychain_get("refresh_token").is_some();
    *CONNECTED.lock().unwrap() = Some(v);
    v
}

pub fn email() -> Option<String> {
    let v: Value = serde_json::from_slice(&std::fs::read(state_file()).ok()?).ok()?;
    v["email"].as_str().map(str::to_string)
}

/// The OAuth client saved before (client id; the secret stays in the Keychain).
pub fn client_id() -> Option<String> {
    keychain_get("client_id")
}

pub fn has_client_secret() -> bool {
    keychain_get("client_secret").is_some()
}

pub fn save_client(id: &str, secret: &str) -> Result<()> {
    let id = id.trim();
    anyhow::ensure!(id.ends_with(".apps.googleusercontent.com"), "El Client ID termina en .apps.googleusercontent.com");
    keychain_set("client_id", id)?;
    if !secret.trim().is_empty() {
        keychain_set("client_secret", secret.trim())?;
    }
    Ok(())
}

fn random_string(n: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut bytes = vec![0u8; n];
    let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut bytes));
    bytes.iter().map(|b| CHARS[*b as usize % CHARS.len()] as char).collect()
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 2;
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).http_status_as_error(false).build().into()
}

fn post_form(url: &str, pairs: &[(&str, &str)]) -> Result<Value> {
    let body = pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&");
    let mut resp = agent().post(url).header("Content-Type", "application/x-www-form-urlencoded").send(body).map_err(|e| anyhow!("{e}"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().map_err(|e| anyhow!("{e}"))?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status >= 400 {
        let msg = v["error_description"].as_str().or(v["error"].as_str()).unwrap_or(&text).to_string();
        bail!("Google: {msg}");
    }
    Ok(v)
}

/// Stop a sign-in waiting for the browser.
pub fn cancel_login() {
    CANCEL.store(true, Ordering::Relaxed);
}

/// Sign in: opens the browser, waits (up to 5 min) for Google to redirect back, stores the
/// refresh token. Blocking: run it off the UI thread. Returns the account's address.
pub fn login() -> Result<String> {
    let client_id = client_id().ok_or_else(|| anyhow!("Guarda primero el Client ID"))?;
    let secret = keychain_get("client_secret").unwrap_or_default();
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let verifier = random_string(64);
    let challenge = {
        use base64::Engine as _;
        use sha2::Digest as _;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
    };
    let state = random_string(24);
    let url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={challenge}&code_challenge_method=S256&access_type=offline&prompt=consent&state={state}",
        enc(&client_id),
        enc(&redirect),
        enc(SCOPES)
    );
    let _ = Command::new("/usr/bin/open").arg(&url).spawn();
    CANCEL.store(false, Ordering::Relaxed);
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(300);
    let code = loop {
        if CANCEL.load(Ordering::Relaxed) {
            bail!("Conexión cancelada");
        }
        if Instant::now() > deadline {
            bail!("Se agotó el tiempo esperando a Google (5 min)");
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                let query = line.split_whitespace().nth(1).and_then(|p| p.split_once('?')).map(|(_, q)| q.to_string()).unwrap_or_default();
                let params: Vec<(String, String)> = query.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), dec(v))).collect();
                let get = |k: &str| params.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
                if get("code").is_none() && get("error").is_none() {
                    // Favicon or other noise: keep waiting.
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    continue;
                }
                let ok = get("state").as_deref() == Some(state.as_str()) && get("code").is_some();
                let page = if ok {
                    "<h2 style='font-family:-apple-system'>Listo. Ya puedes volver a Fragua.</h2>"
                } else {
                    "<h2 style='font-family:-apple-system'>No se pudo conectar. Vuelve a Fragua e inténtalo de nuevo.</h2>"
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len());
                if let Some(err) = get("error") {
                    bail!("Google rechazó el acceso: {err}");
                }
                if !ok {
                    bail!("Respuesta de Google inválida (state)");
                }
                break get("code").unwrap_or_default();
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(150)),
            Err(e) => return Err(e.into()),
        }
    };
    let mut pairs = vec![("code", code.as_str()), ("client_id", client_id.as_str()), ("redirect_uri", redirect.as_str()), ("grant_type", "authorization_code"), ("code_verifier", verifier.as_str())];
    if !secret.is_empty() {
        pairs.push(("client_secret", secret.as_str()));
    }
    let tok = post_form(TOKEN_URL, &pairs)?;
    let refresh = tok["refresh_token"].as_str().ok_or_else(|| anyhow!("Google no devolvió refresh_token"))?;
    let access = tok["access_token"].as_str().unwrap_or("").to_string();
    let expires = tok["expires_in"].as_u64().unwrap_or(3600);
    keychain_set("refresh_token", refresh)?;
    *TOKEN.lock().unwrap() = Some((access.clone(), Instant::now() + Duration::from_secs(expires.saturating_sub(60))));
    *CONNECTED.lock().unwrap() = Some(true);
    let email = userinfo_email(&access).unwrap_or_default();
    let _ = std::fs::write(state_file(), serde_json::json!({ "email": email }).to_string());
    Ok(email)
}

fn userinfo_email(access: &str) -> Option<String> {
    let mut r = agent().get("https://openidconnect.googleapis.com/v1/userinfo").header("Authorization", &format!("Bearer {access}")).call().ok()?;
    let v: Value = serde_json::from_str(&r.body_mut().read_to_string().ok()?).ok()?;
    v["email"].as_str().map(str::to_string)
}

/// A valid access token (refreshed when it's about to expire). Blocking.
pub fn access_token() -> Result<String> {
    if let Some((t, exp)) = TOKEN.lock().unwrap().clone() {
        if Instant::now() < exp {
            return Ok(t);
        }
    }
    let refresh = keychain_get("refresh_token").ok_or_else(|| anyhow!("No hay cuenta de Google conectada"))?;
    let client_id = client_id().ok_or_else(|| anyhow!("Falta el Client ID"))?;
    let secret = keychain_get("client_secret").unwrap_or_default();
    let mut pairs = vec![("refresh_token", refresh.as_str()), ("client_id", client_id.as_str()), ("grant_type", "refresh_token")];
    if !secret.is_empty() {
        pairs.push(("client_secret", secret.as_str()));
    }
    let tok = match post_form(TOKEN_URL, &pairs) {
        Ok(t) => t,
        Err(e) => {
            // Revoked / expired grant: sign out so the settings ask to connect again.
            if format!("{e}").contains("invalid_grant") || format!("{e}").contains("expired or revoked") {
                keychain_delete("refresh_token");
                *CONNECTED.lock().unwrap() = Some(false);
                bail!("La sesión de Google expiró: vuelve a conectar la cuenta");
            }
            return Err(e);
        }
    };
    let access = tok["access_token"].as_str().ok_or_else(|| anyhow!("Google no devolvió access_token"))?.to_string();
    let expires = tok["expires_in"].as_u64().unwrap_or(3600);
    *TOKEN.lock().unwrap() = Some((access.clone(), Instant::now() + Duration::from_secs(expires.saturating_sub(60))));
    Ok(access)
}

/// Sign out: revoke the token at Google and forget it here (the OAuth client stays).
pub fn disconnect() {
    if let Some(refresh) = keychain_get("refresh_token") {
        let _ = post_form("https://oauth2.googleapis.com/revoke", &[("token", refresh.as_str())]);
    }
    keychain_delete("refresh_token");
    *TOKEN.lock().unwrap() = None;
    *CONNECTED.lock().unwrap() = Some(false);
    let _ = std::fs::remove_file(state_file());
}

/// GET a Google API URL with the account's token. Blocking.
pub fn get_json(url: &str) -> Result<Value> {
    let token = access_token()?;
    let mut resp = agent().get(url).header("Authorization", &format!("Bearer {token}")).call().map_err(|e| anyhow!("{e}"))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().map_err(|e| anyhow!("{e}"))?;
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status == 401 {
        *TOKEN.lock().unwrap() = None;
    }
    if status >= 400 {
        let msg = v["error"]["message"].as_str().unwrap_or(&text).to_string();
        if status == 403 && msg.contains("has not been used") {
            bail!("Activa la API en Google Cloud Console: {msg}");
        }
        bail!("Google ({status}): {msg}");
    }
    Ok(v)
}

pub fn urlencode(s: &str) -> String {
    enc(s)
}
