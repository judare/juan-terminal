//! HTTP client data: collections and environments stored as Postman v2.1 JSON (so a Postman
//! export imports by copying it, and anything we don't edit — scripts, docs, examples — is kept
//! as is), `{{variable}}` resolution, inherited auth, and sending through `curl`.
//!
//! Layout: `<state>/http/collections/*.json`, `<state>/http/environments/*.json`,
//! `<state>/http/state.json` (active environment).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::plugins::{self, HTTP};

pub fn dir() -> PathBuf {
    workspace_state::state_dir().join("http")
}
fn collections_dir() -> PathBuf {
    dir().join("collections")
}
fn environments_dir() -> PathBuf {
    dir().join("environments")
}

#[derive(Clone)]
pub struct Collection {
    pub file: PathBuf,
    pub json: Value,
}

impl Collection {
    pub fn name(&self) -> String {
        self.json["info"]["name"].as_str().unwrap_or("Colección").to_string()
    }

    pub fn save(&self) -> Result<()> {
        write_json(&self.file, &self.json)
    }

    /// The item at `path` (indexes into nested `item` arrays).
    pub fn item(&self, path: &[usize]) -> Option<&Value> {
        let mut v = &self.json;
        for &i in path {
            v = v.get("item")?.get(i)?;
        }
        Some(v)
    }

    pub fn item_mut(&mut self, path: &[usize]) -> Option<&mut Value> {
        let mut v = &mut self.json;
        for &i in path {
            v = v.get_mut("item")?.get_mut(i)?;
        }
        Some(v)
    }

    /// Number of requests (folders not counted).
    pub fn request_count(&self) -> usize {
        fn count(v: &Value) -> usize {
            v["item"].as_array().map(|a| a.iter().map(|i| if i.get("request").is_some() { 1 } else { count(i) }).sum()).unwrap_or(0)
        }
        count(&self.json)
    }

    /// Auth the folders above `path` (or the collection) give it.
    pub fn inherited_auth(&self, path: &[usize]) -> Auth {
        for depth in (0..path.len()).rev() {
            if let Some(a) = self.item(&path[..depth]).and_then(|i| i.get("auth")).map(Auth::from_json).filter(|a| *a != Auth::Inherit) {
                return a;
            }
        }
        Auth::None
    }

    pub fn variables(&self) -> Vec<(String, String)> {
        kv_list(&self.json["variable"])
    }
}

#[derive(Clone)]
pub struct Environment {
    pub file: PathBuf,
    pub json: Value,
}

impl Environment {
    pub fn name(&self) -> String {
        self.json["name"].as_str().unwrap_or("Entorno").to_string()
    }
    pub fn id(&self) -> String {
        self.file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }
    pub fn variables(&self) -> Vec<(String, String)> {
        kv_list(&self.json["values"])
    }
}

/// Enabled `{key, value}` pairs of a Postman variable list.
fn kv_list(v: &Value) -> Vec<(String, String)> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter(|x| x["enabled"].as_bool() != Some(false) && x["disabled"].as_bool() != Some(true))
                .filter_map(|x| Some((x["key"].as_str()?.to_string(), value_text(&x["value"]))))
                .collect()
        })
        .unwrap_or_default()
}

fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn list_dir(dir: &Path) -> Vec<(PathBuf, Value)> {
    let mut out: Vec<(PathBuf, Value)> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).filter_map(|p| read_json(&p).map(|v| (p, v))).collect())
        .unwrap_or_default();
    out.sort_by_key(|(p, _)| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    out
}

pub fn load_collections() -> Vec<Collection> {
    let mut v: Vec<Collection> = list_dir(&collections_dir()).into_iter().map(|(file, json)| Collection { file, json }).collect();
    v.sort_by_key(|c| c.name().to_lowercase());
    v
}

