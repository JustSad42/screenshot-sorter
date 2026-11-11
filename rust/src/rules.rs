//! categories.json: each keyword is a regex string (1 point) or {"k": regex, "w": weight}.
//! A screenshot goes to the category with the most points, if it reaches min_score.
use crate::{ocr, settings};
use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

pub struct Category {
    pub name: String,
    pub keywords: Vec<(Regex, String, f64)>,
    pub min_score: f64,
}

pub struct Rules {
    pub fallback: String,
    pub no_text: String,
    pub min_chars: usize,
    pub cats: Vec<Category>,
}

pub fn keyword_parts(k: &Value) -> (String, f64) {
    match k {
        Value::Object(o) => (
            o.get("k").and_then(Value::as_str).unwrap_or("").to_string(),
            o.get("w").and_then(Value::as_f64).unwrap_or(1.0),
        ),
        Value::String(s) => (s.clone(), 1.0),
        _ => (String::new(), 1.0),
    }
}

impl Rules {
    pub fn from_json(d: &Value) -> Rules {
        let mut cats = Vec::new();
        for c in d["categories"].as_array().cloned().unwrap_or_default() {
            let mut kws = Vec::new();
            for k in c["keywords"].as_array().cloned().unwrap_or_default() {
                let (pat, w) = keyword_parts(&k);
                // a broken keyword is skipped rather than failing the whole sort
                if let Ok(rx) = RegexBuilder::new(&pat).case_insensitive(true).build() {
                    kws.push((rx, pat, w));
                }
            }
            let need = c.get("min_score").or_else(|| c.get("min_matches")).and_then(Value::as_f64).unwrap_or(1.0);
            cats.push(Category { name: c["name"].as_str().unwrap_or("").to_string(), keywords: kws, min_score: need });
        }
        Rules {
            fallback: d["fallback"].as_str().unwrap_or("Other").to_string(),
            no_text: d["no_text"].as_str().unwrap_or("No Text").to_string(),
            min_chars: d["min_text_chars"].as_u64().unwrap_or(15) as usize,
            cats,
        }
    }

    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.cats.iter().map(|c| c.name.clone()).collect();
        v.push(self.fallback.clone());
        v.push(self.no_text.clone());
        v
    }

    /// -> (category, matched keyword patterns strongest first, score)
    pub fn classify(&self, text: &str) -> (String, Vec<String>, f64) {
        if text.trim().chars().count() < self.min_chars {
            return (self.no_text.clone(), vec![], 0.0);
        }
        let (mut best, mut best_hits, mut best_score) = (self.fallback.clone(), vec![], 0.0);
        for c in &self.cats {
            let mut hits: Vec<(&str, f64)> =
                c.keywords.iter().filter(|(rx, _, _)| rx.is_match(text)).map(|(_, p, w)| (p.as_str(), *w)).collect();
            let score: f64 = hits.iter().map(|h| h.1).sum();
            if score >= c.min_score && score > best_score {
                hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap()); // stable: keeps order for equal weights
                best = c.name.clone();
                best_hits = hits.into_iter().map(|h| h.0.to_string()).collect();
                best_score = score;
            }
        }
        (best, best_hits, best_score)
    }
}

pub fn load_json() -> Value {
    settings::ensure_rules();
    std::fs::read_to_string(settings::rules_file())
        .ok()
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_else(|| serde_json::from_str(settings::DEFAULT_RULES).unwrap())
}

pub fn load() -> Rules {
    Rules::from_json(&load_json())
}

pub fn save_json(d: &Value) -> Result<(), String> {
    for c in d["categories"].as_array().cloned().unwrap_or_default() {
        for k in c["keywords"].as_array().cloned().unwrap_or_default() {
            let (p, _) = keyword_parts(&k);
            Regex::new(&p).map_err(|e| format!("Keyword “{p}” isn't valid: {e}"))?;
        }
    }
    settings::write_atomic(&settings::rules_file(), serde_json::to_string_pretty(d).unwrap().as_bytes()).map_err(|e| e.to_string())
}

