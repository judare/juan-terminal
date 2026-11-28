//! Agent usage statistics (Claude Code + Codex) for the usage dashboard.
//!
//! Both agents write append-only JSONL transcripts (`~/.claude/projects/**`, `~/.codex/sessions/**`).
//! They're scanned incrementally (byte offset per file) and folded into hourly buckets kept in
//! `<state dir>/usage.json`, so history survives app restarts and outlives the transcripts the
//! agents delete on their own (Claude Code prunes sessions after 30 days by default).

use std::collections::{HashMap, HashSet};
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

pub const CLAUDE: u8 = 0;
pub const CODEX: u8 = 1;
pub const AGENT_NAMES: [&str; 2] = ["Claude Code", "Codex"];

const CACHE_VERSION: u32 = 1;
/// Gaps between transcript events longer than this don't count as agent time.
const IDLE_GAP: i64 = 300;

/// Usage of one session with one model during one UTC hour.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Bucket {
    pub agent: u8,
    pub model: String,
    pub session: String,
    /// Unix time / 3600 (UTC).
    pub hour: i64,
    /// All input tokens, cache reads and writes included.
    pub input: u64,
    /// Input tokens served from cache.
    pub cached: u64,
    pub output: u64,
    /// User prompts.
    pub turns: u32,
    /// Seconds of activity.
    pub active: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
struct FileState {
    offset: u64,
    last_ts: i64,
    #[serde(default)]
    model: String,
    #[serde(default)]
    session: String,
    /// Codex: cumulative total of the last token_count event (repeats are skipped).
    #[serde(default)]
    last_total: u64,
}

#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    version: u32,
    files: HashMap<String, FileState>,
    buckets: Vec<Bucket>,
    /// Hashes of Claude message ids already counted (resumed/forked sessions repeat them).
    seen: Vec<u64>,
}

type Key = (u8, String, String, i64);

#[derive(Default)]
struct Store {
    files: HashMap<String, FileState>,
    buckets: HashMap<Key, Bucket>,
    seen: HashSet<u64>,
}

fn store() -> &'static Mutex<Option<Store>> {
    static S: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

static SCANNING: AtomicBool = AtomicBool::new(false);
static DONE_BYTES: AtomicU64 = AtomicU64::new(0);
static TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);

/// Fraction of the current scan already read (for the "indexing" label).
pub fn progress() -> Option<f32> {
    SCANNING.load(Ordering::Relaxed).then(|| {
        let t = TOTAL_BYTES.load(Ordering::Relaxed).max(1);
        DONE_BYTES.load(Ordering::Relaxed) as f32 / t as f32
    })
}

fn cache_path() -> PathBuf {
    workspace_state::state_dir().join("usage.json")
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn codex_dir() -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join("sessions"),
        _ => home().join(".codex/sessions"),
    }
}

fn load_store() -> Store {
    let Ok(bytes) = std::fs::read(cache_path()) else { return Store::default() };
    let Ok(c) = serde_json::from_slice::<CacheFile>(&bytes) else { return Store::default() };
    if c.version != CACHE_VERSION {
        return Store::default();
    }
    Store {
        files: c.files,
        buckets: c.buckets.into_iter().map(|b| ((b.agent, b.model.clone(), b.session.clone(), b.hour), b)).collect(),
        seen: c.seen.into_iter().collect(),
    }
}

fn save_store(s: &Store) -> std::io::Result<()> {
    let mut buckets: Vec<Bucket> = s.buckets.values().cloned().collect();
    buckets.sort_by(|a, b| (a.hour, a.agent, &a.session, &a.model).cmp(&(b.hour, b.agent, &b.session, &b.model)));
    let c = CacheFile { version: CACHE_VERSION, files: s.files.clone(), buckets, seen: s.seen.iter().copied().collect() };
    let path = cache_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&c)?)?;
    std::fs::rename(tmp, path)
}

/// Hand freed heap pages back to the OS after a big scan (macOS malloc keeps them otherwise,
/// so the process footprint stays at its peak).
fn release_freed_memory() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // SAFETY: null zone = all zones, goal 0 = release as much as possible.
        unsafe { malloc_zone_pressure_relief(std::ptr::null_mut(), 0) };
    }
}