pub fn load_environments() -> Vec<Environment> {
    let mut v: Vec<Environment> = list_dir(&environments_dir()).into_iter().map(|(file, json)| Environment { file, json }).collect();
    v.sort_by_key(|e| e.name().to_lowercase());
    v
}

pub fn active_environment() -> Option<String> {
    read_json(&dir().join("state.json"))?["active_env"].as_str().map(str::to_string)
}

pub fn set_active_environment(id: Option<&str>) {
    let _ = write_json(&dir().join("state.json"), &json!({ "active_env": id }));
}

fn slug(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
    let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
    if s.is_empty() { "coleccion".into() } else { s.chars().take(60).collect() }
}

fn free_path(dir: &Path, name: &str) -> PathBuf {
    let base = slug(name);
    let mut p = dir.join(format!("{base}.json"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base}-{n}.json"));
        n += 1;
    }
    p
}

// ---- Postman import -----------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Collection,
    Environment,
}

/// What a JSON file is, judged by its content.
pub fn detect_value(v: &Value) -> Option<Kind> {
    let schema = v["info"]["schema"].as_str().unwrap_or("");
    if schema.contains("getpostman.com/json/collection") || (v["info"].is_object() && v["item"].is_array()) {
        return Some(Kind::Collection);
    }
    if v["_postman_variable_scope"].as_str().is_some_and(|s| s == "environment" || s == "globals") || (v["values"].is_array() && v["name"].is_string()) {
        return Some(Kind::Environment);
    }
    None
}

/// Postman collection / environment file (cheap check for dropped files: name first, then
/// content of small JSON files).
pub fn is_postman_file(path: &Path) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    if name.ends_with(".postman_collection.json") || name.ends_with(".postman_environment.json") || name.ends_with(".postman_globals.json") {
        return true;
    }
    if !name.ends_with(".json") || std::fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX) > 50 * 1024 * 1024 {
        return false;
    }
    let Some(v) = read_json(path) else { return false };
    detect_value(&v).is_some() || is_postman_v1(&v)
}

fn is_postman_v1(v: &Value) -> bool {
    v["requests"].is_array() && v["order"].is_array()
}

/// Copy a Postman export into the store. Returns what it was and the stored file.
pub fn import(path: &Path) -> Result<(Kind, PathBuf, String)> {
    let v = read_json(path).with_context(|| format!("{} no es JSON válido", path.display()))?;
    if is_postman_v1(&v) {
        bail!("{} está en el formato v1 de Postman. Expórtala de nuevo como Collection v2.1.", path.display());
    }
    match detect_value(&v) {
        Some(Kind::Collection) => {
            let name = v["info"]["name"].as_str().unwrap_or("Colección").to_string();
            let file = free_path(&collections_dir(), &name);
            write_json(&file, &v)?;
            Ok((Kind::Collection, file, name))
        }
        Some(Kind::Environment) => {
            let name = v["name"].as_str().unwrap_or("Entorno").to_string();
            let file = free_path(&environments_dir(), &name);
            write_json(&file, &v)?;
            Ok((Kind::Environment, file, name))
        }
        None => bail!("{} no es una colección ni un entorno de Postman", path.display()),
    }
}

/// New empty collection.
pub fn new_collection(name: &str) -> Result<Collection> {
    let json = json!({
        "info": { "name": name, "schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json" },
        "item": [],
    });
    let c = Collection { file: free_path(&collections_dir(), name), json };
    c.save()?;
    Ok(c)
}

/// New empty environment.
pub fn new_environment(name: &str) -> Result<Environment> {
    let json = json!({ "name": name, "values": [{ "key": "base_url", "value": "https://", "enabled": true }], "_postman_variable_scope": "environment" });
    let e = Environment { file: free_path(&environments_dir(), name), json };
    write_json(&e.file, &e.json)?;
    Ok(e)
}

pub fn new_request_json(name: &str) -> Value {
    json!({ "name": name, "request": { "method": "GET", "header": [], "url": "" } })
}

