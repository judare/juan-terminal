//! Inline calculator and currency converter for the launcher, Raycast-style:
//! `5 * 3`, `(2+3)^2`, `sqrt(16)`, `20% of 150`, `1 usd to cop`, `100 eur en usd`.
//! Exchange rates come from open.er-api.com (USD base, no key), cached on disk for 6 hours.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct Answer {
    /// What gets copied (plain number, no grouping).
    pub value: String,
    /// What is shown big ("4,213.50 COP").
    pub display: String,
    /// The interpreted question ("1 USD → COP · rate of 2026-11-27").
    pub detail: String,
}

// ---- expression evaluator ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Num(f64),
    Ident(String),
    Op(char),
    LParen,
    RParen,
    Comma,
}

fn tokenize(s: &str) -> Option<Vec<Tok>> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch.is_ascii_digit() || (ch == '.' && c.get(i + 1).is_some_and(|d| d.is_ascii_digit())) {
            let mut n = String::new();
            while i < c.len() {
                let d = c[i];
                if d.is_ascii_digit() || d == '.' {
                    n.push(d);
                } else if d == '_' {
                } else if d == ',' && c.get(i + 1..i + 4).is_some_and(|g| g.iter().all(|x| x.is_ascii_digit())) && !c.get(i + 4).is_some_and(|x| x.is_ascii_digit()) {
                    // "1,000,000": thousands separator.
                } else {
                    break;
                }
                i += 1;
            }
            out.push(Tok::Num(n.parse().ok()?));
        } else if ch.is_alphabetic() || ch == '$' {
            let mut w = String::new();
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '$') {
                w.push(c[i]);
                i += 1;
            }
            out.push(Tok::Ident(w.to_lowercase()));
        } else {
            out.push(match ch {
                '+' | '-' | '*' | '/' | '^' | '%' => Tok::Op(ch),
                'x' | '×' => Tok::Op('*'),
                '÷' => Tok::Op('/'),
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                ',' => Tok::Comma,
                _ => return None,
            });
            i += 1;
        }
    }
    Some(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    /// Saw something beyond a bare number (operator or function): worth answering.
    nontrivial: bool,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    // expr := term (('+'|'-') term)*
    fn expr(&mut self) -> Option<f64> {
        let mut v = self.term()?;
        while let Some(Tok::Op(op @ ('+' | '-'))) = self.peek().cloned() {
            self.pos += 1;
            self.nontrivial = true;
            // "200 + 10%": percent of the left side.
            let start = self.pos;
            let r = self.term()?;
            let pct = self.pos == start + 2 && matches!(self.toks.get(start + 1), Some(Tok::Op('%')));
            let r = if pct { v * r } else { r };
            v = if op == '+' { v + r } else { v - r };
        }
        Some(v)
    }

    // term := factor (('*'|'/'|'mod'|'of'|implicit) factor)*
    fn term(&mut self) -> Option<f64> {
        let mut v = self.factor()?;
        loop {
            match self.peek().cloned() {
                Some(Tok::Op(op @ ('*' | '/'))) => {
                    self.pos += 1;
                    self.nontrivial = true;
                    let r = self.factor()?;
                    v = if op == '*' { v * r } else { v / r };
                }
                Some(Tok::Ident(w)) if w == "of" || w == "de" || w == "mod" => {
                    self.pos += 1;
                    self.nontrivial = true;
                    let r = self.factor()?;
                    v = if w == "mod" { v % r } else { v * r };
                }
                // Implicit multiplication: "2pi", "3(4+1)".
                Some(Tok::LParen) => {
                    self.nontrivial = true;
                    let r = self.factor()?;
                    v *= r;
                }
                Some(Tok::Ident(w)) if constant(&w).is_some() || function(&w).is_some() => {
                    self.nontrivial = true;
                    let r = self.factor()?;
                    v *= r;
                }
                _ => return Some(v),
            }
        }
    }

    // factor := '-' factor | power
    fn factor(&mut self) -> Option<f64> {
        match self.peek() {
            Some(Tok::Op('-')) => {
                self.pos += 1;
                Some(-self.factor()?)
            }
            Some(Tok::Op('+')) => {
                self.pos += 1;
                self.factor()
            }
            _ => self.power(),
        }
    }

    // power := postfix ('^' factor)?
    fn power(&mut self) -> Option<f64> {
        let base = self.postfix()?;
        if let Some(Tok::Op('^')) = self.peek() {
            self.pos += 1;
            self.nontrivial = true;
            let e = self.factor()?;
            return Some(base.powf(e));
        }
        Some(base)
    }

    // postfix := atom '%'*  ("15%" = 0.15)
    fn postfix(&mut self) -> Option<f64> {
        let mut v = self.atom()?;
        while let Some(Tok::Op('%')) = self.peek() {
            self.pos += 1;
            self.nontrivial = true;
            v /= 100.;
        }
        Some(v)
    }

    fn atom(&mut self) -> Option<f64> {
        match self.next()? {
            Tok::Num(n) => Some(n),
            Tok::LParen => {
                let v = self.expr()?;
                (self.next()? == Tok::RParen).then_some(v)
            }
            Tok::Ident(w) => {
                if let Some(c) = constant(&w) {
                    return Some(c);
                }
                let f = function(&w)?;
                self.nontrivial = true;
                let mut args = Vec::new();
                if self.peek() == Some(&Tok::LParen) {
                    self.pos += 1;
                    if self.peek() != Some(&Tok::RParen) {
                        args.push(self.expr()?);
                        while self.peek() == Some(&Tok::Comma) {
                            self.pos += 1;
                            args.push(self.expr()?);
                        }
                    }
                    (self.next()? == Tok::RParen).then_some(())?;
                } else {
                    // "sqrt 16"
                    args.push(self.power()?);
                }
                f(&args)
            }
            _ => None,
        }
    }
}

