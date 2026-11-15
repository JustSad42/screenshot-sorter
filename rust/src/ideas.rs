//! Ideas for new categories: words that show up in many "Other" screenshots but rarely
//! in the sorted ones, with the words that tend to come with them.
use crate::rules::Rules;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

const STOP: &[&str] = &[
    "about", "above", "after", "again", "against", "also", "always", "another", "anyone", "anything", "around", "back", "because",
    "been", "before", "being", "below", "best", "better", "between", "both", "came", "come", "could", "didn", "does", "doesn",
    "doing", "done", "down", "during", "each", "even", "ever", "every", "from", "further", "gets", "give", "going", "gone",
    "good", "great", "have", "having", "hello", "here", "hers", "herself", "himself", "into", "just", "know", "last", "like",
    "made", "make", "many", "maybe", "more", "most", "much", "must", "myself", "need", "never", "next", "nothing", "only",
    "other", "ours", "over", "please", "really", "same", "said", "says", "should", "show", "since", "some", "something",
    "still", "such", "sure", "take", "than", "thank", "thanks", "that", "their", "theirs", "them", "then", "there", "these",
    "they", "thing", "things", "think", "this", "those", "though", "through", "time", "today", "under", "until", "very",
    "want", "was", "well", "went", "were", "what", "when", "where", "which", "while", "will", "with", "would", "yeah",
    "year", "years", "your", "yours", "yourself", "dont", "cant", "wont", "isnt", "youre", "thats", "whats", "lets", "okay",
    "more", "less", "view", "views", "click", "here", "page", "home", "menu", "settings", "search", "share", "save", "edit",
    "close", "open", "back", "next", "cancel", "done", "help", "info", "more", "show", "hide", "today", "yesterday",
    "january", "february", "march", "april", "june", "july", "august", "september", "october", "november", "december",
    "monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday", "http", "https", "www",
];

fn words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| (4..=20).contains(&w.len()) && w.chars().all(|c| c.is_ascii_alphabetic()) && !STOP.contains(w))
        .map(String::from)
        .collect()
}

/// other: (path, text) of screenshots in "Other"; rest: texts of sorted screenshots.
pub fn ideas(other: &[(PathBuf, String)], rest: &[String], rules: &Rules, max: usize) -> Vec<Value> {
    let n_o = other.len();
    if n_o < 20 {
        return vec![];
    }
    let n_r = rest.len().max(1);
    let sets: Vec<HashSet<String>> = other.iter().map(|(_, t)| words(t)).collect();
    let mut df_o: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, s) in sets.iter().enumerate() {
        for w in s {
            df_o.entry(w.as_str()).or_default().push(i);
        }
    }
    let mut df_r: HashMap<String, usize> = HashMap::new();
    for t in rest {
        for w in words(t) {
            *df_r.entry(w).or_default() += 1;
        }
    }
    let min_df = (n_o / 100).max(8);
    let known = |w: &str| rules.cats.iter().any(|c| c.keywords.iter().any(|(rx, _, _)| rx.is_match(w)));
    let mut cands: Vec<(&str, f64)> = df_o
        .iter()
        .filter(|(w, ix)| ix.len() >= min_df && !known(w))
        .filter_map(|(w, ix)| {
            let share_o = ix.len() as f64 / n_o as f64;
            let share_r = (df_r.get(*w).copied().unwrap_or(0) as f64 + 1.0) / (n_r as f64 + 1.0);
            (share_o >= 3.0 * share_r).then(|| (*w, share_o / share_r * (ix.len() as f64).ln()))
        })
        .collect();
    cands.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let mut picked: Vec<(String, HashSet<usize>, Vec<String>)> = Vec::new();
    for (w, _) in cands {
        if picked.len() >= max {
            break;
        }
        let set: HashSet<usize> = df_o[w].iter().copied().collect();
        // mostly the same screenshots as an idea we already have: it's a companion word
        if let Some(p) = picked.iter_mut().find(|p| p.1.intersection(&set).count() as f64 >= 0.6 * set.len().min(p.1.len()) as f64) {
            if p.2.len() < 3 && !p.2.iter().any(|x| x == w) {
                p.2.push(w.to_string());
            }
            continue;
        }
        picked.push((w.to_string(), set, vec![]));
    }
    // companions that weren't candidates themselves: frequent words inside each idea's screenshots
    for (w, set, also) in picked.iter_mut() {
        if also.len() >= 3 {
            continue;
        }
        let mut co: HashMap<&str, usize> = HashMap::new();
        for &i in set.iter() {
            for x in &sets[i] {
                if x != w && !known(x) {
                    *co.entry(x.as_str()).or_default() += 1;
                }
            }
        }
        let mut co: Vec<(&str, usize)> = co.into_iter().filter(|(_, n)| *n as f64 >= 0.5 * set.len() as f64).collect();
        co.sort_by(|a, b| b.1.cmp(&a.1));
        for (x, _) in co {
            if also.len() >= 3 {
                break;
            }
            if !also.iter().any(|a| a == x) {
                also.push(x.to_string());
            }
        }
    }
    picked
        .into_iter()
        .map(|(w, set, also)| {
            let mut ix: Vec<usize> = set.into_iter().collect();
            ix.sort();
            let samples: Vec<String> = ix.iter().rev().take(6).map(|&i| other[i].0.to_string_lossy().to_string()).collect();
            let mut name = w.clone();
            if let Some(f) = name.get_mut(..1) {
                f.make_ascii_uppercase();
            }
            json!({"word": w, "name": name, "count": ix.len(), "also": also, "samples": samples})
        })
        .collect()
}
