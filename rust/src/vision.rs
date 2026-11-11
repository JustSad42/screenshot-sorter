//! Image fingerprints (colours, layout, size, difference hash), used to
//! - guess the category of screenshots with no useful text by comparing them with sorted ones,
//! - find duplicates. Fingerprints are cached in look_v2.json next to the app.
use crate::ocr::{cache_key, Log};
use crate::rules::Rules;
use crate::settings;
use crate::sorter::{all_images, category_of};
use image::imageops::FilterType;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const VERSION: u32 = 2;

#[derive(Serialize, Deserialize, Clone)]
pub struct Fp {
    pub f: Vec<f32>,
    pub h: String,
    pub wh: [u32; 2],
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    v: u32,
    items: HashMap<String, Fp>,
}

fn store_file() -> PathBuf {
    settings::app_file("look_v2.json")
}

fn load() -> HashMap<String, Fp> {
    std::fs::read_to_string(store_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Store>(&s).ok())
        .filter(|s| s.v == VERSION)
        .map(|s| s.items)
        .unwrap_or_default()
}

fn save(items: HashMap<String, Fp>) {
    let s = Store { v: VERSION, items };
    let _ = settings::write_atomic(&store_file(), serde_json::to_string(&s).unwrap().as_bytes());
}

fn compute(p: &Path) -> Option<Fp> {
    let img = crate::imageio::open(p)?;
    let (w, h) = (img.width().max(1), img.height().max(1));
    let small = img.resize_exact(64, 64, FilterType::Triangle).to_rgb8();
    // colour histogram, 4 bins per channel
    let mut hist = [0f32; 64];
    for px in small.pixels() {
        let q = |v: u8| ((v as u32 * 4) / 256).min(3) as usize;
        hist[q(px[0]) * 16 + q(px[1]) * 4 + q(px[2])] += 1.0;
    }
    let total: f32 = hist.iter().sum();
    let grey = image::DynamicImage::ImageRgb8(small).to_luma8();
    // tiny greyscale layout
    let g8 = image::imageops::resize(&grey, 8, 8, FilterType::Triangle);
    let vals: Vec<f32> = g8.pixels().map(|p| p[0] as f32 / 255.0).collect();
    let mean = vals.iter().sum::<f32>() / vals.len() as f32;
    // difference hash for duplicates
    let d = image::imageops::resize(&grey, 9, 8, FilterType::Triangle);
    let mut bits: u64 = 0;
    for y in 0..8 {
        for x in 0..8 {
            bits = (bits << 1) | (d.get_pixel(x + 1, y)[0] > d.get_pixel(x, y)[0]) as u64;
        }
    }
    let mut f: Vec<f32> = hist.iter().map(|c| (c / total).sqrt() * 2.0).collect();
    f.extend(vals.iter().map(|v| v - mean));
    f.extend([(w as f32 / h as f32).ln(), (w as f32).ln() / 8.0, (h as f32).ln() / 8.0]);
    Some(Fp { f: f.iter().map(|v| (v * 10000.0).round() / 10000.0).collect(), h: format!("{bits:016x}"), wh: [w, h] })
}

/// Fingerprints for paths, computing only new ones (in parallel).
pub fn fingerprints(paths: &[PathBuf], log: Log) -> HashMap<PathBuf, Fp> {
    let mut items = load();
    let mut out = HashMap::new();
    let mut todo = Vec::new();
    for p in paths {
        let k = cache_key(p);
        match items.get(&k) {
            Some(fp) => {
                out.insert(p.clone(), fp.clone());
            }
            None => todo.push((p.clone(), k)),
        }
    }
    if !todo.is_empty() {
        log(&format!("Comparing how {} screenshot(s) look...", todo.len()));
        let new: Vec<(PathBuf, String, Option<Fp>)> = todo.into_par_iter().map(|(p, k)| { let fp = compute(&p); (p, k, fp) }).collect();
        for (p, k, fp) in new {
            if let Some(fp) = fp {
                items.insert(k, fp.clone());
                out.insert(p, fp);
            }
        }
        save(items);
    }
    out
}

pub struct LookModel {
    mu: Vec<f32>,
    sd: Vec<f32>,
    x: Vec<Vec<f32>>,
    y: Vec<String>,
}

impl LookModel {
    fn norm(&self, v: &[f32]) -> Vec<f32> {
        v.iter().enumerate().map(|(i, a)| (a - self.mu[i]) / self.sd[i]).collect()
    }
    /// k-nearest-neighbours vote -> (category, share of the weighted vote)
    pub fn predict(&self, raw: &[f32], exclude: Option<usize>) -> (String, f64) {
        let q = self.norm(raw);
        let mut d: Vec<(f32, usize)> = self
            .x
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != exclude)
            .map(|(i, r)| (r.iter().zip(&q).map(|(a, b)| (a - b) * (a - b)).sum::<f32>().sqrt(), i))
            .collect();
        let k = 7.min(d.len());
        d.select_nth_unstable_by(k.saturating_sub(1), |a, b| a.0.partial_cmp(&b.0).unwrap());
        let mut votes: HashMap<&str, f64> = HashMap::new();
        for (dist, i) in d.iter().take(k) {
            *votes.entry(self.y[*i].as_str()).or_default() += 1.0 / (*dist as f64 + 1e-3);
        }
        let total: f64 = votes.values().sum();
        let (lab, v) = votes.into_iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap();
        (lab.to_string(), v / total)
    }
}