fn constant(w: &str) -> Option<f64> {
    match w {
        "pi" | "π" => Some(std::f64::consts::PI),
        "e" => Some(std::f64::consts::E),
        "tau" => Some(std::f64::consts::TAU),
        _ => None,
    }
}

type Func = fn(&[f64]) -> Option<f64>;

fn function(w: &str) -> Option<Func> {
    fn one(a: &[f64], f: fn(f64) -> f64) -> Option<f64> {
        (a.len() == 1).then(|| f(a[0]))
    }
    Some(match w {
        "sqrt" | "raiz" => |a| one(a, f64::sqrt),
        "cbrt" => |a| one(a, f64::cbrt),
        "abs" => |a| one(a, f64::abs),
        "ln" => |a| one(a, f64::ln),
        "log" => |a| match a {
            [x] => Some(x.log10()),
            [x, b] => Some(x.log(*b)),
            _ => None,
        },
        "exp" => |a| one(a, f64::exp),
        "sin" => |a| one(a, f64::sin),
        "cos" => |a| one(a, f64::cos),
        "tan" => |a| one(a, f64::tan),
        "asin" => |a| one(a, f64::asin),
        "acos" => |a| one(a, f64::acos),
        "atan" => |a| one(a, f64::atan),
        "floor" => |a| one(a, f64::floor),
        "ceil" => |a| one(a, f64::ceil),
        "round" => |a| match a {
            [x] => Some(x.round()),
            [x, d] => {
                let m = 10f64.powi(*d as i32);
                Some((x * m).round() / m)
            }
            _ => None,
        },
        "min" => |a| a.iter().copied().reduce(f64::min),
        "max" => |a| a.iter().copied().reduce(f64::max),
        _ => return None,
    })
}

/// Evaluate a math expression. `None` unless it parses fully and has at least one operation
/// (a bare "42" or a word isn't a calculation).
fn eval_math(s: &str) -> Option<f64> {
    let toks = tokenize(s)?;
    if toks.is_empty() {
        return None;
    }
    let mut p = Parser { toks, pos: 0, nontrivial: false };
    let v = p.expr()?;
    (p.pos == p.toks.len() && p.nontrivial && v.is_finite()).then_some(v)
}

/// Like [`eval_math`] but a bare number is fine (amounts in "100 usd to cop").
fn eval_amount(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return Some(1.);
    }
    let toks = tokenize(s)?;
    let mut p = Parser { toks, pos: 0, nontrivial: false };
    let v = p.expr()?;
    (p.pos == p.toks.len() && v.is_finite()).then_some(v)
}

// ---- currency ------------------------------------------------------------------------------

struct Rates {
    /// Units per 1 USD.
    rates: HashMap<String, f64>,
    /// Unix seconds of the provider's last update.
    updated: u64,
    fetched: u64,
}

static RATES: Mutex<Option<Rates>> = Mutex::new(None);
const MAX_AGE: u64 = 6 * 3600;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn cache_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Application Support/juan-terminal/fx-rates.json")
}

fn parse_rates(body: &str, fetched: u64) -> Option<Rates> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let rates: HashMap<String, f64> = v.get("rates")?.as_object()?.iter().filter_map(|(k, v)| Some((k.to_uppercase(), v.as_f64()?))).collect();
    let updated = v.get("time_last_update_unix").and_then(|t| t.as_u64()).unwrap_or(fetched);
    let fetched = v.get("fetched").and_then(|t| t.as_u64()).unwrap_or(fetched);
    (!rates.is_empty()).then_some(Rates { rates, updated, fetched })
}