// ---- request form -----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Auth {
    Inherit,
    None,
    Bearer(String),
    Basic(String, String),
    ApiKey { key: String, value: String, in_query: bool },
    /// A type we don't edit (oauth2, digest…): left untouched.
    Other(String),
}

impl Auth {
    pub fn from_json(v: &Value) -> Auth {
        let ty = v["type"].as_str().unwrap_or("");
        let get = |k: &str| -> String {
            v[ty].as_array().and_then(|a| a.iter().find(|x| x["key"] == k)).map(|x| value_text(&x["value"])).unwrap_or_default()
        };
        match ty {
            "" | "inherit" => Auth::Inherit,
            "noauth" => Auth::None,
            "bearer" => Auth::Bearer(get("token")),
            "basic" => Auth::Basic(get("username"), get("password")),
            "apikey" => Auth::ApiKey { key: get("key"), value: get("value"), in_query: get("in") == "query" },
            other => Auth::Other(other.to_string()),
        }
    }

    pub fn to_json(&self) -> Option<Value> {
        let kv = |pairs: &[(&str, &str)]| -> Value { Value::Array(pairs.iter().map(|(k, v)| json!({"key": k, "value": v, "type": "string"})).collect()) };
        Some(match self {
            Auth::Inherit => return None,
            Auth::None => json!({"type": "noauth"}),
            Auth::Bearer(t) => json!({"type": "bearer", "bearer": kv(&[("token", t)])}),
            Auth::Basic(u, p) => json!({"type": "basic", "basic": kv(&[("username", u), ("password", p)])}),
            Auth::ApiKey { key, value, in_query } => json!({"type": "apikey", "apikey": kv(&[("key", key), ("value", value), ("in", if *in_query { "query" } else { "header" })])}),
            Auth::Other(_) => return None,
        })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Auth::Inherit => "Heredada",
            Auth::None => "Sin auth",
            Auth::Bearer(_) => "Bearer token",
            Auth::Basic(..) => "Basic",
            Auth::ApiKey { .. } => "API key",
            Auth::Other(_) => "Otra (de Postman)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BodyMode {
    None,
    Json,
    Text,
    UrlEncoded,
    FormData,
    GraphQl,
}

impl BodyMode {
    pub const ALL: [BodyMode; 6] = [BodyMode::None, BodyMode::Json, BodyMode::Text, BodyMode::UrlEncoded, BodyMode::FormData, BodyMode::GraphQl];
    pub fn label(self) -> &'static str {
        match self {
            BodyMode::None => "Ninguno",
            BodyMode::Json => "JSON",
            BodyMode::Text => "Texto",
            BodyMode::UrlEncoded => "Form URL",
            BodyMode::FormData => "Multipart",
            BodyMode::GraphQl => "GraphQL",
        }
    }
    pub fn hint(self) -> &'static str {
        match self {
            BodyMode::UrlEncoded => "Una línea por campo: clave=valor  (# al inicio la desactiva)",
            BodyMode::FormData => "Una línea por campo: clave=valor, o clave=@/ruta/archivo  (# la desactiva)",
            BodyMode::GraphQl => "Query de GraphQL; variables en JSON después de una línea con ---",
            _ => "",
        }
    }
}

/// The editable part of a request, as text the page's inputs show.
#[derive(Clone, Debug, PartialEq)]
pub struct Form {
    pub name: String,
    pub method: String,
    pub url: String,
    /// `Key: Value` per line; `# ` in front = disabled.
    pub headers: String,
    pub body_mode: BodyMode,
    pub body: String,
    pub auth: Auth,
}

