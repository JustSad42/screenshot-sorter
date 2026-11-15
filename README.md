# Screenshot Sorter

A Windows app that sorts a screenshots folder into category folders by the text inside each
screenshot. Text is read offline with Windows' built-in OCR (English and Arabic). Screenshots
without text are matched by how they look, and Claude can optionally handle any leftovers.

Written in Rust with [Tauri](https://tauri.app). It starts in about a tenth of a second and uses
10-15 MB in the background.

## Install

Download `ScreenshotSorter-x.y.z-setup.exe` from the
[latest release](https://github.com/JustSad42/screenshot-sorter/releases/latest) and run it.
It installs for your user only, so no admin prompt.

The app checks this repo for new releases once a day. When there is one, it shows
**Update available**, and **Update now** installs it and reopens the app. In Settings → Updates
you can check by hand or turn the daily check off.

There is also a `-portable.exe` in each release. Put it in any folder and it keeps its data next
to itself. The portable copy tells you about updates but doesn't install them.

Needs Windows 10 (1809) or 11 with WebView2, which Windows 11 already has. To open AVIF and
JPEG XL screenshots, install the free AV1 and JPEG XL extensions from the Microsoft Store.

## What it does

- **Sort:** preview where each screenshot would go, then move them. Drag a card onto another
  category to fix it, and "Teach it" turns that fix into new keywords.
- **Categories:** weighted keyword rules (`categories.json`) you can edit in the app.
- **Library:** browse everything already sorted. You can re-file screenshots or give them
  readable names, and any move can be undone.
- **Search:** search the text inside every screenshot.
- **Clean up:** find duplicates, or shrink screenshots.
  - AVIF is about 80% smaller. The originals are kept in a backup folder.
  - Lossless WebP keeps every pixel the same.
- **Insights:** counts by category and month, plus an HTML report with extra League of Legends
  and YouTube Studio numbers.
- **Several folders:** watch as many folders as you like. Each one is sorted the moment a
  screenshot lands (using Windows change notifications) or every few hours.
- **Activity:** a live log of every move, whether by auto-sort or by you. It shows where each
  file went and why, has filters, and lets you undo any single move. A file you put back stays
  put.
- **One app:** the window, tray icon and auto-sort run as a single process. Opening it again
  brings the window forward, and closing the window keeps auto-sort running.
- **Fits any screen:** the window opens at a size that fits your screen and remembers its size
  and position. The layout adapts to narrow windows, and you can zoom with Ctrl + / Ctrl − / Ctrl 0.
- **Review:** screenshots that didn't clearly fit (Other, No Text, and close calls that were
  filed anyway) wait on one page. Press 1-9 to file one. Each choice can add a couple of
  telling words to that category, so the rules learn.
- **Confidence:** every move is marked strong or weak. Weak ones still move but show a
  "Check" badge and appear under Review > Worth a second look.
- **Clues from app names:** Game Bar style file names ("Discord 2026-...png") and PNG text
  chunks count as extra text.
- **Clearer reasons:** "no readable text", or "no category keywords matched · top words: ...".
- **Undo a whole sort:** Activity groups each sort into one row with "Undo all".
- **Problems with fixes:** errors explain what went wrong and offer a fix (Start OneDrive, Try again).
- **Library across folders, Timeline:** browse one folder or all of them, as a grid or by
  month and day with a month rail.
- **Drag out:** drag pictures from the app into Discord, a browser or Explorer.
- **Smart albums:** save a search in the sidebar. It updates itself.
- **Summary notification:** a daily or weekly evening summary (needs the tray icon).
- **Auto-shrink:** optionally shrink screenshots to AVIF after 7, 30 or 90 days (off by default).
- **League and YouTube trackers:** Insights reads match results, KDA and rank (League) or views,
  watch time and subscribers (YouTube Studio) from your screenshots and charts them.
- **First-run setup:** pick folders and see exactly what would move before anything does.
  Adding a folder later shows the same preview.
- **Local AI (optional):** a vision model in [Ollama](https://ollama.com) on your PC (for example
  `gemma3` or `gemma4`) suggests a folder for screenshots the rules couldn't place. Nothing leaves
  your computer. It only runs when you sort by hand (Sort page, Review page; press A to accept
  its pick), never during auto-sort. Turn it on in Settings > Local AI.
- **Explorer menu:** a "Sort with Screenshot Sorter" right-click menu on folders.

Auto-sort is off on a new install until you turn it on in Settings.

## Privacy

- Everything runs on your PC.
- The only network calls are:
  - the daily update check to the GitHub API
  - the optional Claude step, which sends only leftover screenshots, downscaled, to Anthropic's
    API, using your own API key

## Build

```powershell
cd rust
cargo build --release          # rust\target\release\screenshot-sorter.exe
```

The installer needs [Inno Setup 6](https://jrsoftware.org/isinfo.php):

```powershell
.\release.ps1                  # builds the exe and dist\ScreenshotSorter-<version>-setup.exe
.\release.ps1 -Publish         # also creates the GitHub release v<version> (needs gh)
```

To ship an update:
1. Bump `version` in `rust/Cargo.toml` and `rust/tauri.conf.json`.
2. Run `.\release.ps1 -Publish -Notes "what changed"`.

Installed copies pick up the new release by themselves.

## Code

| File | What it does |
|---|---|
| `rust/src/main.rs` | Entry point: window, `--tray`, `--setup`, command line |
| `rust/src/gui.rs` + `rust/ui/index.html` | App window, its bridge to Rust, thumbnails, tray icon |
| `rust/src/sorter.rs` | Planning, moving, renaming, undo logs, search, insights |
| `rust/src/ocr.rs` | Windows OCR in parallel, and the text cache |
| `rust/src/rules.rs` | Weighted keyword rules and "teach it" suggestions |
| `rust/src/vision.rs` | Image fingerprints for look-based guesses and duplicates |
| `rust/src/imageio.rs` | Image decoding (with Windows codecs for AVIF/JXL/HEIC) and AVIF encoding |
| `rust/src/tools.rs` | Recycle Bin, AVIF/WebP conversion, thumbnails |
| `rust/src/reports.rs` | HTML report |
| `rust/src/claude.rs` | Optional Claude step for leftovers (off by default) |
| `rust/src/background.rs` | Background auto-sort for several folders (change notifications + interval sweeps) |
| `rust/src/activity.rs` | Activity log (`logs/activity.jsonl`) |
| `rust/src/integration.rs` | Start at login and the Explorer menu (per-user registry) |
| `rust/src/updater.rs` | Update check and install from GitHub releases |
| `installer/setup.iss` | Inno Setup installer |

Command line: `--apply [--folder PATH]`, `--undo`, `--search TEXT`, `--compress avif|webp`,
`--check-update`, `--update`, `--version`. Without `--apply`, a sort is a dry run.

## License

MIT
