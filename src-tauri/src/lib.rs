// FETCH — mini downloader backend.
// Thin Rust layer over `yt-dlp` (+ `ffmpeg` for merging/extraction). The
// frontend drives everything through the commands below; long-running
// downloads stream progress back as events.

use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

// ── binary resolution ────────────────────────────────────────────────────
// GUI apps on macOS launch with a bare PATH that misses Homebrew, so we probe
// the usual install dirs ourselves and hand yt-dlp an augmented PATH so it can
// in turn find ffmpeg. On Windows we don't touch PATH/the registry at all —
// yt-dlp.exe/ffmpeg.exe are downloaded straight into `portable_bin_dir()`,
// an app-owned folder that's always searched first.

const APP_ID: &str = "com.fetch.downloader"; // must match tauri.conf.json "identifier"

/// App-owned folder that holds our own copies of yt-dlp/ffmpeg. Nothing is
/// installed system-wide, so uninstalling FETCH just means deleting its own
/// folders — no leftover PATH entries, registry keys, or shared runtimes.
fn portable_bin_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(APP_ID).join("bin")
}

fn extra_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        portable_bin_dir(),
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/opt/local/bin"),
    ];
    if let Some(home) = dirs_home() {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join("bin"));
    }
    dirs
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Windows caches PATH in each process's environment block at creation time.
/// Installers like winget update the *registry* and broadcast a change
/// notification, but nothing forces an already-running process (or a shell
/// spawned before the install, e.g. a dev-mode terminal) to reload it — so
/// a self-relaunch that just inherits our own env won't see new tools
/// either. Read the live value straight from the registry instead.
#[cfg(windows)]
fn live_path_dirs() -> Vec<PathBuf> {
    fn read(key: &winreg::RegKey, subkey: &str) -> Option<String> {
        key.open_subkey(subkey).ok()?.get_value::<String, _>("Path").ok()
    }

    fn expand(raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        let mut chars = raw.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            let mut name = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '%' {
                    closed = true;
                    break;
                }
                name.push(c2);
            }
            match (closed, std::env::var(&name)) {
                (true, Ok(val)) => out.push_str(&val),
                (true, Err(_)) => {
                    out.push('%');
                    out.push_str(&name);
                    out.push('%');
                }
                (false, _) => {
                    out.push('%');
                    out.push_str(&name);
                }
            }
        }
        out
    }

    let mut dirs = Vec::new();
    if let Some(raw) = read(winreg::HKCU, "Environment") {
        dirs.extend(std::env::split_paths(&expand(&raw)));
    }
    if let Some(raw) = read(
        winreg::HKLM,
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment",
    ) {
        dirs.extend(std::env::split_paths(&expand(&raw)));
    }
    if dirs.is_empty() {
        if let Some(path) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&path));
        }
    }
    dirs
}