fn lines_from_pairs(v: &Value, sep: &str) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|h| {
                    let off = h["disabled"].as_bool() == Some(true);
                    let key = h["key"].as_str().unwrap_or("");
                    let val = if h["type"] == "file" { format!("@{}", value_text(&h["src"])) } else { value_text(&h["value"]) };
                    format!("{}{key}{sep}{val}", if off { "# " } else { "" })
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn pairs_from_lines(text: &str, sep: char, files: bool) -> Value {
    Value::Array(
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let (off, l) = match l.trim_start().strip_prefix('#') {
                    Some(rest) => (true, rest.trim_start()),
                    None => (false, l.trim_start()),
                };
                let (k, v) = l.split_once(sep)?;
                let (k, v) = (k.trim(), v.trim_start());
                let mut o = if files && v.starts_with('@') { json!({"key": k, "type": "file", "src": &v[1..]}) } else { json!({"key": k, "value": v, "type": "text"}) };
                if off {
                    o["disabled"] = json!(true);
                }
                Some(o)
            })
            .collect(),
    )
}

pub fn url_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(_) => v["raw"].as_str().map(str::to_string).unwrap_or_else(|| {
            // Rebuild from parts (some exporters leave out `raw`).
            let host = v["host"].as_array().map(|h| h.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(".")).unwrap_or_default();
            let path = v["path"].as_array().map(|p| p.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("/")).unwrap_or_default();
            let proto = v["protocol"].as_str().map(|p| format!("{p}://")).unwrap_or_default();
            format!("{proto}{host}/{path}")
        }),
        _ => String::new(),
    }
}

impl Form {
    pub fn from_item(item: &Value) -> Form {
        let r = &item["request"];
        // A request can be a bare URL string in Postman.
        let (method, url) = match r {
            Value::String(s) => ("GET".to_string(), s.clone()),
            _ => (r["method"].as_str().unwrap_or("GET").to_uppercase(), url_text(&r["url"])),
        };
        let body = &r["body"];
        let (body_mode, body_text) = match body["mode"].as_str().unwrap_or("") {
            "raw" => {
                let lang = body["options"]["raw"]["language"].as_str().unwrap_or("");
                let text = body["raw"].as_str().unwrap_or("").to_string();
                let json_like = lang == "json" || (lang.is_empty() && serde_json::from_str::<Value>(&text).is_ok());
                (if json_like { BodyMode::Json } else { BodyMode::Text }, text)
            }
            "urlencoded" => (BodyMode::UrlEncoded, lines_from_pairs(&body["urlencoded"], "=")),
            "formdata" => (BodyMode::FormData, lines_from_pairs(&body["formdata"], "=")),
            "graphql" => {
                let q = body["graphql"]["query"].as_str().unwrap_or("").to_string();
                let vars = body["graphql"]["variables"].as_str().unwrap_or("").trim().to_string();
                (BodyMode::GraphQl, if vars.is_empty() { q } else { format!("{q}\n---\n{vars}") })
            }
            _ => (BodyMode::None, String::new()),
        };
        Form {
            name: item["name"].as_str().unwrap_or("Request").to_string(),
            method,
            url,
            headers: lines_from_pairs(&r["header"], ": "),
            body_mode,
            body: body_text,
            auth: r.get("auth").map(Auth::from_json).unwrap_or(Auth::Inherit),
        }
    }

