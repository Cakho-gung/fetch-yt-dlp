// FETCH — mini downloader backend.
// Thin Rust layer over `yt-dlp` (+ `ffmpeg` for merging/extraction). The
// frontend drives everything through the commands below; long-running
// downloads stream progress back as events.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

// ── binary resolution ────────────────────────────────────────────────────
// GUI apps on macOS launch with a bare PATH that misses Homebrew, so we probe
// the usual install dirs ourselves and hand yt-dlp an augmented PATH so it can
// in turn find ffmpeg.

fn extra_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
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
    // 2) PATH
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(&exe);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// PATH string that includes our extra dirs, so yt-dlp can locate ffmpeg.
fn augmented_path() -> String {
    let mut parts: Vec<PathBuf> = extra_bin_dirs();
    if let Some(path) = std::env::var_os("PATH") {
        parts.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(parts)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn ytdlp() -> Result<PathBuf, String> {
    resolve_binary("yt-dlp").ok_or_else(|| {
        "yt-dlp not found. Install it (macOS: `brew install yt-dlp`) and restart FETCH.".into()
    })
}

// ── data shapes shared with the frontend ─────────────────────────────────

#[derive(Serialize, Default)]
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

#[tauri::command]
async fn analyze(url: String) -> Result<AnalyzeResult, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("Empty link.".into());
    }
    let bin = ytdlp()?;
    // -J dumps a single JSON. --flat-playlist keeps playlist reads fast (we
    // only need entry titles, not each entry's full format list).
    let output = Command::new(&bin)
        .args([
            "-J",
            "--no-warnings",
            "--flat-playlist",
            "--playlist-items",
            "1:60",
            &url,
        ])
        .env("PATH", augmented_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to launch yt-dlp: {e}"))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(clean_ytdlp_error(&err));
    }

    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Could not parse yt-dlp output: {e}"))?;

    Ok(build_analyze_result(&json, &url))
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
            video_options: vec![],
            audio_options: vec![],
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

/// Build the BEST / 1080 / 720 / 480 video tiers and MP3 / M4A audio tiers,
/// estimating file sizes from the format list where yt-dlp provides them.
fn derive_options(
    json: &serde_json::Value,
    duration: Option<f64>,
) -> (Vec<QualityOption>, Vec<QualityOption>) {
    let empty = vec![];
    let formats = json
        .get("formats")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

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
        .filter(|f| {
            f.get("vcodec").and_then(|v| v.as_str()) == Some("none")
                && f.get("acodec").and_then(|v| v.as_str()) != Some("none")
        })
        .filter_map(fmt_bytes)
        .max()
        .unwrap_or(0);

    // available video heights
    let max_height = formats
        .iter()
        .filter(|f| f.get("vcodec").and_then(|v| v.as_str()) != Some("none"))
        .filter_map(|f| f.get("height").and_then(|v| v.as_u64()))
        .max();

    let bytes_for_height = |cap: u64| -> Option<u64> {
        formats
            .iter()
            .filter(|f| f.get("vcodec").and_then(|v| v.as_str()) != Some("none"))
            .filter(|f| f.get("acodec").and_then(|v| v.as_str()) == Some("none")) // video-only, will be muxed
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
    // BEST tier — label reflects the real ceiling
    let best_label = match max_height {
        Some(h) if h >= 2160 => "BEST 4K",
        Some(h) if h >= 1440 => "BEST 1440P",
        Some(h) if h >= 1080 => "BEST 1080P",
        _ => "BEST",
    };
    video.push(QualityOption {
        id: "best".into(),
        label: best_label.into(),
        height: max_height,
        approx_bytes: max_height.and_then(bytes_for_height),
        format_selector: "bv*+ba/b".into(),
        kind: "video".into(),
    });
    for cap in [1080u64, 720, 480] {
        // only offer tiers that actually exist (<= max height)
        if max_height.map(|m| m >= cap).unwrap_or(true) {
            video.push(QualityOption {
                id: format!("{cap}p"),
                label: format!("{cap}P"),
                height: Some(cap),
                approx_bytes: bytes_for_height(cap),
                format_selector: format!("bv*[height<={cap}]+ba/b[height<={cap}]"),
                kind: "video".into(),
            });
        }
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
    embed_subs: bool,
    embed_metadata: bool,
    playlist_items: Option<String>, // e.g. "1,3,4-6" for playlist selections
    #[allow(dead_code)]
    title: Option<String>,
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

    let out_tmpl = format!("{}/%(title)s.%(ext)s", req.output_dir.trim_end_matches('/'));

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

    if req.kind == "audio" {
        let af = req.audio_format.as_deref().unwrap_or("mp3");
        args.push("-x".into());
        args.push("--audio-format".into());
        args.push(af.into());
    } else {
        args.push("--merge-output-format".into());
        args.push("mp4".into());
    }
    if req.embed_metadata {
        args.push("--embed-metadata".into());
        args.push("--embed-thumbnail".into());
    }
    if req.embed_subs && req.kind == "video" {
        args.push("--embed-subs".into());
        args.push("--sub-langs".into());
        args.push("en.*,en".into());
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
            final_path = Some(rest.trim().to_string());
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
// Lets the user install the missing tools without leaving the app, by driving
// the platform package manager (Homebrew / winget) with a live log stream.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Installers {
    /// A package manager we can drive is present for this platform.
    available: bool,
    manager: String, // "brew" | "winget" | ""
    platform: String,
}

#[tauri::command]
fn detect_installers() -> Installers {
    let platform = std::env::consts::OS.to_string();
    let (available, manager) = if cfg!(target_os = "macos") {
        (resolve_binary("brew").is_some(), "brew")
    } else if cfg!(target_os = "windows") {
        (resolve_binary("winget").is_some(), "winget")
    } else {
        (false, "")
    };
    Installers {
        available,
        manager: manager.to_string(),
        platform,
    }
}

/// Build the per-platform installer invocations for the requested tools.
fn build_install_plan(tools: &[String]) -> Result<Vec<(PathBuf, Vec<String>)>, String> {
    if cfg!(target_os = "macos") {
        let brew = resolve_binary("brew").ok_or(
            "Homebrew isn't installed. Get it from https://brew.sh, then reopen FETCH.",
        )?;
        let mut args = vec!["install".to_string()];
        args.extend(tools.iter().cloned());
        Ok(vec![(brew, args)])
    } else if cfg!(target_os = "windows") {
        let winget = resolve_binary("winget").ok_or(
            "winget isn't available. Install 'App Installer' from the Microsoft Store, then reopen FETCH.",
        )?;
        // winget installs one package id per call.
        let plan = tools
            .iter()
            .map(|t| {
                let id = match t.as_str() {
                    "yt-dlp" => "yt-dlp.yt-dlp",
                    "ffmpeg" => "Gyan.FFmpeg",
                    other => other,
                };
                (
                    winget.clone(),
                    vec![
                        "install".into(),
                        "--id".into(),
                        id.into(),
                        "-e".into(),
                        "--accept-source-agreements".into(),
                        "--accept-package-agreements".into(),
                    ],
                )
            })
            .collect();
        Ok(plan)
    } else {
        Err("Automatic install is only supported on macOS and Windows. Install yt-dlp and ffmpeg with your package manager.".into())
    }
}

#[tauri::command]
async fn install_tools(app: AppHandle, tools: Vec<String>) -> Result<(), String> {
    if tools.is_empty() {
        return Ok(());
    }
    let plan = build_install_plan(&tools)?;

    for (cmd, args) in plan {
        app.emit(
            "install-log",
            format!("$ {} {}", cmd.file_name().and_then(|s| s.to_str()).unwrap_or("installer"), args.join(" ")),
        )
        .ok();

        let mut child = Command::new(&cmd)
            .args(&args)
            .env("PATH", augmented_path())
            // brew refuses to run if it thinks it's non-interactive in odd ways;
            // NONINTERACTIVE makes it proceed without prompting.
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("NONINTERACTIVE", "1")
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
            let msg = "The installer reported an error. See the log above.".to_string();
            app.emit("install-error", msg.clone()).ok();
            return Err(msg);
        }
    }

    app.emit("install-done", ()).ok();
    Ok(())
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
            download,
            cancel_download,
            reveal_in_folder,
            detect_installers,
            install_tools
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
