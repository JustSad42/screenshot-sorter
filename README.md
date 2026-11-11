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
- **Background:** auto-sort, either instantly or every few hours, from the tray icon or hidden.
  Explorer also gets a "Sort with Screenshot Sorter" right-click menu.

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
| `rust/src/background.rs` | Background auto-sort |
| `rust/src/integration.rs` | Start at login and the Explorer menu (per-user registry) |
| `rust/src/updater.rs` | Update check and install from GitHub releases |
| `installer/setup.iss` | Inno Setup installer |

Command line: `--apply [--folder PATH]`, `--undo`, `--search TEXT`, `--compress avif|webp`,
`--check-update`, `--update`, `--version`. Without `--apply`, a sort is a dry run.

## License

MIT