    /// Write the form back into the item, keeping the fields we don't edit.
    pub fn apply(&self, item: &mut Value) {
        item["name"] = json!(self.name);
        if !item["request"].is_object() {
            item["request"] = json!({});
        }
        let r = &mut item["request"];
        r["method"] = json!(self.method);
        // Keep the object form (query/variables) only while its raw text is unchanged.
        if url_text(&r["url"]) != self.url {
            r["url"] = json!(self.url);
        }
        r["header"] = pairs_from_lines(&self.headers, ':', false);
        let keep_auth = matches!(self.auth, Auth::Other(_));
        match self.auth.to_json() {
            Some(a) => r["auth"] = a,
            None if !keep_auth => {
                if let Some(o) = r.as_object_mut() {
                    o.remove("auth");
                }
            }
            None => {}
        }
        let body = match self.body_mode {
            BodyMode::None => None,
            BodyMode::Json => Some(json!({"mode": "raw", "raw": self.body, "options": {"raw": {"language": "json"}}})),
            BodyMode::Text => Some(json!({"mode": "raw", "raw": self.body, "options": {"raw": {"language": "text"}}})),
            BodyMode::UrlEncoded => Some(json!({"mode": "urlencoded", "urlencoded": pairs_from_lines(&self.body, '=', false)})),
            BodyMode::FormData => Some(json!({"mode": "formdata", "formdata": pairs_from_lines(&self.body, '=', true)})),
            BodyMode::GraphQl => {
                let (q, vars) = split_graphql(&self.body);
                Some(json!({"mode": "graphql", "graphql": {"query": q, "variables": vars}}))
            }
        };
        match body {
            Some(b) => r["body"] = b,
            None => {
                if let Some(o) = r.as_object_mut() {
                    o.remove("body");
                }
            }
        }
    }
}

fn split_graphql(text: &str) -> (String, String) {
    match text.split_once("\n---\n") {
        Some((q, v)) => (q.to_string(), v.trim().to_string()),
        None => (text.to_string(), String::new()),
    }
}

// ---- variables ----------------------------------------------------------------------------

/// Replace `{{name}}` with environment values (which win) or collection values; `{{$guid}}`,
/// `{{$timestamp}}`, `{{$isoTimestamp}}` and `{{$randomInt}}` are generated. Unknown names stay.
pub fn resolve(text: &str, vars: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for _ in 0..5 {
        if !out.contains("{{") {
            break;
        }
        let mut next = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(start) = rest.find("{{") {
            next.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                next.push_str(&rest[start..]);
                rest = "";
                break;
            };
            let name = after[..end].trim();
            match lookup(name, vars) {
                Some(v) => next.push_str(&v),
                None => next.push_str(&rest[start..start + 2 + end + 2]),
            }
            rest = &after[end + 2..];
        }
        next.push_str(rest);
        if next == out {
            break;
        }
        out = next;
    }
    out
}

fn lookup(name: &str, vars: &[(String, String)]) -> Option<String> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    match name {
        "$guid" | "$randomUUID" => Some(uuid_v4()),
        "$timestamp" => Some(now.as_secs().to_string()),
        "$isoTimestamp" => Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
        "$randomInt" => Some((random_u64() % 1001).to_string()),
        _ => vars.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v.clone()),
    }
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    if std::fs::File::open("/dev/urandom").and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b)).is_err() {
        return std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(7);
    }
    u64::from_le_bytes(b)
}

fn uuid_v4() -> String {
    let (a, b) = (random_u64(), random_u64());
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_le_bytes());
    bytes[8..].copy_from_slice(&b.to_le_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h: String = bytes.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Names used as `{{name}}` in `text` that have no value (shown as a warning).
pub fn missing_vars(text: &str, vars: &[(String, String)]) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(s) = rest.find("{{") {
        let after = &rest[s + 2..];
        let Some(e) = after.find("}}") else { break };
        let name = after[..e].trim().to_string();
        if !name.starts_with('$') && lookup(&name, vars).is_none() && !out.contains(&name) {
            out.push(name);
        }
        rest = &after[e + 2..];
    }
    out
}

// ---- sending ------------------------------------------------------------------------------

/// A request with variables and auth applied, ready for curl.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<PreparedBody>,
}

#[derive(Clone, Debug)]
pub enum PreparedBody {
    Raw(String),
    UrlEncoded(Vec<(String, String)>),
    /// (key, value, is_file)
    Form(Vec<(String, String, bool)>),
}

fn enabled_pairs(text: &str, sep: char) -> Vec<(String, String)> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .filter_map(|l| l.split_once(sep).map(|(k, v)| (k.trim().to_string(), v.trim_start().to_string())))
        .collect()
}