/// Add plain words (escaped) to a category; creates the category if it's new.
pub fn add_keywords(category: &str, words: &[String], weight: f64) -> Result<(), String> {
    let mut d = load_json();
    let cats = d["categories"].as_array_mut().ok_or("bad categories.json")?;
    if !cats.iter().any(|c| c["name"] == category) {
        cats.push(json!({"name": category, "min_score": 2, "keywords": []}));
    }
    let cat = cats.iter_mut().find(|c| c["name"] == category).unwrap();
    let existing: HashSet<String> = cat["keywords"].as_array().unwrap().iter().map(|k| keyword_parts(k).0).collect();
    for w in words {
        let pat = format!(r"\b{}\b", regex::escape(w));
        if !existing.contains(&pat) {
            cat["keywords"].as_array_mut().unwrap().push(json!({"k": pat, "w": weight}));
        }
    }
    save_json(&d)
}

// ---------- readable keywords ----------

static RX_ESC: Lazy<Regex> = Lazy::new(|| Regex::new(r"\\([^\w\s])").unwrap());
static RX_CLASS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\\[bBdDsSwW](\{\d*,?\d*\}|[+*?])?").unwrap());
static RX_SET: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[[^\]]*\](\{\d*,?\d*\}|[+*?])?").unwrap());
static RX_GROUP: Lazy<Regex> = Lazy::new(|| Regex::new(r"\((?:\?:)?([^()|]*)(?:\|[^()]*)?\)\??").unwrap());
static RX_META: Lazy<Regex> = Lazy::new(|| Regex::new(r"[\^$?+*]|\{\d*,?\d*\}").unwrap());
static RX_WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());

/// r"\bKDA\b" -> "KDA", "solo/?duo" -> "solo/duo", "(good|bad) team" -> "good team"
pub fn pretty_kw(pattern: &str) -> String {
    let mut lits: Vec<String> = Vec::new();
    let s = RX_ESC.replace_all(pattern, |c: &regex::Captures| {
        lits.push(c[1].to_string());
        format!("\u{0}{}\u{0}", lits.len() - 1)
    });
    let s = RX_CLASS.replace_all(&s, " ");
    let s = RX_SET.replace_all(&s, "");
    let s = RX_GROUP.replace_all(&s, "$1");
    let s = s.split('|').next().unwrap_or("").to_string();
    let mut s = RX_META.replace_all(&s, "").to_string();
    for (i, l) in lits.iter().enumerate() {
        s = s.replace(&format!("\u{0}{i}\u{0}"), l);
    }
    RX_WS.replace_all(&s, " ").trim_matches(|c: char| " .:-/".contains(c)).to_string()
}

// ---------- "teach it" suggestions ----------

pub static STOP: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    "the and you for that this with are was have from your what just not but they all can".split(' ').collect()
});
static RX_WORD: Lazy<Regex> = Lazy::new(|| Regex::new(r"[A-Za-z][A-Za-z'\-]{3,}").unwrap());

/// Words that stand out in these texts compared with every other screenshot.
pub fn suggest_keywords(texts: &[String], category: &str, limit: usize) -> Vec<String> {
    let corpus = ocr::load_cache();
    let n = corpus.len().max(1) as f64;
    let mut df: HashMap<String, usize> = HashMap::new();
    for t in corpus.values() {
        let words: HashSet<String> = RX_WORD.find_iter(t).map(|m| m.as_str().to_lowercase()).collect();
        for w in words {
            *df.entry(w).or_default() += 1;
        }
    }
    let mut here: HashMap<String, usize> = HashMap::new();
    for t in texts {
        for m in RX_WORD.find_iter(t) {
            *here.entry(m.as_str().to_lowercase()).or_default() += 1;
        }
    }
    let rules = load();
    let kws = rules.cats.iter().find(|c| c.name == category).map(|c| &c.keywords);
    let mut scored: Vec<(f64, String)> = here
        .into_iter()
        .filter_map(|(w, tf)| {
            let d = *df.get(&w).unwrap_or(&0) as f64;
            if STOP.contains(w.as_str()) || d < 2.0 || d > n * 0.3 {
                return None;
            }
            if kws.map(|k| k.iter().any(|(rx, _, _)| rx.is_match(&w))).unwrap_or(false) {
                return None;
            }
            Some((tf as f64 * (n / d).ln(), w))
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap().then(b.1.cmp(&a.1)));
    scored.into_iter().take(limit).map(|s| s.1).collect()
}