fn jsonl_files(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if depth > 0 {
                jsonl_files(&p, depth - 1, out);
            }
        } else if p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
}

/// Reads whatever the agents appended since the last scan, saves the cache and returns every
/// bucket. Blocking: call from a background thread. Concurrent calls wait for the running one.
pub fn refresh() -> Arc<Vec<Bucket>> {
    let mut guard = store().lock().unwrap_or_else(|e| e.into_inner());
    let s = guard.get_or_insert_with(load_store);

    let mut jobs: Vec<(PathBuf, u8, FileState, u64)> = Vec::new();
    let add = |dir: PathBuf, depth: usize, agent: u8, jobs: &mut Vec<(PathBuf, u8, FileState, u64)>| {
        let mut files = Vec::new();
        jsonl_files(&dir, depth, &mut files);
        for p in files {
            let Ok(meta) = std::fs::metadata(&p) else { continue };
            let size = meta.len();
            let st = s.files.get(&*p.to_string_lossy()).cloned().unwrap_or_default();
            if size > st.offset {
                jobs.push((p, agent, st, size));
            }
        }
    };
    add(crate::claude_sessions::projects_dir(), 4, CLAUDE, &mut jobs);
    add(codex_dir(), 5, CODEX, &mut jobs);

    if !jobs.is_empty() {
        // Biggest first so the long ones don't end up alone at the tail.
        jobs.sort_by(|a, b| (b.3 - b.2.offset).cmp(&(a.3 - a.2.offset)));
        DONE_BYTES.store(0, Ordering::Relaxed);
        TOTAL_BYTES.store(jobs.iter().map(|j| j.3 - j.2.offset).sum(), Ordering::Relaxed);
        SCANNING.store(true, Ordering::Relaxed);
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<(PathBuf, Parsed)>> = Mutex::new(Vec::new());
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 4);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some((path, agent, st, _)) = jobs.get(i) else { break };
                        if let Some(p) = parse_file(path, *agent, st.clone()) {
                            results.lock().unwrap_or_else(|e| e.into_inner()).push((path.clone(), p));
                        }
                    }
                });
            }
        });
        SCANNING.store(false, Ordering::Relaxed);
        let mut results = results.into_inner().unwrap_or_else(|e| e.into_inner());
        // Deterministic dedupe winner: older files first.
        results.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, p) in results {
            merge(s, p);
        }
        if let Err(e) = save_store(s) {
            eprintln!("usage: saving cache failed: {e}");
        }
        release_freed_memory();
    }
    Arc::new(s.buckets.values().cloned().collect())
}

struct Usage {
    hash: Option<u64>,
    model: String,
    hour: i64,
    input: u64,
    cached: u64,
    output: u64,
}

struct Parsed {
    key: String,
    state: FileState,
    agent: u8,
    usage: Vec<Usage>,
    /// (model, hour) → (turns, active seconds).
    activity: HashMap<(String, i64), (u32, u32)>,
}

fn bucket<'a>(s: &'a mut Store, agent: u8, model: &str, session: &str, hour: i64) -> &'a mut Bucket {
    s.buckets.entry((agent, model.to_string(), session.to_string(), hour)).or_insert_with(|| Bucket {
        agent,
        model: model.to_string(),
        session: session.to_string(),
        hour,
        ..Default::default()
    })
}

fn merge(s: &mut Store, p: Parsed) {
    let session = p.state.session.clone();
    for ((model, hour), (turns, active)) in &p.activity {
        let b = bucket(s, p.agent, model, &session, *hour);
        b.turns += turns;
        b.active += active;
    }
    for u in p.usage {
        if let Some(h) = u.hash {
            if !s.seen.insert(h) {
                continue;
            }
        }
        let b = bucket(s, p.agent, &u.model, &session, u.hour);
        b.input += u.input;
        b.cached += u.cached;
        b.output += u.output;
    }
    s.files.insert(p.key, p.state);
}