pub fn build_model(folder: &Path, rules: &Rules, log: Log) -> Option<LookModel> {
    let names = rules.names();
    let labelled: Vec<(PathBuf, String)> = all_images(folder)
        .into_iter()
        .filter_map(|p| {
            let c = category_of(folder, &p)?;
            (c != rules.fallback && c != rules.no_text && names.contains(&c)).then_some((p, c))
        })
        .collect();
    if labelled.len() < 30 {
        return None;
    }
    let paths: Vec<PathBuf> = labelled.iter().map(|l| l.0.clone()).collect();
    let fps = fingerprints(&paths, log);
    let (mut x, mut y) = (Vec::new(), Vec::new());
    for (p, c) in labelled {
        if let Some(fp) = fps.get(&p) {
            x.push(fp.f.clone());
            y.push(c);
        }
    }
    let n = x.len() as f32;
    let dims = x.first()?.len();
    let mu: Vec<f32> = (0..dims).map(|j| x.iter().map(|r| r[j]).sum::<f32>() / n).collect();
    let sd: Vec<f32> = (0..dims).map(|j| (x.iter().map(|r| (r[j] - mu[j]).powi(2)).sum::<f32>() / n).sqrt() + 1e-6).collect();
    let mut m = LookModel { mu, sd, x: vec![], y };
    m.x = x.iter().map(|r| m.norm(r)).collect();
    Some(m)
}

pub const CONFIDENCE: f64 = 0.8; // only trust a guess when ~80% of the nearest neighbours agree

/// {path: (category, confidence)} for the paths it is confident about.
pub fn guess(folder: &Path, paths: &[PathBuf], rules: &Rules, log: Log) -> HashMap<PathBuf, (String, f64)> {
    let Some(model) = build_model(folder, rules, log) else { return HashMap::new() };
    let fps = fingerprints(paths, log);
    paths
        .iter()
        .filter_map(|p| {
            let (lab, conf) = model.predict(&fps.get(p)?.f, None);
            (conf >= CONFIDENCE).then(|| (p.clone(), (lab, conf)))
        })
        .collect()
}

/// Leave-one-out check on sorted screenshots: (right, wrong, unsure) at the given confidence.
pub fn self_test(folder: &Path, rules: &Rules, min_conf: f64) -> Option<(usize, usize, usize)> {
    let m = build_model(folder, rules, &|_| {})?;
    let raw: Vec<Vec<f32>> = m.x.iter().map(|r| r.iter().enumerate().map(|(i, v)| v * m.sd[i] + m.mu[i]).collect()).collect();
    let res: Vec<i8> = (0..m.y.len())
        .into_par_iter()
        .map(|i| {
            let (lab, conf) = m.predict(&raw[i], Some(i));
            if conf < min_conf { 0 } else if lab == m.y[i] { 1 } else { -1 }
        })
        .collect();
    Some((res.iter().filter(|&&r| r == 1).count(), res.iter().filter(|&&r| r == -1).count(), res.iter().filter(|&&r| r == 0).count()))
}

// ---------- duplicates ----------

/// Groups of images that look the same. Each group is best-first: biggest resolution,
/// then biggest file, then newest.
pub fn find_duplicates(folder: &Path, log: Log) -> Vec<Vec<PathBuf>> {
    let paths = all_images(folder);
    let fps = fingerprints(&paths, log);
    let items: Vec<(&PathBuf, &Fp, u64)> =
        paths.iter().filter_map(|p| fps.get(p).map(|f| (p, f, u64::from_str_radix(&f.h, 16).unwrap_or(0)))).collect();
    let mut parent: Vec<usize> = (0..items.len()).collect();
    fn find(p: &mut Vec<usize>, mut i: usize) -> usize {
        while p[i] != i {
            p[i] = p[p[i]];
            i = p[i];
        }
        i
    }
    // a close match shares at least one of the four 16-bit pieces of its hash
    let mut buckets: HashMap<(u8, u16), Vec<usize>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        for part in 0..4u8 {
            buckets.entry((part, (it.2 >> (part * 16)) as u16)).or_default().push(i);
        }
    }
    for idx in buckets.values() {
        if idx.len() > 400 {
            continue; // e.g. many plain dark screenshots - too vague to trust
        }
        for a in 0..idx.len() {
            for b in a + 1..idx.len() {
                let (i, j) = (idx[a], idx[b]);
                if items[i].1.wh == items[j].1.wh && (items[i].2 ^ items[j].2).count_ones() <= 4 {
                    let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                    parent[ri] = rj;
                }
            }
        }
    }
    let mut groups: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..items.len() {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(i);
    }
    let meta = |p: &Path| std::fs::metadata(p).map(|m| (m.len(), m.modified().ok())).unwrap_or((0, None));
    let mut out: Vec<Vec<PathBuf>> = groups
        .into_values()
        .filter(|g| g.len() > 1)
        .map(|mut g| {
            g.sort_by(|&a, &b| {
                let area = |i: usize| items[i].1.wh[0] as u64 * items[i].1.wh[1] as u64;
                let (ma, mb) = (meta(items[a].0), meta(items[b].0));
                area(b).cmp(&area(a)).then(mb.0.cmp(&ma.0)).then(mb.1.cmp(&ma.1))
            });
            g.into_iter().map(|i| items[i].0.clone()).collect()
        })
        .collect();
    out.sort_by_key(|g| std::cmp::Reverse(g.len()));
    out
}

/// After files are converted/renamed, keep their fingerprints under the new keys.
pub fn carry_keys(pairs: &[(String, String)]) {
    if pairs.is_empty() {
        return;
    }
    let mut items = load();
    for (old, new) in pairs {
        if let Some(fp) = items.get(old).cloned() {
            items.insert(new.clone(), fp);
        }
    }
    save(items);
}