/// Load cached rates and refresh them if stale. Blocking: call off the main thread.
/// True when rates changed (the caller should re-run the query).
pub fn refresh_rates() -> bool {
    let mut changed = false;
    if RATES.lock().unwrap().is_none() {
        if let Some(r) = std::fs::read_to_string(cache_path()).ok().and_then(|b| parse_rates(&b, 0)) {
            *RATES.lock().unwrap() = Some(r);
            changed = true;
        }
    }
    let stale = RATES.lock().unwrap().as_ref().is_none_or(|r| now().saturating_sub(r.fetched) > MAX_AGE);
    if !stale {
        return changed;
    }
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(8))).build().into();
    let Ok(resp) = agent.get("https://open.er-api.com/v6/latest/USD").call() else { return changed };
    let Ok(body) = resp.into_body().read_to_string() else { return changed };
    let t = now();
    let Some(r) = parse_rates(&body, t) else { return changed };
    // Remember when we fetched so a restart doesn't refetch.
    if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&body) {
        v["fetched"] = serde_json::json!(t);
        let path = cache_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(path, v.to_string());
    }
    *RATES.lock().unwrap() = Some(r);
    true
}

fn currency_code(w: &str) -> Option<String> {
    let w = w.trim().to_lowercase();
    Some(String::from(match w.as_str() {
        "$" | "usd" | "us$" | "dolar" | "dolares" | "dólar" | "dólares" | "dollar" | "dollars" | "usd$" => "USD",
        "cop" | "peso" | "pesos" | "col$" | "cop$" => "COP",
        "eur" | "€" | "euro" | "euros" => "EUR",
        "gbp" | "£" | "libra" | "libras" | "pound" | "pounds" => "GBP",
        "mxn" => "MXN",
        "brl" | "real" | "reales" => "BRL",
        "ars" => "ARS",
        "clp" => "CLP",
        "pen" | "sol" | "soles" => "PEN",
        "jpy" | "¥" | "yen" | "yenes" => "JPY",
        "cad" => "CAD",
        "aud" => "AUD",
        "chf" => "CHF",
        "cny" | "yuan" | "rmb" => "CNY",
        "btc" => return None,
        _ => {
            // Any 3-letter ISO code the provider knows.
            if w.len() == 3 && w.chars().all(|c| c.is_ascii_alphabetic()) {
                let up = w.to_uppercase();
                let known = RATES.lock().unwrap().as_ref().is_some_and(|r| r.rates.contains_key(&up));
                if known {
                    return Some(up);
                }
            }
            return None;
        }
    }))
}

/// "1 usd to cop", "100usd en cop", "$50 a cop", "20 eur" (→ USD, or COP for USD).
fn eval_currency(q: &str) -> Option<Answer> {
    let lower = q.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    // Split "<amount+from> <to|in|a|en|=|->|→> <to>".
    let sep = words.iter().rposition(|w| matches!(*w, "to" | "in" | "a" | "en" | "=" | "->" | "→" | "=>"));
    let (left, to) = match sep {
        Some(i) if i + 1 == words.len() => return None,
        Some(i) => (words[..i].join(" "), Some(words[i + 1..].join(" "))),
        None => (lower.clone(), None),
    };
    let to = match &to {
        Some(t) => Some(currency_code(t)?),
        None => None,
    };
    // The source currency is a trailing word ("100 usd", "100usd") or a leading symbol ("$100").
    let left = left.trim();
    let (amount_s, from) = {
        let tail_start = left.rfind(|c: char| !(c.is_alphabetic() || c == '$' || c == '€' || c == '£' || c == '¥')).map(|i| i + left[i..].chars().next().unwrap().len_utf8()).unwrap_or(0);
        let tail = &left[tail_start..];
        if let Some(code) = (!tail.is_empty()).then(|| currency_code(tail)).flatten() {
            (left[..tail_start].trim().to_string(), code)
        } else {
            let head: String = left.chars().take_while(|c| matches!(c, '$' | '€' | '£' | '¥')).collect();
            if head.is_empty() {
                return None;
            }
            (left[head.len()..].trim().to_string(), currency_code(&head)?)
        }
    };
    // "all", "top", "try" are ISO codes too: without "to X" demand an explicit amount.
    if amount_s.is_empty() && to.is_none() {
        return None;
    }
    let amount = eval_amount(&amount_s)?;
    let to = to.unwrap_or_else(|| if from == "USD" { "COP".into() } else { "USD".into() });
    if to == from {
        return None;
    }
    let guard = RATES.lock().unwrap();
    let r = guard.as_ref()?;
    let (rf, rt) = (r.rates.get(&from)?, r.rates.get(&to)?);
    let v = amount / rf * rt;
    let rate = rt / rf;
    let date = chrono::DateTime::from_timestamp(r.updated as i64, 0).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default();
    Some(Answer {
        value: plain(v, if v.abs() >= 100. { 2 } else { 4 }),
        display: format!("{} {to}", pretty(v, if v.abs() >= 100. { 2 } else { 4 })),
        detail: format!("{} {from} → {to} · 1 {from} = {} {to} · {date}", pretty(amount, 4), pretty(rate, if rate >= 100. { 2 } else { 6 })),
    })
}