fn hash_str(s: &str) -> u64 {
    // FNV-1a: stable across runs (the hashes are persisted).
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// "2026-11-24T02:52:19.083Z" → unix seconds.
pub fn parse_ts(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (n(0..4)?, n(5..7)?, n(8..10)?, n(11..13)?, n(14..16)?, n(17..19)?);
    let mut t = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se;
    // Explicit offset ("+08:00"); "Z" or nothing means UTC.
    if let Some(off) = s.get(19..).map(|r| r.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit())) {
        if off.len() >= 6 && (off.starts_with('+') || off.starts_with('-')) {
            let oh = off.get(1..3).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            let om = off.get(4..6).and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
            let sign = if off.starts_with('-') { -1 } else { 1 };
            t -= sign * (oh * 3600 + om * 60);
        }
    }
    Some(t)
}

/// Days since 1970-01-01 (proleptic Gregorian).
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// (year, month, day) of a day count since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

fn line_ts(line: &str) -> Option<i64> {
    const K: &str = "\"timestamp\":\"";
    let i = line.find(K)? + K.len();
    let rest = &line[i..];
    parse_ts(&rest[..rest.find('"')?])
}

fn parse_file(path: &Path, agent: u8, mut st: FileState) -> Option<Parsed> {
    let mut f = std::fs::File::open(path).ok()?;
    f.seek(SeekFrom::Start(st.offset)).ok()?;
    // Streamed line by line (transcripts reach ~100 MB; several threads read at once).
    let mut reader = std::io::BufReader::with_capacity(256 * 1024, f);
    let subagent = path.parent().and_then(|p| p.file_name()).is_some_and(|n| n == "subagents");
    if st.session.is_empty() {
        st.session = if subagent {
            path.parent().and_then(|p| p.parent()).and_then(|p| p.file_name())
        } else {
            path.file_stem()
        }
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    }
    let mut out = Parsed { key: path.to_string_lossy().into_owned(), state: FileState::default(), agent, usage: Vec::new(), activity: HashMap::new() };
    let mut pending_turns = 0u32;
    let mut end = 0u64;
    let mut raw: Vec<u8> = Vec::new();
    loop {
        raw.clear();
        // A huge line (pasted images) leaves a big buffer behind: don't keep it.
        if raw.capacity() > 4 << 20 {
            raw.shrink_to(1 << 20);
        }
        let n = match std::io::BufRead::read_until(&mut reader, b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        DONE_BYTES.fetch_add(n as u64, Ordering::Relaxed);
        // Only complete lines: the agent may be mid-write.
        if raw.last() != Some(&b'\n') {
            break;
        }
        end += n as u64;
        let Ok(line) = std::str::from_utf8(&raw[..n - 1]) else { continue };
        if line.is_empty() {
            continue;
        }
        let ts = line_ts(line);
        if agent == CLAUDE {
            if line.contains("\"type\":\"assistant\"") && line.contains("\"usage\"") {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    let m = &v["message"];
                    let model = m["model"].as_str().unwrap_or("");
                    if !model.is_empty() && model != "<synthetic>" {
                        st.model = model.to_string();
                        let u = &m["usage"];
                        let g = |k: &str| u[k].as_u64().unwrap_or(0);
                        let id = format!("{}:{}", m["id"].as_str().unwrap_or(""), v["requestId"].as_str().unwrap_or(""));
                        if let Some(ts) = ts {
                            out.usage.push(Usage {
                                hash: (id.len() > 1).then(|| hash_str(&id)),
                                model: model.to_string(),
                                hour: ts.div_euclid(3600),
                                input: g("input_tokens") + g("cache_creation_input_tokens") + g("cache_read_input_tokens"),
                                cached: g("cache_read_input_tokens"),
                                output: g("output_tokens"),
                            });
                        }
                    }
                }
            } else if !subagent && line.contains("\"type\":\"user\"") && !line.contains("\"tool_result\"") && !line.contains("\"isSidechain\":true") {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if v["isMeta"] != true {
                        if let Some(t) = crate::claude_sessions::text_of(&v["message"]["content"]) {
                            let t = t.trim();
                            if !t.is_empty() && !t.starts_with('<') && !t.starts_with("Caveat:") {
                                pending_turns += 1;
                            }
                        }
                    }
                }
            }
        } else if line.contains("\"type\":\"session_meta\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(id) = v["payload"]["id"].as_str() {
                    st.session = id.to_string();
                }
            }
        } else if line.contains("\"type\":\"turn_context\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(m) = v["payload"]["model"].as_str() {
                    st.model = m.to_string();
                }
            }
        } else if line.contains("\"type\":\"token_count\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let info = &v["payload"]["info"];
                let total = info["total_token_usage"]["total_tokens"].as_u64().unwrap_or(0);
                let last = &info["last_token_usage"];
                if total > 0 && total != st.last_total && last.is_object() {
                    st.last_total = total;
                    if let Some(ts) = ts {
                        let g = |k: &str| last[k].as_u64().unwrap_or(0);
                        out.usage.push(Usage {
                            hash: None,
                            model: if st.model.is_empty() { "codex".into() } else { st.model.clone() },
                            hour: ts.div_euclid(3600),
                            input: g("input_tokens"),
                            cached: g("cached_input_tokens"),
                            output: g("output_tokens"),
                        });
                    }
                }
            }
        } else if line.contains("\"type\":\"task_started\"") {
            pending_turns += 1;
        }
        if let Some(ts) = ts {
            let model = if st.model.is_empty() { if agent == CODEX { "codex".to_string() } else { String::new() } } else { st.model.clone() };
            let gap = ts - st.last_ts;
            let active = if st.last_ts > 0 && gap > 0 && gap <= IDLE_GAP { gap as u32 } else { 0 };
            if (active > 0 || pending_turns > 0) && !model.is_empty() {
                let e = out.activity.entry((model, ts.div_euclid(3600))).or_default();
                e.0 += pending_turns;
                e.1 += active;
                pending_turns = 0;
            }
            st.last_ts = st.last_ts.max(ts);
        }
    }
    if end == 0 {
        return None;
    }
    st.offset += end;
    out.state = st;
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Aggregation for the dashboard.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Range {
    Days7,
    Days30,
    Days90,
    All,
}

