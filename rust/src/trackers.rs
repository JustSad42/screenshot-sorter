//! Numbers read out of League and YouTube Studio screenshots, for the Insights charts.
//! Only text that's already been read is used (nothing is downloaded or re-read).
use crate::ocr;
use crate::sorter::{all_images, category_of, taken_at};
use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;

fn ci(p: &str) -> Regex {
    RegexBuilder::new(p).case_insensitive(true).build().unwrap()
}
static RESULT: Lazy<Regex> = Lazy::new(|| ci(r"\b(victory|win|defeat|loss)\b"));
static KDA: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{1,2}) ?/ ?(\d{1,2}) ?/ ?(\d{1,2})\b").unwrap());
static LP_CHANGE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:^|[^\w/])([+-])\s?(\d{1,2})\s?LP\b").unwrap());
static RANK: Lazy<Regex> = Lazy::new(|| {
    ci(r"\b(Iron|Bronze|Silver|Gold|Platinum|Emerald|Diamond|Master|Grandmaster|Challenger)\s*(IV|III|II|I|[1-4])?\s*[|:]?\s*(\d{1,4}(?:,\d{3})?)\s*LP\b")
});
static VIEWS: Lazy<Regex> = Lazy::new(|| ci(r"\bViews\s+([\d.,]+\s?[KM]?)\b"));
static WATCH: Lazy<Regex> = Lazy::new(|| ci(r"Watch time \(hours\)\s+([\d.,]+\s?[KM]?)\b"));
static SUBS: Lazy<Regex> = Lazy::new(|| ci(r"Subscribers\s+\+?([\d.,]+\s?[KM]?)\b"));
static IMPR: Lazy<Regex> = Lazy::new(|| ci(r"Impressions\s+([\d.,]+\s?[KM]?)\b"));

const TIERS: [&str; 10] = ["iron", "bronze", "silver", "gold", "platinum", "emerald", "diamond", "master", "grandmaster", "challenger"];

/// Screenshots (with their text) in categories whose name starts with `prefix`.
fn texts_in(folders: &[PathBuf], prefix: &str) -> Vec<(PathBuf, String)> {
    let cache = ocr::load_cache();
    let pre = prefix.to_lowercase();
    let mut out = Vec::new();
    for root in folders {
        for p in all_images(root) {
            if category_of(root, &p).map(|c| c.to_lowercase().starts_with(&pre)).unwrap_or(false) {
                if let Some(t) = cache.get(&ocr::cache_key(&p)) {
                    out.push((p, t.clone()));
                }
            }
        }
    }
    out
}

fn when(p: &std::path::Path) -> String {
    taken_at(p).format("%Y-%m-%d %H:%M").to_string()
}

/// "12.3K" -> 12300, "1,204" -> 1204
fn number(s: &str) -> Option<f64> {
    let s = s.trim().replace(',', "");
    let (num, mul) = match s.chars().last() {
        Some('K') | Some('k') => (s[..s.len() - 1].trim().to_string(), 1e3),
        Some('M') | Some('m') => (s[..s.len() - 1].trim().to_string(), 1e6),
        _ => (s, 1.0),
    };
    num.parse::<f64>().ok().map(|v| v * mul)
}

/// Rank as one number on a ladder (Iron IV 0 LP = 0, each division 100), so it can be charted.
fn ladder(tier: usize, div: u32, lp: f64) -> f64 {
    if tier >= 7 {
        2800.0 + lp // Master and above: LP keeps counting
    } else {
        tier as f64 * 400.0 + (4 - div.clamp(1, 4)) as f64 * 100.0 + lp.min(100.0)
    }
}

/// One game from a screenshot: (won, KDA, LP change). Stats sites and the client show the
/// result word next to the KDA ("Win 31:20 ... 6/10/3", "25/12/6 ... Defeat (Blue Side)"),
/// and often both teams' words, so the word closest to the first KDA wins. Without a KDA
/// only a screen with a single result word counts.
fn game_of(t: &str) -> Option<(bool, Option<Vec<u32>>, Option<i64>)> {
    let words: Vec<(usize, bool)> = RESULT
        .captures_iter(t)
        .map(|c| (c.get(0).unwrap().start(), matches!(c[1].to_lowercase().as_str(), "victory" | "win")))
        .collect();
    let lp = LP_CHANGE.captures(t).and_then(|c| c[2].parse::<i64>().ok().map(|n| if &c[1] == "-" { -n } else { n }));
    let kda = KDA.captures_iter(t).find_map(|c| {
        let n: Vec<u32> = (1..=3).filter_map(|i| c[i].parse().ok()).collect();
        (n.len() == 3 && n.iter().all(|x| *x <= 60)).then(|| (c.get(0).unwrap().start(), n))
    });
    match kda {
        Some((at, n)) => {
            // closest word; one before the KDA beats one the same distance after
            let (pos, win) = words.iter().min_by_key(|(p, _)| if *p <= at { (at - p) * 2 } else { (p - at) * 2 + 1 })?;
            (pos.abs_diff(at) < 300).then_some((*win, Some(n), lp))
        }
        None => (words.len() == 1).then(|| (words[0].1, None, lp)),
    }
}

