//! Local HTML report from the text already read out of the screenshots:
//! overview stats, League results/rank snapshots, YouTube Studio numbers.
use crate::ocr::{self, cache_key};
use crate::settings;
use crate::sorter::{all_images, category_of, taken_at};
use chrono::{DateTime, Local};
use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

const PAGE: &str = include_str!("report_page.html");

fn ci(p: &str) -> Regex {
    RegexBuilder::new(p).case_insensitive(true).build().unwrap()
}
static RANK: Lazy<Regex> = Lazy::new(|| {
    ci(r"\b(Iron|Bronze|Silver|Gold|Platinum|Emerald|Diamond|Master|Grandmaster|Challenger)\s*(IV|III|II|I|[1-4])?\s*[|:]?\s*(\d{1,4}(?:,\d{3})?)\s*LP\b")
});
static LP_CHANGE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?:^|[^\w/])([+-])\s?(\d{1,2})\s?LP\b").unwrap());
static WIN: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(Victory|Win)\b").unwrap());
static LOSS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(Defeat|Loss)\b").unwrap());
static VIEWS: Lazy<Regex> = Lazy::new(|| ci(r"\bViews\s+([\d.,]+K?)"));
static WATCH: Lazy<Regex> = Lazy::new(|| ci(r"Watch time \(hours\)\s+([\d.,]+K?)"));
static SUBS: Lazy<Regex> = Lazy::new(|| ci(r"Subscribers\s+\+?([\d.,]+K?)"));
static IMPR: Lazy<Regex> = Lazy::new(|| ci(r"Impressions\s+([\d.,]+K?)"));
static TITLE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b([A-Z][A-Z0-9'!?,. ]{12,60})\b").unwrap());

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}
fn uri(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    format!("file:///{}", s.split('/').map(|seg| urlencoding::encode(seg).replace("%3A", ":")).collect::<Vec<_>>().join("/"))
}
fn link(p: &Path) -> String {
    format!(r#"<a href="{}">{}</a>"#, esc(&uri(p)), esc(&p.file_name().unwrap_or_default().to_string_lossy()))
}
fn first(rx: &Regex, t: &str) -> String {
    rx.captures(t).map(|c| c[1].to_string()).unwrap_or_default()
}

fn nice_max(v: f64) -> f64 {
    if v <= 5.0 {
        return 5.0;
    }
    let mag = 10f64.powi((v.log10()).floor() as i32);
    for m in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if v <= m * mag {
            return m * mag;
        }
    }
    v
}

/// Grouped columns (one or two series), recessive grid, hover tips.
fn column_chart(labels: &[String], series: &[Vec<f64>], names: &[&str]) -> String {
    let (n, k) = (labels.len().max(1), series.len());
    let (w, h, left, bottom, top) = (920.0, 220.0, 44.0, 28.0, 12.0);
    let vmax = nice_max(series.iter().flat_map(|s| s.iter().cloned()).fold(1.0, f64::max));
    let plot_h = h - bottom - top;
    let group = (w - left - 8.0) / n as f64;
    let bw = (28f64).min((group * 0.7 - 2.0 * (k as f64 - 1.0)) / k as f64);
    let mut out = format!(r#"<svg viewBox="0 0 {w} {h}" class="chart" role="img">"#);
    for i in 0..5 {
        let v = vmax * i as f64 / 4.0;
        let y = top + plot_h - plot_h * i as f64 / 4.0;
        out += &format!(r#"<line x1="{left}" x2="{}" y1="{y:.1}" y2="{y:.1}" class="grid"/><text x="{}" y="{:.1}" class="tick" text-anchor="end">{}</text>"#,
            w - 4.0, left - 6.0, y + 4.0, v);
    }
    for (gi, lab) in labels.iter().enumerate() {
        let gx = left + gi as f64 * group + (group - (bw * k as f64 + 2.0 * (k as f64 - 1.0))) / 2.0;
        for (si, s) in series.iter().enumerate() {
            let v = s[gi];
            let bh = plot_h * v / vmax;
            let x = gx + si as f64 * (bw + 2.0);
            let y = top + plot_h - bh;
            let r = 4f64.min(bw / 2.0).min(bh);
            let base = top + plot_h;
            let path = if bh > 0.0 {
                format!(r#"<path d="M{x:.1},{base:.1} V{:.1} Q{x:.1},{y:.1} {:.1},{y:.1} H{:.1} Q{:.1},{y:.1} {:.1},{:.1} V{base:.1} Z" class="s{}"/>"#,
                    y + r, x + r, x + bw - r, x + bw, x + bw, y + r, si + 1)
            } else { String::new() };
            out += &format!(r#"<g class="hit" data-tip="{} · {}: {}"><rect x="{:.1}" y="{top}" width="{:.1}" height="{plot_h}" fill="transparent"/>{path}</g>"#,
                esc(lab), names[si], v, x - 1.0, bw + 2.0);
        }
        if labels.len() <= 16 || gi % 2 == 0 {
            out += &format!(r#"<text x="{:.1}" y="{}" class="tick" text-anchor="middle">{}</text>"#, gx + bw * k as f64 / 2.0, h - 8.0, esc(lab));
        }
    }
    out + &format!(r#"<line x1="{left}" x2="{}" y1="{}" y2="{}" class="axis"/></svg>"#, w - 4.0, top + plot_h, top + plot_h)
}

/// Horizontal bars, one series, largest first.
fn bar_chart(items: &[(String, usize)]) -> String {
    let vmax = items.iter().map(|i| i.1).max().unwrap_or(1).max(1) as f64;
    let (row, left, width) = (26.0, 230.0, 920.0);
    let mut out = format!(r#"<svg viewBox="0 0 {width} {}" class="chart" role="img">"#, row * items.len() as f64 + 8.0);
    for (i, (lab, v)) in items.iter().enumerate() {
        let y = 4.0 + i as f64 * row;
        let bw = (width - left - 60.0) * *v as f64 / vmax;
        out += &format!(r#"<g class="hit" data-tip="{l}: {v}"><rect x="0" y="{y}" width="{width}" height="{row}" fill="transparent"/><text x="{}" y="{}" class="lab" text-anchor="end">{l}</text><rect x="{left}" y="{}" width="{:.1}" height="{}" rx="4" class="s1"/><text x="{:.1}" y="{}" class="val">{v}</text></g>"#,
            left - 8.0, y + 17.0, y + 5.0, bw.max(1.0), row - 10.0, left + bw + 6.0, y + 17.0, l = esc(lab));
    }
    out + "</svg>"
}

fn table(headers: &[&str], rows: &[Vec<String>], cls: &str) -> String {
    let head: String = headers.iter().map(|h| format!("<th>{}</th>", esc(h))).collect();
    let body: String = rows.iter().map(|r| format!("<tr>{}</tr>", r.iter().map(|c| format!("<td>{c}</td>")).collect::<String>())).collect();
    format!(r#"<div class="tablewrap"><table class="{cls}"><thead><tr>{head}</tr></thead><tbody>{body}</tbody></table></div>"#)
}

pub fn build(folder: &Path) -> std::io::Result<PathBuf> {
    let cache = ocr::load_cache();
    let mine: Vec<String> = settings::load().your_names.iter().map(|n| n.to_lowercase()).collect();
    let rows: Vec<(PathBuf, String, DateTime<Local>, String)> = all_images(folder)
        .into_iter()
        .map(|p| {
            let c = category_of(folder, &p).unwrap_or_else(|| "(unsorted)".into());
            let t = cache.get(&cache_key(&p)).cloned().unwrap_or_default();
            let d = taken_at(&p);
            (p, c, d, t)
        })
        .collect();
    let months: Vec<String> = rows.iter().map(|r| r.2.format("%Y-%m").to_string()).collect::<BTreeSet<_>>().into_iter().collect();
    let mut per_month: HashMap<String, usize> = HashMap::new();
    let mut per_cat: HashMap<String, usize> = HashMap::new();
    for r in &rows {
        *per_month.entry(r.2.format("%Y-%m").to_string()).or_default() += 1;
        *per_cat.entry(r.1.clone()).or_default() += 1;
    }
    let mut cats: Vec<(String, usize)> = per_cat.into_iter().collect();
    cats.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    // League
    let lg: Vec<&(PathBuf, String, DateTime<Local>, String)> = rows.iter().filter(|r| r.1.starts_with("League")).collect();
    let (mut wins, mut losses): (HashMap<String, usize>, HashMap<String, usize>) = Default::default();
    let (mut ups, mut downs) = (0, 0);
    let mut ranks: Vec<(DateTime<Local>, String, String, bool, PathBuf)> = Vec::new();
    for (p, _, d, t) in &lg {
        let m = d.format("%Y-%m").to_string();
        *wins.entry(m.clone()).or_default() += WIN.find_iter(t).count();
        *losses.entry(m).or_default() += LOSS.find_iter(t).count();
        for c in LP_CHANGE.captures_iter(t) {
            if &c[1] == "+" { ups += 1 } else { downs += 1 }
        }
        let mut seen = HashSet::new();
        let yours = mine.iter().any(|n| t.to_lowercase().contains(n));
        for c in RANK.captures_iter(t) {
            let tier = { let s = c[1].to_lowercase(); s[..1].to_uppercase() + &s[1..] };
            let div = c.get(2).map(|m| m.as_str()).unwrap_or("");
            let key = (tier.clone(), div.to_string(), c[3].to_string());
            if seen.insert(key) {
                ranks.push((*d, format!("{tier} {div}").trim().to_string(), c[3].replace(',', ""), yours, p.clone()));
            }
        }
    }
    ranks.sort_by(|a, b| b.0.cmp(&a.0));
    let lg_months: Vec<String> = months.iter().filter(|m| wins.get(*m).copied().unwrap_or(0) + losses.get(*m).copied().unwrap_or(0) > 0).cloned().collect();
    let (tw, tl): (usize, usize) = (wins.values().sum(), losses.values().sum());

    // YouTube Studio
    let mut yt: Vec<(DateTime<Local>, [String; 5], PathBuf)> = rows
        .iter()
        .filter(|r| r.1.starts_with("YouTube - Studio"))
        .filter_map(|(p, _, d, t)| {
            let v = [first(&VIEWS, t), first(&WATCH, t), first(&SUBS, t), first(&IMPR, t)];
            if v.iter().all(|x| x.is_empty()) { return None; }
            let title = first(&TITLE, t).trim().to_string();
            Some((*d, [v[0].clone(), v[1].clone(), v[2].clone(), v[3].clone(), title], p.clone()))
        })
        .collect();
    yt.sort_by(|a, b| b.0.cmp(&a.0));

    let pm = |m: &String| per_month.get(m).copied().unwrap_or(0) as f64;
    let busiest = months.iter().max_by_key(|m| per_month.get(*m).copied().unwrap_or(0)).cloned().unwrap_or("-".into());
    let mut s = String::new();
    s += &format!(r#"<section><h2>Overview</h2><div class="tiles">
      <div class="tile"><div class="big">{}</div><div class="muted">screenshots</div></div>
      <div class="tile"><div class="big">{}</div><div class="muted">categories</div></div>
      <div class="tile"><div class="big">{busiest}</div><div class="muted">busiest month</div></div></div>
      <h3>Screenshots per month</h3>{}<details><summary>Show table</summary>{}</details>
      <h3>Screenshots per category</h3>{}</section>"#,
        rows.len(), cats.len(),
        column_chart(&months, &[months.iter().map(pm).collect()], &["Screenshots"]),
        table(&["Month", "Screenshots"], &months.iter().map(|m| vec![m.clone(), (pm(m) as usize).to_string()]).collect::<Vec<_>>(), ""),
        bar_chart(&cats));
    let w = |m: &String| wins.get(m).copied().unwrap_or(0);
    let l = |m: &String| losses.get(m).copied().unwrap_or(0);
    s += &format!(r#"<section><h2>League of Legends</h2>
      <p class="muted">Counted from the text in {} League screenshots. If the same match history was screenshotted twice,
      its games are counted twice, and profiles of other players you looked at are included, so treat these as
      "what your screenshots show", not exact stats.</p>
      <div class="tiles"><div class="tile"><div class="big">{tw}</div><div class="muted">wins seen</div></div>
      <div class="tile"><div class="big">{tl}</div><div class="muted">losses seen</div></div>
      <div class="tile"><div class="big">{:.0}%</div><div class="muted">win share</div></div>
      <div class="tile"><div class="big">{ups} / {downs}</div><div class="muted">LP gains / losses seen</div></div></div>
      <h3>Wins and losses seen, per month</h3>
      <div class="legend"><span><i class="sw s1"></i>Wins</span><span><i class="sw s2"></i>Losses</span></div>{}
      <details><summary>Show table</summary>{}</details>
      <h3>Rank snapshots ({})</h3><label class="filter"><input type="checkbox" id="mineOnly"> Only screenshots that mention your name</label>{}</section>"#,
        lg.len(), if tw + tl > 0 { tw as f64 * 100.0 / (tw + tl) as f64 } else { 0.0 },
        column_chart(&lg_months, &[lg_months.iter().map(|m| w(m) as f64).collect(), lg_months.iter().map(|m| l(m) as f64).collect()], &["Wins", "Losses"]),
        table(&["Month", "Wins", "Losses"], &lg_months.iter().map(|m| vec![m.clone(), w(m).to_string(), l(m).to_string()]).collect::<Vec<_>>(), ""),
        ranks.len(),
        table(&["Date", "Rank", "LP", "Mentions your name", "Screenshot"],
            &ranks.iter().take(400).map(|r| vec![r.0.format("%Y-%m-%d %H:%M").to_string(), esc(&r.1), r.2.clone(), if r.3 { "yes".into() } else { String::new() }, link(&r.4)]).collect::<Vec<_>>(), "ranks"));
    s += &format!(r#"<section><h2>YouTube Studio</h2><p class="muted">Numbers read from {} YouTube Studio screenshots. Some show one
      video, some your whole channel, so compare like with like.</p>{}</section>"#,
        yt.len(),
        table(&["Date", "Views", "Watch time (h)", "Subscribers", "Impressions", "Title (if visible)", "Screenshot"],
            &yt.iter().map(|r| vec![r.0.format("%Y-%m-%d").to_string(), r.1[0].clone(), r.1[1].clone(), r.1[2].clone(), r.1[3].clone(), esc(&r.1[4]), link(&r.2)]).collect::<Vec<_>>(), ""));

    let dir = settings::app_file("reports");
    std::fs::create_dir_all(&dir)?;
    let out = dir.join("report.html");
    std::fs::write(&out, PAGE.replace("{{SECTIONS}}", &s).replace("{{WHEN}}", &Local::now().format("%Y-%m-%d %H:%M").to_string()).replace("{{FOLDER}}", &esc(&folder.to_string_lossy())))?;
    Ok(out)
}