impl Range {
    pub fn days(self) -> Option<i64> {
        match self {
            Range::Days7 => Some(7),
            Range::Days30 => Some(30),
            Range::Days90 => Some(90),
            Range::All => None,
        }
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Totals {
    pub input: u64,
    pub cached: u64,
    pub output: u64,
    pub turns: u64,
    pub tasks: usize,
    pub active: u64,
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Day {
    pub input: u64,
    pub output: u64,
    pub tasks: usize,
    pub active: u64,
}

#[derive(Clone, Debug)]
pub struct ModelRow {
    pub name: String,
    pub input: u64,
    pub output: u64,
    pub tasks: usize,
}

#[derive(Clone, Debug)]
pub struct AgentRow {
    pub agent: u8,
    pub input: u64,
    pub output: u64,
    pub active: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Local day number (days since 1970-01-01) of the first column.
    pub start_day: i64,
    pub totals: Totals,
    /// Same-length period right before (none for "All").
    pub prev: Option<Totals>,
    pub max_concurrent: usize,
    pub daily: Vec<Day>,
    /// Active seconds by local weekday (Mon = 0) and hour.
    pub heat: [[u64; 24]; 7],
    pub models: Vec<ModelRow>,
    /// (provider, share of tokens), biggest first.
    pub providers: Vec<(&'static str, f32)>,
    /// Every agent with data in range (ignores the agent filter).
    pub agents: Vec<AgentRow>,
}

pub fn provider(model: &str) -> &'static str {
    let m = model.to_ascii_lowercase();
    if m.starts_with("claude") || m.contains("opus") || m.contains("sonnet") || m.contains("haiku") || m.contains("fable") {
        "Anthropic"
    } else if m.starts_with("gemini") {
        "Google"
    } else if m.starts_with("grok") {
        "xAI"
    } else if m.starts_with("gpt") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") || m.starts_with("codex") {
        "OpenAI"
    } else {
        "Other"
    }
}

/// `offset`: local UTC offset in seconds; `now`: unix seconds.
pub fn compute(buckets: &[Bucket], range: Range, agent: Option<u8>, now: i64, offset: i64) -> Stats {
    let local_hour = |h: i64| h + offset.div_euclid(3600);
    let day_of = |h: i64| local_hour(h).div_euclid(24);
    let today = (now + offset).div_euclid(86_400);
    let first = buckets.iter().map(|b| day_of(b.hour)).min().unwrap_or(today);
    let start_day = match range.days() {
        Some(n) => today - n + 1,
        None => first.min(today),
    };
    let len = (today - start_day + 1).max(1) as usize;

    let mut st = Stats { start_day, daily: vec![Day::default(); len], ..Default::default() };
    let mut sessions: HashSet<(u8, &str)> = HashSet::new();
    let mut day_sessions: Vec<HashSet<(u8, &str)>> = vec![HashSet::new(); len];
    let mut hour_sessions: HashMap<i64, HashSet<(u8, &str)>> = HashMap::new();
    let mut models: HashMap<&str, (u64, u64, HashSet<(u8, &str)>)> = HashMap::new();
    let mut agents: HashMap<u8, AgentRow> = HashMap::new();
    let mut prev = Totals::default();
    let mut prev_sessions: HashSet<(u8, &str)> = HashSet::new();
    let n_days = range.days();

    for b in buckets {
        let d = day_of(b.hour);
        if let Some(n) = n_days {
            if d < start_day && d >= start_day - n && agent.is_none_or(|a| a == b.agent) {
                prev.input += b.input;
                prev.cached += b.cached;
                prev.output += b.output;
                prev.turns += b.turns as u64;
                prev.active += b.active as u64;
                if b.input + b.output > 0 || b.active > 0 {
                    prev_sessions.insert((b.agent, &b.session));
                }
            }
        }
        if d < start_day || d > today {
            continue;
        }
        let a = agents.entry(b.agent).or_insert(AgentRow { agent: b.agent, input: 0, output: 0, active: 0 });
        a.input += b.input;
        a.output += b.output;
        a.active += b.active as u64;
        if agent.is_some_and(|a| a != b.agent) {
            continue;
        }
        let sid = (b.agent, b.session.as_str());
        let t = &mut st.totals;
        t.input += b.input;
        t.cached += b.cached;
        t.output += b.output;
        t.turns += b.turns as u64;
        t.active += b.active as u64;
        let day = &mut st.daily[(d - start_day) as usize];
        day.input += b.input;
        day.output += b.output;
        day.active += b.active as u64;
        if b.input + b.output > 0 || b.active > 0 {
            sessions.insert(sid);
            day_sessions[(d - start_day) as usize].insert(sid);
        }
        if b.active > 0 {
            hour_sessions.entry(b.hour).or_default().insert(sid);
            let lh = local_hour(b.hour);
            let wd = (lh.div_euclid(24) + 3).rem_euclid(7) as usize;
            st.heat[wd][lh.rem_euclid(24) as usize] += b.active as u64;
        }
        if b.input + b.output > 0 {
            let m = models.entry(b.model.as_str()).or_default();
            m.0 += b.input;
            m.1 += b.output;
            m.2.insert(sid);
        }
    }
    st.totals.tasks = sessions.len();
    for (i, s) in day_sessions.iter().enumerate() {
        st.daily[i].tasks = s.len();
    }
    st.max_concurrent = hour_sessions.values().map(|s| s.len()).max().unwrap_or(0);
    if n_days.is_some() {
        prev.tasks = prev_sessions.len();
        st.prev = Some(prev);
    }
    let mut rows: Vec<ModelRow> = models.into_iter().map(|(name, (i, o, s))| ModelRow { name: name.to_string(), input: i, output: o, tasks: s.len() }).collect();
    rows.sort_by(|a, b| (b.input + b.output).cmp(&(a.input + a.output)).then(a.name.cmp(&b.name)));
    let total: u64 = rows.iter().map(|r| r.input + r.output).sum();
    let mut prov: HashMap<&'static str, u64> = HashMap::new();
    for r in &rows {
        *prov.entry(provider(&r.name)).or_default() += r.input + r.output;
    }
    let mut providers: Vec<(&'static str, f32)> = prov.into_iter().map(|(p, v)| (p, v as f32 / total.max(1) as f32)).collect();
    providers.sort_by(|a, b| b.1.total_cmp(&a.1));
    st.providers = providers;
    st.models = rows;
    let mut agents: Vec<AgentRow> = agents.into_values().collect();
    agents.sort_by(|a, b| (b.input + b.output).cmp(&(a.input + a.output)));
    st.agents = agents;
    st
}

/// 11.6B, 61.3M, 22.9K, 950.
pub fn fmt_tokens(n: u64) -> String {
    let f = n as f64;
    let (v, s) = if f >= 1e9 {
        (f / 1e9, "B")
    } else if f >= 1e6 {
        (f / 1e6, "M")
    } else if f >= 1e3 {
        (f / 1e3, "K")
    } else {
        return n.to_string();
    };
    if v >= 100. { format!("{v:.0}{s}") } else { format!("{:.1}{s}", v).replace(".0", "") }
}

/// 1,110
pub fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "347 h", "9.2 h", "23 min".
pub fn fmt_duration(secs: u64) -> String {
    let h = secs as f64 / 3600.;
    if secs < 3600 {
        format!("{} min", (secs + 30) / 60)
    } else if h < 10. {
        format!("{h:.1} h")
    } else {
        format!("{} h", fmt_int(h.round() as u64))
    }
}

pub fn month_day(day: i64) -> String {
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (_, m, d) = civil_from_days(day);
    format!("{} {}", M[(m - 1) as usize], d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(parse_ts("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_ts("2026-11-24T02:52:19.083Z"), Some(1795488739));
        assert_eq!(parse_ts("2026-11-24T10:52:19+08:00"), Some(1795488739));
        assert_eq!(civil_from_days(days_from_civil(2026, 2, 28) + 1), (2026, 3, 1));
        assert_eq!(month_day(days_from_civil(2026, 10, 23)), "Oct 23");
    }

    #[test]
    fn formats() {
        assert_eq!(fmt_tokens(11_600_000_000), "11.6B");
        assert_eq!(fmt_tokens(61_300_000), "61.3M");
        assert_eq!(fmt_tokens(402_000_000), "402M");
        assert_eq!(fmt_tokens(5_000_000), "5M");
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_int(1110), "1,110");
        assert_eq!(fmt_int(100), "100");
        assert_eq!(fmt_duration(23 * 60), "23 min");
        assert_eq!(fmt_duration(347 * 3600), "347 h");
        assert_eq!(fmt_duration(33_120), "9.2 h");
    }

    fn write(name: &str, lines: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jt-usage-{}-{name}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("sess-1.jsonl");
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        p
    }

    #[test]
    fn parses_claude_transcript_incrementally() {
        let p = write("claude", &[
            r#"{"type":"user","message":{"role":"user","content":"arregla el login"},"timestamp":"2026-11-20T10:00:00.000Z"}"#,
            r#"{"type":"assistant","requestId":"r1","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":50}},"timestamp":"2026-11-20T10:00:10.000Z"}"#,
            r#"{"type":"assistant","requestId":"r1","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":10,"cache_creation_input_tokens":100,"cache_read_input_tokens":1000,"output_tokens":50}},"timestamp":"2026-11-20T10:00:11.000Z"}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"x"}]},"timestamp":"2026-11-20T10:00:20.000Z"}"#,
        ]);
        let mut s = Store::default();
        let parsed = parse_file(&p, CLAUDE, FileState::default()).unwrap();
        merge(&mut s, parsed);
        let b: Vec<&Bucket> = s.buckets.values().collect();
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].input, b[0].cached, b[0].output, b[0].turns, b[0].active), (1110, 1000, 50, 1, 20));
        assert_eq!(b[0].session, "sess-1");

        // Appended line continues from the saved offset; idle gap is not counted.
        let st = s.files[&*p.to_string_lossy()].clone();
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, br#"{"type":"assistant","requestId":"r2","message":{"id":"m2","model":"claude-opus-5-5","usage":{"input_tokens":1,"output_tokens":2}},"timestamp":"2026-11-20T11:00:00.000Z"}"#).unwrap();
        std::io::Write::write_all(&mut f, b"\n").unwrap();
        merge(&mut s, parse_file(&p, CLAUDE, st).unwrap());
        let total: u64 = s.buckets.values().map(|b| b.output).sum();
        let active: u32 = s.buckets.values().map(|b| b.active).sum();
        assert_eq!((total, active), (52, 20));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn parses_codex_rollout() {
        let p = write("codex", &[
            r#"{"timestamp":"2026-11-20T10:00:00.000Z","type":"session_meta","payload":{"id":"cx-1"}}"#,
            r#"{"timestamp":"2026-11-20T10:00:01.000Z","type":"turn_context","payload":{"model":"gpt-6.2"}}"#,
            r#"{"timestamp":"2026-11-20T10:00:02.000Z","type":"event_msg","payload":{"type":"task_started"}}"#,
            r#"{"timestamp":"2026-11-20T10:00:30.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":120},"last_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":20}}}}"#,
            r#"{"timestamp":"2026-11-20T10:00:31.000Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":120},"last_token_usage":{"input_tokens":100,"cached_input_tokens":80,"output_tokens":20}}}}"#,
        ]);
        let mut s = Store::default();
        merge(&mut s, parse_file(&p, CODEX, FileState::default()).unwrap());
        let b: Vec<&Bucket> = s.buckets.values().collect();
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].model.as_str(), b[0].session.as_str()), ("gpt-6.2", "cx-1"));
        assert_eq!((b[0].input, b[0].cached, b[0].output, b[0].turns, b[0].active), (100, 80, 20, 1, 31));
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }

    #[test]
    fn computes_ranges_and_filters() {
        let day = days_from_civil(2026, 11, 20);
        let mk = |agent, model: &str, session: &str, d: i64, h: i64, input, output, active| Bucket {
            agent,
            model: model.into(),
            session: session.into(),
            hour: d * 24 + h,
            input,
            cached: input / 2,
            output,
            turns: 1,
            active,
        };
        let buckets = vec![
            mk(CLAUDE, "claude-opus-5-5", "a", day, 10, 1000, 100, 600),
            mk(CODEX, "gpt-6.2", "b", day, 10, 500, 50, 300),
            mk(CLAUDE, "claude-opus-5-5", "c", day - 10, 9, 400, 40, 60),
        ];
        let now = day * 86_400 + 12 * 3600;
        let s = compute(&buckets, Range::Days7, None, now, 0);
        assert_eq!(s.daily.len(), 7);
        assert_eq!((s.totals.input, s.totals.output, s.totals.tasks, s.max_concurrent), (1500, 150, 2, 2));
        assert_eq!(s.prev.unwrap().input, 400);
        assert_eq!(s.heat[4][10], 900); // 2026-11-20 is a Friday
        assert_eq!(s.providers[0].0, "Anthropic");
        let only = compute(&buckets, Range::Days7, Some(CODEX), now, 0);
        assert_eq!((only.totals.input, only.agents.len()), (500, 2));
        let all = compute(&buckets, Range::All, None, now, 0);
        assert_eq!((all.daily.len(), all.totals.tasks), (11, 3));
    }

    /// Real data smoke test: `JT_USAGE_REAL=1 cargo test -p juan-terminal real_scan -- --nocapture`.
    #[test]
    fn real_scan() {
        if std::env::var_os("JT_USAGE_REAL").is_none() {
            return;
        }
        let t = std::time::Instant::now();
        let b = refresh();
        let first = t.elapsed();
        let t2 = std::time::Instant::now();
        let b2 = refresh();
        let (now, _) = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64, 0);
        let s = compute(&b2, Range::Days30, None, now, -5 * 3600);
        println!("buckets {} first {:?} second {:?}", b.len(), first, t2.elapsed());
        println!("30d in {} out {} cached {} tasks {} turns {} time {} conc {}", fmt_tokens(s.totals.input), fmt_tokens(s.totals.output), s.totals.cached, s.totals.tasks, s.totals.turns, fmt_duration(s.totals.active), s.max_concurrent);
        for m in &s.models { println!("  {} {} {} {}", m.name, fmt_tokens(m.input), fmt_tokens(m.output), m.tasks); }
        for a in &s.agents { println!("  agent {} {} {} {}", a.agent, fmt_tokens(a.input), fmt_tokens(a.output), fmt_duration(a.active)); }
    }
}