pub fn league(folders: &[PathBuf]) -> Value {
    let shots = texts_in(folders, "League");
    let mut games = Vec::new();
    let mut ranks = Vec::new();
    let mut seen = HashSet::new();
    for (p, t) in &shots {
        let d = when(p);
        if let Some((win, kda, lp)) = game_of(t) {
            // the same game screenshotted twice: same day + result + KDA (or within ten minutes)
            let key = if kda.is_some() { format!("{}|{}|{:?}", &d[..10], win, kda) } else { format!("{}|{}", &d[..d.len().min(15)], win) };
            if seen.insert(key) {
                games.push(json!({"d": d, "r": if win { "W" } else { "L" }, "kda": kda, "lp": lp, "p": p.to_string_lossy()}));
            }
        }
        if let Some(c) = RANK.captures(t) {
            let tier_name = c[1].to_lowercase();
            let tier = TIERS.iter().position(|x| *x == tier_name).unwrap_or(0);
            let div = match c.get(2).map(|m| m.as_str().to_uppercase()).as_deref() {
                Some("I") | Some("1") => 1,
                Some("II") | Some("2") => 2,
                Some("III") | Some("3") => 3,
                _ => 4,
            };
            let lp: f64 = c[3].replace(',', "").parse().unwrap_or(0.0);
            let label = if tier >= 7 { format!("{} {lp} LP", title(&tier_name)) } else { format!("{} {} {lp} LP", title(&tier_name), ["I", "II", "III", "IV"][div as usize - 1]) };
            ranks.push(json!({"d": d, "label": label, "score": ladder(tier, div, lp), "p": p.to_string_lossy()}));
        }
    }
    let by_date = |a: &Value, b: &Value| a["d"].as_str().cmp(&b["d"].as_str());
    games.sort_by(by_date);
    ranks.sort_by(by_date);
    let w = games.iter().filter(|g| g["r"] == "W").count();
    json!({"screens": shots.len(), "games": games, "ranks": ranks, "wins": w, "losses": games.len() - w})
}

fn title(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

pub fn youtube(folders: &[PathBuf]) -> Value {
    let shots = texts_in(folders, "YouTube - Studio");
    let mut points = Vec::new();
    for (p, t) in &shots {
        let get = |rx: &Regex| rx.captures(t).and_then(|c| number(&c[1]));
        let (views, watch, subs, impr) = (get(&VIEWS), get(&WATCH), get(&SUBS), get(&IMPR));
        if views.is_some() || watch.is_some() || subs.is_some() || impr.is_some() {
            points.push(json!({"d": when(p), "views": views, "watch": watch, "subs": subs, "impr": impr, "p": p.to_string_lossy()}));
        }
    }
    points.sort_by(|a, b| a["d"].as_str().cmp(&b["d"].as_str()));
    json!({"screens": shots.len(), "points": points})
}

#[cfg(test)]
mod tests {
    use super::game_of;
    #[test]
    fn games() {
        let w = game_of("48 minutes ago 2 Games / 18 R IWIL 1 Master / lhOm Ranked Solo 48 minutes ago Win 31:20 Fate Godlike Al Analysis (Blue Team) 6/10/3 0.90 KDA");
        assert_eq!(w, Some((true, Some(vec![6, 10, 3]), None)));
        let l = game_of("36:34 ago Solo/Duo 25/12/ 6 Bad team 2.6 KDA 7.7 CS/m 63% KP (9 General Defeat (Blue Side) 3 0 Details Victory (Red Side) Victory Defeat");
        assert_eq!(l.map(|g| (g.0, g.1)), Some((false, Some(vec![25, 12, 6]))));
        assert_eq!(game_of("VICTORY +18 LP").map(|g| (g.0, g.2)), Some((true, Some(18))));
        assert_eq!(game_of("SOLO/DUO DIAMOND IV 0 Wins 10 LP"), None);
        assert_eq!(game_of("Victory Defeat Victory"), None);
    }
    #[test]
    fn numbers() {
        assert_eq!(super::number("12.3K"), Some(12300.0));
        assert_eq!(super::number("1,204"), Some(1204.0));
        assert_eq!(super::ladder(7, 4, 120.0), 2920.0);
        assert_eq!(super::ladder(0, 4, 50.0), 50.0);
    }
}