pub fn prepare(form: &Form, auth: &Auth, vars: &[(String, String)]) -> Prepared {
    let r = |s: &str| resolve(s, vars);
    let mut url = r(form.url.trim());
    if !url.contains("://") && !url.is_empty() {
        url = format!("https://{url}");
    }
    let mut headers: Vec<(String, String)> = enabled_pairs(&form.headers, ':').into_iter().map(|(k, v)| (r(&k), r(&v))).collect();
    let has = |headers: &[(String, String)], name: &str| headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    match auth {
        Auth::Bearer(t) if !has(&headers, "authorization") => headers.push(("Authorization".into(), format!("Bearer {}", r(t)))),
        Auth::Basic(u, p) if !has(&headers, "authorization") => {
            use base64::Engine as _;
            headers.push(("Authorization".into(), format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", r(u), r(p))))));
        }
        Auth::ApiKey { key, value, in_query: false } if !key.is_empty() => headers.push((r(key), r(value))),
        Auth::ApiKey { key, value, in_query: true } if !key.is_empty() => {
            let sep = if url.contains('?') { '&' } else { '?' };
            url = format!("{url}{sep}{}={}", urlencode(&r(key)), urlencode(&r(value)));
        }
        _ => {}
    }
    let body = match form.body_mode {
        BodyMode::None => None,
        BodyMode::Json => {
            if !has(&headers, "content-type") {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
            Some(PreparedBody::Raw(r(&form.body)))
        }
        BodyMode::Text => Some(PreparedBody::Raw(r(&form.body))),
        BodyMode::GraphQl => {
            let (q, v) = split_graphql(&form.body);
            let vars_json: Value = serde_json::from_str(&r(&v)).unwrap_or(Value::Null);
            if !has(&headers, "content-type") {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
            Some(PreparedBody::Raw(json!({"query": r(&q), "variables": vars_json}).to_string()))
        }
        BodyMode::UrlEncoded => Some(PreparedBody::UrlEncoded(enabled_pairs(&form.body, '=').into_iter().map(|(k, v)| (r(&k), r(&v))).collect())),
        BodyMode::FormData => Some(PreparedBody::Form(
            enabled_pairs(&form.body, '=')
                .into_iter()
                .map(|(k, v)| match v.strip_prefix('@') {
                    Some(path) => (r(&k), r(path), true),
                    None => (r(&k), r(&v), false),
                })
                .collect(),
        )),
    };
    Prepared { method: form.method.clone(), url, headers, body }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Clone, Debug, Default)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub millis: u128,
    pub size: u64,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// curl couldn't complete the request (DNS, refused, timeout…).
    pub error: Option<String>,
}

impl Response {
    pub fn content_type(&self) -> String {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-type")).map(|(_, v)| v.to_lowercase()).unwrap_or_default()
    }

    /// Body as text, pretty-printed when it's JSON.
    pub fn pretty_body(&self) -> String {
        let text = String::from_utf8_lossy(&self.body);
        if self.content_type().contains("json") || text.trim_start().starts_with(['{', '[']) {
            if let Ok(v) = serde_json::from_slice::<Value>(&self.body) {
                return serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.into_owned());
            }
        }
        text.into_owned()
    }

    /// Extension for "open in editor".
    pub fn extension(&self) -> &'static str {
        let ct = self.content_type();
        if ct.contains("json") {
            "json"
        } else if ct.contains("html") {
            "html"
        } else if ct.contains("xml") {
            "xml"
        } else if ct.contains("csv") {
            "csv"
        } else if self.body.trim_ascii_start().starts_with(b"{") || self.body.trim_ascii_start().starts_with(b"[") {
            "json"
        } else {
            "txt"
        }
    }
}

fn curl_args(p: &Prepared, for_shell: bool) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    if p.method != "GET" || p.body.is_some() {
        a.push("-X".into());
        a.push(p.method.clone());
    }
    for (k, v) in &p.headers {
        a.push("-H".into());
        a.push(format!("{k}: {v}"));
    }
    match &p.body {
        Some(PreparedBody::Raw(b)) => {
            a.push("--data-binary".into());
            // Through stdin when run here (no size limit, no `@` surprises).
            a.push(if for_shell { b.clone() } else { "@-".into() });
        }
        Some(PreparedBody::UrlEncoded(pairs)) => {
            for (k, v) in pairs {
                a.push("--data-urlencode".into());
                a.push(format!("{k}={v}"));
            }
        }
        Some(PreparedBody::Form(fields)) => {
            for (k, v, file) in fields {
                a.push("--form-string".into());
                if *file {
                    a.pop();
                    a.push("-F".into());
                    a.push(format!("{k}=@{v}"));
                } else {
                    a.push(format!("{k}={v}"));
                }
            }
        }
        None => {}
    }
    if plugins::setting(HTTP, "follow_redirects") {
        a.push("-L".into());
    }
    if !plugins::setting(HTTP, "verify_tls") {
        a.push("-k".into());
    }
    a.push(p.url.clone());
    a
}