// ---- formatting ----------------------------------------------------------------------------

fn plain(v: f64, max_dec: usize) -> String {
    let s = format!("{v:.max_dec$}");
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    if s == "-0" { "0".into() } else { s }
}

/// Thousands separators and trimmed decimals: 4213.5 → "4,213.5".
fn pretty(v: f64, max_dec: usize) -> String {
    if v.abs() >= 1e15 || (v != 0. && v.abs() < 1e-6) {
        return format!("{v:e}");
    }
    let s = plain(v, max_dec);
    let (sign, s) = s.strip_prefix('-').map(|r| ("-", r)).unwrap_or(("", s.as_str()));
    let (int, frac) = s.split_once('.').map(|(a, b)| (a, Some(b))).unwrap_or((s, None));
    let mut grouped = String::new();
    for (i, ch) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    match frac {
        Some(f) => format!("{sign}{grouped}.{f}"),
        None => format!("{sign}{grouped}"),
    }
}

/// Answer a launcher query, if it's a calculation or a currency conversion.
pub fn answer(q: &str) -> Option<Answer> {
    let q = q.trim().trim_start_matches('=').trim();
    if q.is_empty() {
        return None;
    }
    if let Some(a) = eval_currency(q) {
        return Some(a);
    }
    let v = eval_math(q)?;
    Some(Answer { value: plain(v, 10), display: pretty(v, 10), detail: q.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calc(s: &str) -> Option<String> {
        answer(s).map(|a| a.value)
    }

    #[test]
    fn math() {
        assert_eq!(calc("5 * 3").as_deref(), Some("15"));
        assert_eq!(calc("5*3").as_deref(), Some("15"));
        assert_eq!(calc("(2+3)^2").as_deref(), Some("25"));
        assert_eq!(calc("2^3^2").as_deref(), Some("512"));
        assert_eq!(calc("-3 + 10 / 4").as_deref(), Some("-0.5"));
        assert_eq!(calc("sqrt(16) + 1").as_deref(), Some("5"));
        assert_eq!(calc("20% of 150").as_deref(), Some("30"));
        assert_eq!(calc("200 + 10%").as_deref(), Some("220"));
        assert_eq!(calc("1,000 * 3").as_deref(), Some("3000"));
        assert_eq!(calc("2pi").as_deref(), Some("6.2831853072"));
        assert_eq!(calc("10 mod 3").as_deref(), Some("1"));
        assert_eq!(calc("0.1 + 0.2").as_deref(), Some("0.3"));
    }

    #[test]
    fn not_math() {
        assert!(calc("42").is_none());
        assert!(calc("bemovil").is_none());
        assert!(calc("ssh bemo").is_none());
        assert!(calc("e").is_none());
        assert!(calc("all").is_none());
        assert!(calc("main.rs").is_none());
        assert!(calc("1/0").is_none());
    }

    #[test]
    fn currency() {
        *RATES.lock().unwrap() = Some(Rates { rates: [("USD".into(), 1.), ("COP".into(), 4000.), ("EUR".into(), 0.5)].into_iter().collect(), updated: 0, fetched: now() });
        assert_eq!(calc("1 usd to cop").as_deref(), Some("4000"));
        assert_eq!(calc("1usd in cop").as_deref(), Some("4000"));
        assert_eq!(calc("$2 a cop").as_deref(), Some("8000"));
        assert_eq!(calc("8000 cop en usd").as_deref(), Some("2"));
        assert_eq!(calc("10 eur").as_deref(), Some("20"));
        assert_eq!(calc("2*5 usd to eur").as_deref(), Some("5"));
        assert_eq!(answer("1 usd to cop").unwrap().display, "4,000 COP");
        assert!(calc("usd to xyz").is_none());
    }

    #[test]
    fn grouping() {
        assert_eq!(pretty(4213.5, 2), "4,213.5");
        assert_eq!(pretty(-1234567., 2), "-1,234,567");
        assert_eq!(pretty(12., 2), "12");
    }
}