#[cfg(not(windows))]
fn live_path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Resolve a binary to an absolute path if we can find it in PATH or a known dir.
fn resolve_binary(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    // 1) explicit known dirs
    for dir in extra_bin_dirs() {
        let cand = dir.join(&exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    // 2) PATH (live from the registry on Windows, process env elsewhere)
    for dir in live_path_dirs() {
        let cand = dir.join(&exe);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// PATH string that includes our extra dirs, so yt-dlp can locate ffmpeg.
fn augmented_path() -> String {
    let mut parts: Vec<PathBuf> = extra_bin_dirs();
    parts.extend(live_path_dirs());
    std::env::join_paths(parts)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn ytdlp() -> Result<PathBuf, String> {
    resolve_binary("yt-dlp").ok_or_else(|| {
        "yt-dlp not found. Use FETCH's installer, or `brew install yt-dlp`, then restart FETCH.".into()
    })
}

// ── data shapes shared with the frontend ─────────────────────────────────

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct Binaries {
    yt_dlp: bool,
    ffmpeg: bool,
    yt_dlp_path: Option<String>,
    ffmpeg_path: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct QualityOption {
    id: String,
    label: String,
    height: Option<u64>,
    approx_bytes: Option<u64>,
    format_selector: String,
    kind: String, // "video" | "audio"
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct PlaylistEntry {
    index: u64,
    title: String,
    duration_seconds: Option<f64>,
    thumbnail: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AnalyzeResult {
    kind: String, // "video" | "playlist" | "mix"
    webpage_url: String,
    title: String,
    uploader: Option<String>,
    duration_seconds: Option<f64>,
    view_count: Option<u64>,
    thumbnail: Option<String>,
    extractor: String,
    video_options: Vec<QualityOption>,
    audio_options: Vec<QualityOption>,
    entries: Vec<PlaylistEntry>,
    playlist_count: Option<u64>,
}

// Builds the yt-dlp cookie flags for the current settings. `mode` is
// "browser" | "file" | anything else (treated as no cookies).
fn cookie_args(mode: Option<&str>, browser: Option<&str>, file: Option<&str>) -> Vec<String> {
    match mode {
        Some("browser") => match browser.filter(|s| !s.is_empty()) {
            Some(b) => vec!["--cookies-from-browser".into(), b.to_string()],
            None => vec![],
        },
        Some("file") => match file.filter(|s| !s.is_empty()) {
            Some(f) => vec!["--cookies".into(), f.to_string()],
            None => vec![],
        },
        _ => vec![],
    }
}

// ── commands ─────────────────────────────────────────────────────────────

#[tauri::command]
fn check_binaries() -> Binaries {
    let yt = resolve_binary("yt-dlp");
    let ff = resolve_binary("ffmpeg");
    Binaries {
        yt_dlp: yt.is_some(),
        ffmpeg: ff.is_some(),
        yt_dlp_path: yt.map(|p| p.to_string_lossy().into_owned()),
        ffmpeg_path: ff.map(|p| p.to_string_lossy().into_owned()),
    }
}

#[tauri::command]
fn default_download_dir() -> String {
    let base = dirs_home().unwrap_or_else(|| PathBuf::from("."));
    base.join("Downloads").join("Fetch").to_string_lossy().into_owned()
}

// Registered in `jobs` under `id` for the duration of the call so a fresh
// analyze request (the user editing the link and resubmitting) can kill
// this one via `cancel_analyze` instead of letting two run concurrently.
#[tauri::command]
async fn analyze(
    id: String,
    jobs: State<'_, Jobs>,
    url: String,
    cookie_mode: Option<String>,
    cookie_browser: Option<String>,
    cookie_file: Option<String>,
) -> Result<AnalyzeResult, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("Empty link.".into());
    }
    let bin = ytdlp()?;
    // -J dumps a single JSON. --flat-playlist keeps playlist reads fast (we
    // only need entry titles, not each entry's full format list).
    let mut args: Vec<String> = vec![
        "-J".into(),
        "--no-warnings".into(),
        "--flat-playlist".into(),
        "--playlist-items".into(),
        "1:60".into(),
    ];
    args.extend(cookie_args(
        cookie_mode.as_deref(),
        cookie_browser.as_deref(),
        cookie_file.as_deref(),
    ));
    args.push(url.clone());

    let mut child = Command::new(&bin)
        .args(&args)
        .env("PATH", augmented_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to launch yt-dlp: {e}"))?;

    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;

    // register for cancellation
    jobs.0.lock().unwrap().insert(id.clone(), child);

    // Read both streams concurrently (not sequentially) — -J output for a
    // large playlist can exceed the OS pipe buffer, and reading stdout to
    // completion before touching stderr risks a deadlock if yt-dlp is
    // blocked writing warnings to the other pipe.
    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let mut stdout_buf = Vec::new();
    let _ = stdout.read_to_end(&mut stdout_buf).await;

    // reclaim the child and await its exit status
    let mut child = jobs
        .0
        .lock()
        .unwrap()
        .remove(&id)
        .ok_or("Analysis was cancelled.")?;
    let status = child
        .wait()
        .await
        .map_err(|e| format!("yt-dlp did not exit cleanly: {e}"))?;
    let stderr_buf = stderr_handle.await.unwrap_or_default();

    if !status.success() {
        let err = String::from_utf8_lossy(&stderr_buf);
        return Err(clean_ytdlp_error(&err));
    }

    let json: serde_json::Value = serde_json::from_slice(&stdout_buf)
        .map_err(|e| format!("Could not parse yt-dlp output: {e}"))?;

    Ok(build_analyze_result(&json, &url))
}

#[tauri::command]
fn cancel_analyze(jobs: State<'_, Jobs>, id: String) -> Result<(), String> {
    if let Some(mut child) = jobs.0.lock().unwrap().remove(&id) {
        let _ = child.start_kill();
    }
    Ok(())
}

fn clean_ytdlp_error(raw: &str) -> String {
    // Surface the most relevant ERROR line rather than the whole traceback.
    for line in raw.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("ERROR:") {
            return rest.trim().to_string();
        }
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "yt-dlp could not read that link.".to_string()
    } else {
        trimmed.lines().next().unwrap_or("Unknown error").to_string()
    }
}

fn build_analyze_result(json: &serde_json::Value, url: &str) -> AnalyzeResult {
    let typ = json.get("_type").and_then(|v| v.as_str()).unwrap_or("video");
    let extractor = json
        .get("extractor_key")
        .or_else(|| json.get("extractor"))
        .and_then(|v| v.as_str())
        .unwrap_or("source")
        .to_string();

    if typ == "playlist" {
        let playlist_id = json.get("id").and_then(|v| v.as_str()).unwrap_or("");
        // Mixes / radios are auto-generated and effectively endless.
        let is_mix = playlist_id.starts_with("RD") || url.contains("list=RD");
        let entries: Vec<PlaylistEntry> = json
            .get("entries")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .enumerate()
                    .filter_map(|(i, e)| {
                        let title = e.get("title").and_then(|v| v.as_str())?.to_string();
                        Some(PlaylistEntry {
                            index: (i as u64) + 1,
                            title,
                            duration_seconds: e.get("duration").and_then(|v| v.as_f64()),
                            thumbnail: e
                                .get("thumbnails")
                                .and_then(|t| t.as_array())
                                .and_then(|a| a.first())
                                .and_then(|t| t.get("url"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string()),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let (video_options, audio_options) = generic_options();
        return AnalyzeResult {
            kind: if is_mix { "mix" } else { "playlist" }.to_string(),
            webpage_url: json
                .get("webpage_url")
                .and_then(|v| v.as_str())
                .unwrap_or(url)
                .to_string(),
            title: json
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("Playlist")
                .to_string(),
            uploader: json.get("uploader").and_then(|v| v.as_str()).map(String::from),
            duration_seconds: None,
            view_count: None,
            thumbnail: None,
            extractor,
            video_options,
            audio_options,
            playlist_count: json
                .get("playlist_count")
                .and_then(|v| v.as_u64())
                .or(Some(entries.len() as u64)),
            entries,
        };
    }

    // single video
    let duration = json.get("duration").and_then(|v| v.as_f64());
    let (video_options, audio_options) = derive_options(json, duration);

    AnalyzeResult {
        kind: "video".to_string(),
        webpage_url: json
            .get("webpage_url")
            .and_then(|v| v.as_str())
            .unwrap_or(url)
            .to_string(),
        title: json
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled")
            .to_string(),
        uploader: json
            .get("uploader")
            .or_else(|| json.get("channel"))
            .and_then(|v| v.as_str())
            .map(String::from),
        duration_seconds: duration,
        view_count: json.get("view_count").and_then(|v| v.as_u64()),
        thumbnail: json.get("thumbnail").and_then(|v| v.as_str()).map(String::from),
        extractor,
        video_options,
        audio_options,
        entries: vec![],
        playlist_count: None,
    }
}

/// Same BEST / 1080 / 720 / 480 / MP3 / M4A tiers as `derive_options`, but
/// without size estimates — used for playlists/mixes, where `--flat-playlist`
/// never fetches each entry's format list.
fn generic_options() -> (Vec<QualityOption>, Vec<QualityOption>) {
    let video = vec![
        QualityOption { id: "best".into(), label: "BEST".into(), height: None, approx_bytes: None, format_selector: "bv*+ba/b".into(), kind: "video".into() },
        QualityOption { id: "1080p".into(), label: "1080P".into(), height: Some(1080), approx_bytes: None, format_selector: "bv*[height<=1080]+ba/b[height<=1080]".into(), kind: "video".into() },
        QualityOption { id: "720p".into(), label: "720P".into(), height: Some(720), approx_bytes: None, format_selector: "bv*[height<=720]+ba/b[height<=720]".into(), kind: "video".into() },
        QualityOption { id: "480p".into(), label: "480P".into(), height: Some(480), approx_bytes: None, format_selector: "bv*[height<=480]+ba/b[height<=480]".into(), kind: "video".into() },
    ];
    let audio = vec![
        QualityOption { id: "mp3".into(), label: "MP3 · 192K".into(), height: None, approx_bytes: None, format_selector: "ba/b".into(), kind: "audio".into() },
        QualityOption { id: "m4a".into(), label: "M4A · BEST".into(), height: None, approx_bytes: None, format_selector: "ba[ext=m4a]/ba/b".into(), kind: "audio".into() },
    ];
    (video, audio)
}

/// Build a BEST tier plus one tier per resolution the source actually offers,
/// and MP3 / M4A audio tiers, estimating file sizes from the format list
/// where yt-dlp provides them.
fn derive_options(
    json: &serde_json::Value,
    duration: Option<f64>,
) -> (Vec<QualityOption>, Vec<QualityOption>) {
    let empty = vec![];
    let formats = json
        .get("formats")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

    // Storyboard tiles (the mhtml sprite sheets behind the seek-bar preview)
    // report no codec at all rather than an explicit "none", so a naive
    // `!= Some("none")` check treats the missing field as "has video" and
    // lets their odd little tile sizes (e.g. 106p/178p/266p) leak into the
    // quality list. Require the field to be present and non-"none" instead,
    // and drop mhtml formats outright as a second line of defense.
    let is_storyboard = |f: &serde_json::Value| -> bool {
        f.get("ext").and_then(|v| v.as_str()) == Some("mhtml")
            || f.get("protocol").and_then(|v| v.as_str()) == Some("mhtml")
    };
    let has_video = |f: &serde_json::Value| -> bool {
        !is_storyboard(f) && f.get("vcodec").and_then(|v| v.as_str()).is_some_and(|v| v != "none")
    };
    let has_audio = |f: &serde_json::Value| -> bool {
        !is_storyboard(f) && f.get("acodec").and_then(|v| v.as_str()).is_some_and(|v| v != "none")
    };

    let fmt_bytes = |f: &serde_json::Value| -> Option<u64> {
        f.get("filesize")
            .and_then(|v| v.as_u64())
            .or_else(|| f.get("filesize_approx").and_then(|v| v.as_u64()))
            .or_else(|| {
                // fall back to bitrate * duration
                let tbr = f.get("tbr").and_then(|v| v.as_f64())?;
                let dur = duration?;
                Some((tbr * 1000.0 / 8.0 * dur) as u64)
            })
    };

    // best audio-only stream (for muxing size estimates)
    let best_audio_bytes = formats
        .iter()
        .filter(|f| !has_video(f) && has_audio(f))
        .filter_map(fmt_bytes)
        .max()
        .unwrap_or(0);

    let bytes_for_height = |cap: u64| -> Option<u64> {
        formats
            .iter()
            .filter(|f| has_video(f) && !has_audio(f)) // video-only, will be muxed
            .filter(|f| f.get("height").and_then(|v| v.as_u64()).map(|h| h <= cap).unwrap_or(false))
            .filter_map(|f| {
                let h = f.get("height").and_then(|v| v.as_u64())?;
                let b = fmt_bytes(f)?;
                Some((h, b))
            })
            .max_by_key(|(h, _)| *h)
            .map(|(_, b)| b + best_audio_bytes)
    };

    let mut video = Vec::new();
    // one tier per resolution the source actually offers (not a fixed
    // 1080/720/480 shortlist) — mirrors what yt-dlp -F would list. No
    // separate "BEST" entry: the highest tier here already is the best.
    let mut heights: Vec<u64> = formats
        .iter()
        .filter(|f| has_video(f))
        .filter_map(|f| f.get("height").and_then(|v| v.as_u64()))
        .collect();
    heights.sort_unstable();
    heights.dedup();
    for cap in heights.into_iter().rev() {
        video.push(QualityOption {
            id: format!("{cap}p"),
            label: format!("{cap}P"),
            height: Some(cap),
            approx_bytes: bytes_for_height(cap),
            format_selector: format!("bv*[height<={cap}]+ba/b[height<={cap}]"),
            kind: "video".into(),
        });
    }

    let audio = vec![
        QualityOption {
            id: "mp3".into(),
            label: "MP3 · 192K".into(),
            height: None,
            approx_bytes: if best_audio_bytes > 0 { Some(best_audio_bytes) } else { None },
            format_selector: "ba/b".into(),
            kind: "audio".into(),
        },
        QualityOption {
            id: "m4a".into(),
            label: "M4A · BEST".into(),
            height: None,
            approx_bytes: if best_audio_bytes > 0 { Some(best_audio_bytes) } else { None },
            format_selector: "ba[ext=m4a]/ba/b".into(),
            kind: "audio".into(),
        },
    ];

    (video, audio)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRequest {
    id: String,
    url: String,
    format_selector: String,
    kind: String,          // "video" | "audio"
    audio_format: Option<String>, // "mp3" | "m4a" when kind == audio
    output_dir: String,
    write_thumbnail: bool, // save the thumbnail as its own image file
    write_description: bool, // save the description to a .description text file
    write_subs: bool,      // save subtitles as their own .srt file
    playlist_items: Option<String>, // e.g. "1,3,4-6" for playlist selections
    cookie_mode: Option<String>,
    cookie_browser: Option<String>,
    cookie_file: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ProgressPayload {
    id: String,
    percent: f64,
    speed: String,
    eta: String,
    stage: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct DonePayload {
    id: String,
    filepath: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ErrorPayload {
    id: String,
    message: String,
}

#[derive(Default)]
struct Jobs(Mutex<HashMap<String, Child>>);

#[tauri::command]
async fn download(
    app: AppHandle,
    jobs: State<'_, Jobs>,
    req: DownloadRequest,
) -> Result<(), String> {
    let bin = ytdlp()?;
    std::fs::create_dir_all(&req.output_dir)
        .map_err(|e| format!("Cannot create download folder: {e}"))?;

    // Every video gets its own folder, named with today's date so the
    // download dir sorts cleanly, e.g. "260715 - My Video". For playlists
    // yt-dlp fills in %(title)s per entry, so each video still lands in its
    // own folder rather than being pooled together.
    let today = Local::now().format("%y%m%d");
    let out_tmpl = format!(
        "{}/{today} - %(title)s/%(title)s.%(ext)s",
        req.output_dir.trim_end_matches('/')
    );

    let mut args: Vec<String> = vec![
        req.url.clone(),
        "-f".into(),
        req.format_selector.clone(),
        "-o".into(),
        out_tmpl,
        "--newline".into(),
        "--no-color".into(),
        "--no-warnings".into(),
        // machine-parsable progress on stdout
        "--progress".into(),
        "--progress-template".into(),
        "download:FETCHPROG|%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s".into(),
        // final path(s) after post-processing/move, prefixed so we can grep them
        "--no-simulate".into(),
        "--print".into(),
        "after_move:FETCHFILE|%(filepath)s".into(),
    ];
    args.extend(cookie_args(
        req.cookie_mode.as_deref(),
        req.cookie_browser.as_deref(),
        req.cookie_file.as_deref(),
    ));

    if req.kind == "audio" {
        let af = req.audio_format.as_deref().unwrap_or("mp3");
        args.push("-x".into());
        args.push("--audio-format".into());
        args.push(af.into());
    } else {
        args.push("--merge-output-format".into());
        args.push("mp4".into());
    }
    // Thumbnail + metadata are always embedded into the media file itself.
    args.push("--embed-metadata".into());
    args.push("--embed-thumbnail".into());

    if req.write_thumbnail {
        args.push("--write-thumbnail".into());
    }
    if req.write_description {
        args.push("--write-description".into());
    }
    if req.write_subs && req.kind == "video" {
        args.push("--write-subs".into());
        args.push("--sub-langs".into());
        args.push("en.*,en".into());
        args.push("--convert-subs".into());
        args.push("srt".into());
    }
    if let Some(items) = &req.playlist_items {
        if !items.is_empty() {
            args.push("--playlist-items".into());
            args.push(items.clone());
        }
    } else {
        // a lone video inside a mix/playlist URL — grab just it
        args.push("--no-playlist".into());
    }

    let mut child = Command::new(&bin)
        .args(&args)
        .env("PATH", augmented_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to launch yt-dlp: {e}"))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;

    // register for cancellation
    jobs.0.lock().unwrap().insert(req.id.clone(), child);

    let id = req.id.clone();
    let mut final_path: Option<String> = None;
    // One entry per media file yt-dlp finishes moving into place — almost
    // always one, but a playlist download prints this once per video, so we
    // need all of them (not just the last) to know which `.description`
    // sidecar files to tidy up below.
    let mut all_paths: Vec<String> = Vec::new();

    // stream stderr into a buffer for a useful error message
    let err_app = app.clone();
    let err_id = id.clone();
    let stderr_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut buf = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            if line.trim().starts_with("ERROR:") {
                buf.push_str(&line);
                buf.push('\n');
            }
            let _ = err_app.emit("dl-log", (err_id.clone(), line));
        }
        buf
    });

    let mut reader = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        if let Some(rest) = line.strip_prefix("FETCHPROG|") {
            let parts: Vec<&str> = rest.split('|').collect();
            let percent = parts
                .first()
                .map(|s| s.trim().trim_end_matches('%').trim())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let payload = ProgressPayload {
                id: id.clone(),
                percent,
                speed: parts.get(1).map(|s| s.trim().to_string()).unwrap_or_default(),
                eta: parts.get(2).map(|s| s.trim().to_string()).unwrap_or_default(),
                stage: "downloading".into(),
            };
            let _ = app.emit("dl-progress", payload);
        } else if let Some(rest) = line.strip_prefix("FETCHFILE|") {
            let p = rest.trim().to_string();
            all_paths.push(p.clone());
            final_path = Some(p);
        } else if line.contains("[Merger]") || line.contains("Merging formats") {
            let _ = app.emit(
                "dl-progress",
                ProgressPayload {
                    id: id.clone(),
                    percent: 100.0,
                    speed: String::new(),
                    eta: String::new(),
                    stage: "merging".into(),
                },
            );
        } else if line.contains("[ExtractAudio]") {
            let _ = app.emit(
                "dl-progress",
                ProgressPayload {
                    id: id.clone(),
                    percent: 100.0,
                    speed: String::new(),
                    eta: String::new(),
                    stage: "extracting".into(),
                },
            );
        }
    }

    // reclaim the child and await its exit status
    let mut child = jobs
        .0
        .lock()
        .unwrap()
        .remove(&id)
        .ok_or("job was cancelled")?;
    let status = child
        .wait()
        .await
        .map_err(|e| format!("yt-dlp did not exit cleanly: {e}"))?;
    let err_text = stderr_handle.await.unwrap_or_default();

    if status.success() {
        if req.write_description {
            for p in &all_paths {
                tidy_description_file(p);
            }
        }
        app.emit(
            "dl-done",
            DonePayload {
                id: id.clone(),
                filepath: final_path,
            },
        )
        .ok();
        Ok(())
    } else {
        let message = if err_text.trim().is_empty() {
            "Download failed.".to_string()
        } else {
            clean_ytdlp_error(&err_text)
        };
        app.emit(
            "dl-error",
            ErrorPayload {
                id,
                message: message.clone(),
            },
        )
        .ok();
        Err(message)
    }
}

/// yt-dlp's `--write-description` saves the raw description text to a file
/// literally named `<title>.description` — no extension a text/markdown
/// viewer recognizes, so it opens as "unknown file type" until renamed by
/// hand. `media_path` is the finished video/audio file sitting next to it
/// (same folder, same stem); this reads that sidecar, wraps it as a small
/// Markdown doc (title heading + body), writes it out as `<title>.md`, and
/// removes the original. Best-effort: silently no-ops if the sidecar isn't
/// there (e.g. yt-dlp couldn't find a description for this entry).
fn tidy_description_file(media_path: &str) {
    let media = Path::new(media_path);
    let (Some(dir), Some(stem)) = (media.parent(), media.file_stem().and_then(|s| s.to_str())) else {
        return;
    };
    let desc_path = dir.join(format!("{stem}.description"));
    let Ok(body) = std::fs::read_to_string(&desc_path) else {
        return;
    };
    let md_path = dir.join(format!("{stem}.md"));
    let content = format!("# {stem}\n\n{}\n", body.trim_end());
    if std::fs::write(&md_path, content).is_ok() {
        let _ = std::fs::remove_file(&desc_path);
    }
}

#[tauri::command]
fn cancel_download(jobs: State<'_, Jobs>, id: String) -> Result<(), String> {
    if let Some(mut child) = jobs.0.lock().unwrap().remove(&id) {
        let _ = child.start_kill();
    }
    Ok(())
}

/// Reveal a file in the OS file manager (Finder / Explorer), or open a folder.
#[tauri::command]
fn reveal_in_folder(path: String) -> Result<(), String> {
    let p = Path::new(&path);
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        if p.is_file() {
            cmd.arg("-R");
        }
        cmd.arg(&path);
        cmd.spawn().map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "windows")]
    {
        if p.is_file() {
            std::process::Command::new("explorer")
                .arg("/select,")
                .arg(&path)
                .spawn()
                .map_err(|e| e.to_string())?;
        } else {
            std::process::Command::new("explorer")
                .arg(&path)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let target = if p.is_file() {
            p.parent().unwrap_or(p).to_path_buf()
        } else {
            p.to_path_buf()
        };
        std::process::Command::new("xdg-open")
            .arg(target)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── in-app installer ─────────────────────────────────────────────────────
// Lets the user get the missing tools without leaving the app.
//
// - Windows: downloaded straight into `portable_bin_dir()` as standalone
//   .exe files (yt-dlp ships one; ffmpeg's is pulled out of the official
//   gyan.dev static build zip). Nothing touches the system PATH, the
//   registry, or a package manager — install/uninstall is just files in one
//   app-owned folder.
// - macOS: yt-dlp is downloaded the same portable way, straight from its
//   GitHub release (`yt-dlp_macos`, a universal x86_64+arm64 binary — no
//   PATH/registry writes). ffmpeg has no official static macOS build (unlike
//   Windows' gyan.dev), and the community ones are either Intel-only
//   (evermeet.cx, needs Rosetta on Apple Silicon) or don't have stable
//   scriptable URLs (osxexperts.net) — so ffmpeg still goes through
//   Homebrew, which already solves that dependency problem properly.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Installers {
    /// An install path (package manager or direct download) is available.
    available: bool,
    manager: String, // "mac" | "fetch" | ""
    platform: String,
}

#[tauri::command]
fn detect_installers() -> Installers {
    let platform = std::env::consts::OS.to_string();
    let (available, manager) = if cfg!(target_os = "macos") {
        // yt-dlp always downloads portably regardless of Homebrew; ffmpeg
        // still needs brew, and that dependency surfaces its own error
        // ("Homebrew isn't installed...") if it's missing when needed.
        (true, "mac")
    } else if cfg!(target_os = "windows") {
        // Self-contained download — always available, no external tool needed.
        (true, "fetch")
    } else {
        (false, "")
    };
    Installers {
        available,
        manager: manager.to_string(),
        platform,
    }
}

#[tauri::command]
async fn install_tools(app: AppHandle, tools: Vec<String>) -> Result<(), String> {
    if tools.is_empty() {
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    let result = install_tools_macos(&app, &tools).await;

    #[cfg(windows)]
    let result = install_tools_portable(&app, &tools).await;

    #[cfg(all(not(target_os = "macos"), not(windows)))]
    let result: Result<(), String> = Err("Automatic install is only supported on macOS and Windows. Install yt-dlp and ffmpeg with your package manager.".to_string());

    if let Err(msg) = &result {
        app.emit("install-error", msg.clone()).ok();
        return result;
    }
    app.emit("install-done", ()).ok();
    Ok(())
}

async fn install_tools_brew(app: &AppHandle, tools: &[String]) -> Result<(), String> {
    brew_cmd(app, "install", tools).await
}

async fn upgrade_tools_brew(app: &AppHandle, tools: &[String]) -> Result<(), String> {
    brew_cmd(app, "upgrade", tools).await
}

async fn brew_cmd(app: &AppHandle, subcommand: &str, tools: &[String]) -> Result<(), String> {
    let brew = resolve_binary("brew")
        .ok_or("Homebrew isn't installed. Get it from https://brew.sh, then reopen FETCH.")?;
    let mut args = vec![subcommand.to_string()];
    args.extend(tools.iter().cloned());

    app.emit(
        "install-log",
        format!("$ {} {}", brew.file_name().and_then(|s| s.to_str()).unwrap_or("brew"), args.join(" ")),
    )
    .ok();

    let mut cmd = Command::new(&brew);
    cmd.args(&args)
        .env("PATH", augmented_path())
        // brew refuses to run if it thinks it's non-interactive in odd ways;
        // NONINTERACTIVE makes it proceed without prompting.
        .env("NONINTERACTIVE", "1");
    // Skipping brew's auto-update is fine (and faster) for a first-time
    // install, but `upgrade` needs a fresh formula index to even know a
    // newer version exists — with auto-update disabled, `brew upgrade` on a
    // stale tap silently no-ops (exits 0, nothing changes), so our own
    // version check against gyan.dev keeps reporting the same update
    // available forever.
    if subcommand != "upgrade" {
        cmd.env("HOMEBREW_NO_AUTO_UPDATE", "1");
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to launch installer: {e}"))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;

    let a2 = app.clone();
    let err_handle = tokio::spawn(async move {
        let mut r = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = r.next_line().await {
            a2.emit("install-log", l).ok();
        }
    });
    let mut r = BufReader::new(stdout).lines();
    while let Ok(Some(l)) = r.next_line().await {
        app.emit("install-log", l).ok();
    }
    let _ = err_handle.await;

    let status = child
        .wait()
        .await
        .map_err(|e| format!("Installer did not exit cleanly: {e}"))?;
    if !status.success() {
        return Err("Homebrew reported an error. See the log above.".to_string());
    }
    Ok(())
}

/// Downloads `url` to `dest` via the system `curl` (ships with macOS),
/// writing to a `.part` file first so a failed download never leaves a
/// half-written binary behind.
#[cfg(target_os = "macos")]
async fn mac_curl_download(url: &str, dest: &Path) -> Result<(), String> {
    let curl = resolve_binary("curl").ok_or("curl not found. It ships with macOS — check it hasn't been removed.")?;
    let tmp = dest.with_extension("part");
    let status = Command::new(&curl)
        // --connect-timeout bounds the initial connection; --speed-time/
        // --speed-limit aborts a connection that stalls mid-transfer —
        // without these a dropped/blocked connection can hang indefinitely.
        .args(["-L", "-sS", "--fail", "--connect-timeout", "15", "--speed-time", "30", "--speed-limit", "1000", "-o"])
        .arg(&tmp)
        .arg(url)
        .status()
        .await
        .map_err(|e| format!("Failed to launch curl: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Download failed: {url}"));
    }
    std::fs::rename(&tmp, dest).map_err(|e| format!("Couldn't save {}: {e}", dest.display()))
}

/// yt-dlp downloads portably (its GitHub release ships a universal
/// x86_64+arm64 macOS binary, so there's no arch-detection needed). Any
/// other requested tool (i.e. ffmpeg) still goes through Homebrew.
#[cfg(target_os = "macos")]
async fn install_tools_macos(app: &AppHandle, tools: &[String]) -> Result<(), String> {
    let mut brew_tools: Vec<String> = Vec::new();

    for tool in tools {
        if tool == "yt-dlp" {
            let bin_dir = portable_bin_dir();
            std::fs::create_dir_all(&bin_dir)
                .map_err(|e| format!("Couldn't create {}: {e}", bin_dir.display()))?;
            let dest = bin_dir.join("yt-dlp");

            app.emit("install-log", "Downloading yt-dlp (portable, universal binary)…").ok();
            mac_curl_download(
                "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos",
                &dest,
            )
            .await?;

            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest).map_err(|e| e.to_string())?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dest, perms).map_err(|e| e.to_string())?;

            app.emit("install-log", format!("Saved to {}", dest.display())).ok();
        } else {
            brew_tools.push(tool.clone());
        }
    }

    if !brew_tools.is_empty() {
        install_tools_brew(app, &brew_tools).await?;
    }
    Ok(())
}

/// Windows' own System32 copy of a tool, bypassing PATH entirely. Some
/// machines have a different `curl`/`tar` earlier on PATH (e.g. Git for
/// Windows' GNU tar, which — unlike the bsdtar in System32 — can't read
/// .zip archives), so for tools we depend on for correctness we go straight
/// to the OS-bundled copy (Windows 10 1803+ / 11) instead of trusting
/// whichever one `resolve_binary` happens to find first.
#[cfg(windows)]
fn system32_bin(name: &str) -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let cand = root.join("System32").join(format!("{name}.exe"));
    cand.is_file().then_some(cand)
}

/// Downloads `url` to `dest` via curl, writing to a `.part` file first so a
/// failed download never leaves a half-written binary behind.
#[cfg(windows)]
async fn curl_download(url: &str, dest: &Path) -> Result<(), String> {
    let curl = system32_bin("curl")
        .or_else(|| resolve_binary("curl"))
        .ok_or("curl.exe not found. It ships with Windows 10/11 — check it hasn't been removed.")?;
    let tmp = dest.with_extension("part");
    let status = Command::new(&curl)
        .args(["-L", "-sS", "--fail", "--connect-timeout", "15", "--speed-time", "30", "--speed-limit", "1000", "-o"])
        .arg(&tmp)
        .arg(url)
        .status()
        .await
        .map_err(|e| format!("Failed to launch curl: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Download failed: {url}"));
    }
    std::fs::rename(&tmp, dest).map_err(|e| format!("Couldn't save {}: {e}", dest.display()))
}

/// Recursively finds a file named `name` under `dir` (used to pull
/// ffmpeg.exe/ffprobe.exe out of the zip's versioned `…/bin/` subfolder
/// without having to know its exact name).
#[cfg(windows)]
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|s| s.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}

#[cfg(windows)]
async fn install_tools_portable(app: &AppHandle, tools: &[String]) -> Result<(), String> {
    let bin_dir = portable_bin_dir();
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| format!("Couldn't create {}: {e}", bin_dir.display()))?;

    for tool in tools {
        match tool.as_str() {
            "yt-dlp" => {
                app.emit("install-log", "Downloading yt-dlp.exe…").ok();
                curl_download(
                    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe",
                    &bin_dir.join("yt-dlp.exe"),
                )
                .await?;
                app.emit("install-log", format!("Saved to {}", bin_dir.join("yt-dlp.exe").display())).ok();
            }
            "ffmpeg" => {
                app.emit("install-log", "Downloading ffmpeg (~90 MB, can take a few minutes)…").ok();
                let tmp_dir = std::env::temp_dir().join(format!("fetch-ffmpeg-{}", std::process::id()));
                std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
                let zip_path = tmp_dir.join("ffmpeg.zip");

                let dl = curl_download(
                    "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip",
                    &zip_path,
                )
                .await;
                if let Err(e) = dl {
                    let _ = std::fs::remove_dir_all(&tmp_dir);
                    return Err(e);
                }

                app.emit("install-log", "Extracting ffmpeg…").ok();
                // Must be the bsdtar in System32 — GNU tar (e.g. Git for Windows') can't read .zip.
                let tar = system32_bin("tar")
                    .ok_or("tar.exe not found in System32. It ships with Windows 10/11 — check it hasn't been removed.")?;
                let status = Command::new(&tar)
                    .arg("-xf")
                    .arg(&zip_path)
                    .arg("-C")
                    .arg(&tmp_dir)
                    .status()
                    .await
                    .map_err(|e| format!("Failed to run tar: {e}"))?;
                if !status.success() {
                    let _ = std::fs::remove_dir_all(&tmp_dir);
                    return Err("Failed to extract the ffmpeg archive.".to_string());
                }

                let mut found = 0;
                for name in ["ffmpeg.exe", "ffprobe.exe"] {
                    if let Some(src) = find_file(&tmp_dir, name) {
                        std::fs::copy(&src, bin_dir.join(name))
                            .map_err(|e| format!("Couldn't copy {name}: {e}"))?;
                        found += 1;
                    }
                }
                let _ = std::fs::remove_dir_all(&tmp_dir);
                if found < 2 {
                    return Err("ffmpeg.exe/ffprobe.exe weren't found in the downloaded archive.".to_string());
                }
                app.emit("install-log", format!("Saved ffmpeg + ffprobe to {}", bin_dir.display())).ok();
            }
            other => {
                app.emit("install-log", format!("Skipping unknown tool: {other}")).ok();
            }
        }
    }
    Ok(())
}

// ── update checking ──────────────────────────────────────────────────────
// Best-effort and read-only by default: `check_for_updates` never fails the
// whole call just because one lookup (or the network) is unavailable — a
// tool's `current`/`latest` just stay `None` and `updateAvailable` false.
// yt-dlp especially needs this: it ships new releases every 1-2 weeks to
// keep up with YouTube's changes, and a stale copy silently breaks.

#[derive(Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct ToolUpdate {
    current: Option<String>,
    latest: Option<String>,
    update_available: bool,
    source: String, // "portable" | "brew" | "other" | ""
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateCheck {
    yt_dlp: ToolUpdate,
    ffmpeg: ToolUpdate,
}

/// Where a resolved binary came from, so `update_tool` knows how to update
/// it: our own portable copy can just be re-downloaded in place, a Homebrew
/// one gets `brew upgrade`, anything else (system pip, apt, a manual copy on
/// PATH) we don't touch.
fn tool_source(path: &Path) -> &'static str {
    if path.starts_with(portable_bin_dir()) {
        "portable"
    } else if path.starts_with("/opt/homebrew")
        || path.starts_with("/usr/local/Cellar")
        || path.starts_with("/usr/local/bin")
        || path.starts_with("/opt/local")
    {
        "brew"
    } else {
        "other"
    }
}

/// GET `url` via the system `curl` and return the response body as text.
async fn curl_get(url: &str) -> Result<String, String> {
    let curl = resolve_binary("curl").ok_or("curl not found.")?;
    let output = Command::new(&curl)
        .args(["-L", "-sS", "--fail", "--connect-timeout", "10", "--max-time", "15", "-H", "User-Agent: fetch-app"])
        .arg(url)
        .env("PATH", augmented_path())
        .output()
        .await
        .map_err(|e| format!("Failed to launch curl: {e}"))?;
    if !output.status.success() {
        return Err(format!("Request failed: {url}"));
    }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

async fn latest_ytdlp_version() -> Result<String, String> {
    let body = curl_get("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest").await?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    json.get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Unexpected response from GitHub".to_string())
}

/// gyan.dev (the same source Windows' portable ffmpeg comes from) publishes
/// this as a plain-text file with just the version, e.g. "8.1.2" — it tracks
/// upstream FFmpeg's actual release version, so it's a fine stand-in for
/// "latest" on macOS too even though that build itself is Windows-only.
async fn latest_ffmpeg_version() -> Result<String, String> {
    let body = curl_get("https://www.gyan.dev/ffmpeg/builds/release-version").await?;
    let v = body.trim().to_string();
    if v.is_empty() {
        Err("Empty response".to_string())
    } else {
        Ok(v)
    }
}

async fn binary_version_output(path: &Path, arg: &str) -> Option<String> {
    let out = Command::new(path).arg(arg).output().await.ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !stdout.trim().is_empty() {
        Some(stdout.into_owned())
    } else {
        Some(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// Pulls the first digit-dot run out of a string, e.g.
/// "ffmpeg version 8.1.2 Copyright..." -> "8.1.2".
fn parse_leading_version(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let tok = s[start..i].trim_end_matches('.');
            if tok.contains('.') {
                return Some(tok.to_string());
            }
        }
        i += 1;
    }
    None
}

/// Just the locally-installed version + source of each tool — no network
/// calls, so it resolves near-instantly. Kept separate from
/// `check_for_updates` so the frontend can show "current version" right
/// away instead of it being stuck blank behind a slow/blocked network
/// lookup of the *latest* version.
#[tauri::command]
async fn installed_versions() -> UpdateCheck {
    let mut out = UpdateCheck::default();
    if let Some(path) = resolve_binary("yt-dlp") {
        out.yt_dlp.source = tool_source(&path).to_string();
        out.yt_dlp.current = binary_version_output(&path, "--version")
            .await
            .map(|s| s.trim().to_string());
    }
    if let Some(path) = resolve_binary("ffmpeg") {
        out.ffmpeg.source = tool_source(&path).to_string();
        out.ffmpeg.current = binary_version_output(&path, "-version")
            .await
            .and_then(|s| parse_leading_version(&s));
    }
    out
}

#[tauri::command]
async fn check_for_updates() -> UpdateCheck {
    let mut out = installed_versions().await;

    if !out.yt_dlp.source.is_empty() {
        if let Ok(latest) = latest_ytdlp_version().await {
            out.yt_dlp.update_available = out.yt_dlp.current.as_deref() != Some(latest.as_str());
            out.yt_dlp.latest = Some(latest);
        }
    }

    if !out.ffmpeg.source.is_empty() {
        if let Ok(latest) = latest_ffmpeg_version().await {
            out.ffmpeg.update_available = out.ffmpeg.current.as_deref().map(|c| c != latest).unwrap_or(false);
            out.ffmpeg.latest = Some(latest);
        }
    }

    out
}

/// Updates a single tool in place, using whatever installed it (mirrors
/// `install_tools`'s events: `install-log`/`install-done`/`install-error`,
/// so the frontend can reuse the same progress plumbing).
#[tauri::command]
async fn update_tool(app: AppHandle, tool: String) -> Result<(), String> {
    let path = resolve_binary(&tool).ok_or_else(|| format!("{tool} not found."))?;
    let source = tool_source(&path);

    let result: Result<(), String> = match source {
        "portable" => {
            #[cfg(target_os = "macos")]
            {
                install_tools_macos(&app, std::slice::from_ref(&tool)).await
            }
            #[cfg(windows)]
            {
                install_tools_portable(&app, std::slice::from_ref(&tool)).await
            }
            #[cfg(all(not(target_os = "macos"), not(windows)))]
            {
                Err("Portable updates aren't supported on this platform.".to_string())
            }
        }
        "brew" => upgrade_tools_brew(&app, std::slice::from_ref(&tool)).await,
        _ => Err(format!("{tool} wasn't installed by FETCH or Homebrew — update it manually.")),
    };

    if let Err(msg) = &result {
        app.emit("install-error", msg.clone()).ok();
        return result;
    }
    app.emit("install-done", ()).ok();
    Ok(())
}

/// Relaunches FETCH. Not needed for portable installs (binaries land in
/// `portable_bin_dir()`, which is always searched first — no restart
/// required), but kept as a fallback for the Homebrew path on macOS (ffmpeg).
#[tauri::command]
fn restart_app(app: AppHandle) {
    app.restart();
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(Jobs::default())
        .invoke_handler(tauri::generate_handler![
            check_binaries,
            default_download_dir,
            analyze,
            cancel_analyze,
            download,
            cancel_download,
            reveal_in_folder,
            detect_installers,
            install_tools,
            installed_versions,
            check_for_updates,
            update_tool,
            restart_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
