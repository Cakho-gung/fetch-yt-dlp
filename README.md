# FETCH — Mini Downloader

A small cross-platform (macOS + Windows) desktop downloader built with **Tauri
2 + Rust**, implementing the *Mini Downloader UI* design (the monochrome
"Industry" wireframe system). It wraps [`yt-dlp`](https://github.com/yt-dlp/yt-dlp)
(with `ffmpeg` for merging/extraction) behind a single-window UI.

## What works

- **Paste → Analyze** — detects the source and reads metadata (title, channel,
  duration, views, thumbnail) and real per-quality file sizes via `yt-dlp -J`.
- **Video / Audio** — BEST / 1080p / 720p / 480p video tiers (muxed to MP4) or
  MP3 / M4A audio, with optional embedded subtitles, thumbnail and metadata.
- **Live download** — streams real progress (percent, speed, ETA) and shows the
  FFmpeg merge/extract stage; cancellable.
- **Playlist** — pick individual entries (maps to `yt-dlp --playlist-items`).
- **Mix / radio** — endless auto-lists: grab this video only (`--no-playlist`)
  or cap at the first N.
- **History** with reveal-in-Finder/Explorer, and a light/dark theme toggle.

The full state flow (empty → analyzed → downloading → success, plus playlist,
mix and dark) mirrors the seven states in `Mini Downloader UI.dc.html`.

## Requirements

The app shells out to two external tools: **yt-dlp** and **ffmpeg**.

- **In-app install** — if either is missing, FETCH shows a banner with an
  **INSTALL** button. Accepting it runs the platform package manager (Homebrew
  on macOS, winget on Windows) with a live log, then re-checks and continues.
  This needs a package manager present (`brew` / `winget`).
- **Manual** — or install them yourself once:

  ```sh
  # macOS
  brew install yt-dlp ffmpeg
  # Windows
  winget install yt-dlp.yt-dlp Gyan.FFmpeg
  ```

The Rust side probes common install dirs, so a Homebrew install is found even
when the app is launched from Finder with a bare `PATH`.

## Develop

```sh
npm install
npm run tauri dev      # native app with hot-reload
npm run tauri build    # production bundle (.app / .dmg / .msi)
```

`npm run dev` alone serves just the web UI at http://localhost:1420 — useful for
visual work, though the Rust commands only run inside the Tauri shell.

## Layout

| Path | Role |
| --- | --- |
| `index.html` | window shell (blueprint frame + header) |
| `src/styles.css` | ported "Industry" design system, monochrome variant |
| `src/main.ts` | UI state machine, `invoke` calls, live progress events |
| `src-tauri/src/lib.rs` | `analyze`, `download`, `cancel_download`, `check_binaries`, `reveal_in_folder` |

## Notes / next steps

- Sizes on the quality tiers are estimates (yt-dlp doesn't always report exact
  bytes until download time).
- A concurrent download queue (the design shows "1 active · 2 queued") is not
  wired yet — downloads currently run one at a time.
- Fonts (Space Grotesk / Barlow) load from Google Fonts; vendor them locally for
  fully offline use.