/// `curl …` command line equivalent to the request (copy as cURL).
pub fn as_curl(p: &Prepared) -> String {
    let mut s = String::from("curl");
    for arg in curl_args(p, true) {
        s.push(' ');
        s.push_str(&crate::terminal_view::shell_quote(&arg));
    }
    s
}

/// Send with curl, blocking. `pid` gets the child's pid so the UI can cancel it.
pub fn send(p: &Prepared, pid: Arc<AtomicU32>) -> Response {
    let started = Instant::now();
    let tmp = std::env::temp_dir().join(format!("fragua-http-{}-{}", std::process::id(), random_u64()));
    let _ = std::fs::create_dir_all(&tmp);
    let (hfile, bfile) = (tmp.join("headers"), tmp.join("body"));
    let timeout = plugins::choice_number(HTTP, "timeout").unwrap_or(60);
    let mut cmd = Command::new("/usr/bin/curl");
    cmd.args(["-sS", "--compressed", "-D"]).arg(&hfile).arg("-o").arg(&bfile).args(["-w", "%{http_code} %{size_download}", "--max-time", &timeout.to_string()]);
    cmd.args(curl_args(p, false));
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let out = match cmd.spawn() {
        Ok(mut child) => {
            pid.store(child.id(), Ordering::Relaxed);
            if let Some(mut stdin) = child.stdin.take() {
                if let Some(PreparedBody::Raw(b)) = &p.body {
                    let _ = std::io::Write::write_all(&mut stdin, b.as_bytes());
                }
            }
            child.wait_with_output()
        }
        Err(e) => Err(e),
    };
    pid.store(0, Ordering::Relaxed);
    let mut resp = Response { millis: started.elapsed().as_millis(), ..Default::default() };
    match out {
        Err(e) => resp.error = Some(format!("No se pudo ejecutar curl: {e}")),
        Ok(o) => {
            let w = String::from_utf8_lossy(&o.stdout);
            let mut parts = w.split_whitespace();
            resp.status = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            resp.size = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            if !o.status.success() {
                let err = String::from_utf8_lossy(&o.stderr).trim().trim_start_matches("curl: ").to_string();
                resp.error = Some(match o.status.code() {
                    None => "Cancelado".into(),
                    Some(28) => format!("Tiempo agotado ({timeout} s)"),
                    _ if err.is_empty() => format!("curl salió con {}", o.status),
                    _ => err,
                });
            }
            let raw = std::fs::read_to_string(&hfile).unwrap_or_default();
            // With redirects / 100-continue there are several blocks: the last one is the answer.
            if let Some(block) = raw.split("\r\n\r\n").filter(|b| b.starts_with("HTTP/")).last() {
                let mut lines = block.lines();
                if let Some(status) = lines.next() {
                    resp.reason = status.splitn(3, ' ').nth(2).unwrap_or("").trim().to_string();
                }
                resp.headers = lines.filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect();
            }
            resp.body = std::fs::read(&bfile).unwrap_or_default();
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
    resp
}

/// Stop a request in flight.
pub fn cancel(pid: &AtomicU32) {
    let p = pid.load(Ordering::Relaxed);
    if p > 0 {
        unsafe {
            libc::kill(p as i32, libc::SIGTERM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_variables() {
        let vars = vec![("base".to_string(), "https://api.x".to_string()), ("v".to_string(), "{{base}}/v1".to_string())];
        assert_eq!(resolve("{{v}}/users/{{id}}", &vars), "https://api.x/v1/users/{{id}}");
        assert_eq!(missing_vars("{{v}} {{id}} {{$guid}}", &vars), vec!["id".to_string()]);
        assert_eq!(uuid_v4().len(), 36);
    }

    #[test]
    fn form_round_trip_keeps_unknown_fields() {
        let mut item = json!({
            "name": "Crear",
            "event": [{"listen": "test"}],
            "request": {
                "method": "post",
                "url": {"raw": "{{base}}/users?x=1", "host": ["{{base}}"], "query": [{"key": "x", "value": "1"}]},
                "header": [{"key": "X-A", "value": "1"}, {"key": "X-B", "value": "2", "disabled": true}],
                "body": {"mode": "raw", "raw": "{\"a\":1}", "options": {"raw": {"language": "json"}}},
                "auth": {"type": "bearer", "bearer": [{"key": "token", "value": "{{tok}}"}]}
            }
        });
        let f = Form::from_item(&item);
        assert_eq!(f.method, "POST");
        assert_eq!(f.url, "{{base}}/users?x=1");
        assert_eq!(f.headers, "X-A: 1\n# X-B: 2");
        assert_eq!(f.body_mode, BodyMode::Json);
        assert_eq!(f.auth, Auth::Bearer("{{tok}}".into()));
        f.apply(&mut item);
        assert!(item["event"].is_array());
        assert!(item["request"]["url"]["query"].is_array());
        assert_eq!(Form::from_item(&item), f);
        let p = prepare(&f, &f.auth, &[("base".into(), "https://h".into()), ("tok".into(), "T".into())]);
        assert_eq!(p.url, "https://h/users?x=1");
        assert!(p.headers.contains(&("Authorization".into(), "Bearer T".into())));
        assert!(!p.headers.iter().any(|(k, _)| k == "X-B"));
    }

    #[test]
    fn detects_postman_files() {
        assert_eq!(detect_value(&json!({"info": {"schema": "https://schema.getpostman.com/json/collection/v2.1.0/collection.json"}, "item": []})), Some(Kind::Collection));
        assert_eq!(detect_value(&json!({"name": "Prod", "values": [], "_postman_variable_scope": "environment"})), Some(Kind::Environment));
        assert_eq!(detect_value(&json!({"name": "x"})), None);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore = "network"]
    fn sends_json_post() {
        let form = Form {
            name: "t".into(),
            method: "POST".into(),
            url: "{{base}}/post?x=1".into(),
            headers: "X-Test: {{v}}".into(),
            body_mode: BodyMode::Json,
            body: "{\"a\": {{v}}}".into(),
            auth: Auth::Bearer("tok".into()),
        };
        let vars = vec![("base".to_string(), "https://httpbin.org".to_string()), ("v".to_string(), "7".to_string())];
        let p = prepare(&form, &form.auth, &vars);
        let r = send(&p, Arc::new(AtomicU32::new(0)));
        assert_eq!(r.error, None);
        assert_eq!(r.status, 200);
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["json"]["a"], 7);
        assert_eq!(v["headers"]["Authorization"], "Bearer tok");
        assert_eq!(v["headers"]["X-Test"], "7");
        assert!(r.content_type().contains("json"));
        assert!(as_curl(&p).starts_with("curl -X POST"));
    }
}
