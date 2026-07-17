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
use std::time::SystemTime;
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

fn spotdl() -> Result<PathBuf, String> {
    resolve_binary("spotdl").ok_or_else(|| {
        "spotDL isn't installed yet — it's what lets FETCH grab Spotify links. Install it from the prompt above (or Settings), then try again.".into()
    })
}

/// Turns a failed `spawn()` of the spotDL binary into an actionable message.
/// A raw os error 86 on macOS means the binary's CPU architecture doesn't
/// match this machine (e.g. an arm64-only release asset on an Intel Mac) —
/// the portable binary is unusable, so delete it: reinstalling from the
/// prompt will detect the same mismatch and fall back to `pip install
/// spotdl` automatically instead of failing the same way on every retry.
fn spotdl_launch_error(e: std::io::Error, bin: &Path) -> String {
    if e.raw_os_error() == Some(86) {
        let _ = std::fs::remove_file(bin);
        "spotDL's binary doesn't run on this Mac's CPU. Reinstall it from the prompt (or Settings) — \
         it'll detect that and fall back to a pip install automatically."
            .to_string()
    } else {
        format!("Failed to launch spotDL: {e}")
    }
}

/// Which downloader handles a URL. Spotify streams are DRM-protected, so
/// yt-dlp can't touch them; those links go to spotDL instead, which reads the
/// track/album/playlist metadata from Spotify, finds the closest match on
/// YouTube Music, downloads that, and tags it with Spotify's metadata + cover.
/// Everything else (YouTube, SoundCloud, Vimeo, TikTok, …) is a native yt-dlp
/// extractor and takes the unchanged yt-dlp path.
#[derive(PartialEq)]
enum Source {
    Spotify,
    YtDlp,
}

fn url_source(url: &str) -> Source {
    let u = url.trim().to_ascii_lowercase();
    if u.starts_with("spotify:") || u.contains("spotify.com") {
        Source::Spotify
    } else {
        Source::YtDlp
    }
}

// ── data shapes shared with the frontend ─────────────────────────────────

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct Binaries {
    yt_dlp: bool,
    ffmpeg: bool,
    spotdl: bool,
    yt_dlp_path: Option<String>,
    ffmpeg_path: Option<String>,
    spotdl_path: Option<String>,
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
    // Spotify only: the individual track URL. yt-dlp playlists select entries
    // by index (`--playlist-items`), but spotDL has no index-range option, so
    // for Spotify we download the exact track URLs the user ticked instead.
    url: Option<String>,
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
// "file" | anything else (treated as no cookies). Cookies are never read
// live from a running browser here — see `import_cookies_from_browser`,
// which snapshots them into this file once so downloads don't each need
// the browser closed.
fn cookie_args(mode: Option<&str>, file: Option<&str>) -> Vec<String> {
    match mode {
        Some("file") => match file.filter(|s| !s.is_empty()) {
            Some(f) => vec!["--cookies".into(), f.to_string()],
            None => vec![],
        },
        _ => vec![],
    }
}

// ── cookie import from browser ───────────────────────────────────────────
// Reading cookies straight from a running browser on every yt-dlp call
// means asking the user to quit their browser before every single
// download. Instead, `import_cookies_from_browser` snapshots the browser's
// cookies into a Netscape-format file once (closing/reopening the browser
// itself if needed), and `cookie_args` "file" mode reads that afterwards —
// same as if the user had exported it by hand.

/// yt-dlp browser id -> the app's display/process name, used for the UI
/// label and to find/quit/reopen the running app (macOS).
fn browser_app_name(browser: &str) -> Option<&'static str> {
    match browser {
        "chrome" => Some("Google Chrome"),
        "edge" => Some("Microsoft Edge"),
        "firefox" => Some("Firefox"),
        "brave" => Some("Brave Browser"),
        "opera" => Some("Opera"),
        "vivaldi" => Some("Vivaldi"),
        "safari" => Some("Safari"),
        _ => None,
    }
}

/// yt-dlp browser id -> Windows process image name.
#[cfg(target_os = "windows")]
fn browser_process_image(browser: &str) -> Option<&'static str> {
    match browser {
        "chrome" => Some("chrome.exe"),
        "edge" => Some("msedge.exe"),
        "firefox" => Some("firefox.exe"),
        "brave" => Some("brave.exe"),
        "opera" => Some("opera.exe"),
        "vivaldi" => Some("vivaldi.exe"),
        _ => None, // Safari doesn't ship on Windows
    }
}

#[cfg(target_os = "macos")]
fn pgrep_running(name: &str) -> bool {
    std::process::Command::new("pgrep")
        .args(["-x", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn is_browser_running(browser: &str) -> bool {
    browser_app_name(browser).map(pgrep_running).unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn is_browser_running(browser: &str) -> bool {
    let Some(image) = browser_process_image(browser) else {
        return false;
    };
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains(&image.to_lowercase()))
        .unwrap_or(false)
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn is_browser_running(_browser: &str) -> bool {
    false
}

/// Asks the browser to quit (a normal graceful quit, same as Cmd+Q /
/// closing every window) and waits up to 8s for it to actually exit, so
/// the cookie database file isn't still held open. Best-effort: if it's
/// still running after the timeout, the extraction step below will just
/// surface that as a "still in use" error.
#[cfg(target_os = "macos")]
fn quit_browser(browser: &str) -> Result<(), String> {
    let app_name = browser_app_name(browser).ok_or_else(|| format!("Unsupported browser: {browser}"))?;
    let output = std::process::Command::new("osascript")
        .args(["-e", &format!("quit app \"{app_name}\"")])
        .output()
        .map_err(|e| format!("Could not send quit to {app_name}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("-1743") || stderr.to_lowercase().contains("not authorized") {
            return Err(format!(
                "FETCH needs permission to control {app_name}. Grant it in System Settings → Privacy & Security → Automation, then try again."
            ));
        }
        return Err(format!("Could not close {app_name}: {}", stderr.trim()));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    while std::time::Instant::now() < deadline && pgrep_running(app_name) {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn quit_browser(browser: &str) -> Result<(), String> {
    if let Some(image) = browser_process_image(browser) {
        let _ = std::process::Command::new("taskkill").args(["/IM", image]).output();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        while std::time::Instant::now() < deadline && is_browser_running(browser) {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    Ok(())
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn quit_browser(_browser: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn reopen_browser(browser: &str) {
    if let Some(app_name) = browser_app_name(browser) {
        let _ = std::process::Command::new("open").args(["-a", app_name]).spawn();
    }
}

#[cfg(target_os = "windows")]
fn reopen_browser(browser: &str) {
    if let Some(image) = browser_process_image(browser) {
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", image]).spawn();
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn reopen_browser(_browser: &str) {}

fn cookies_file_path() -> PathBuf {
    Path::new(&default_download_dir()).join("cookies.txt")
}

/// Turns a raw yt-dlp/OS failure into one of the specific, actionable
/// messages the cookie-import UI shows, instead of a generic traceback.
fn classify_cookie_extraction_error(stderr: &str, label: &str) -> String {
    let lower = stderr.to_lowercase();
    if lower.contains("database is locked") || lower.contains("could not copy") {
        format!("{label}'s cookie database is still in use. Quit {label} completely and try again.")
    } else if lower.contains("keychain") || lower.contains("-25293") || lower.contains("-128") || lower.contains("decrypt") {
        format!(
            "macOS blocked access to {label}'s saved cookies. Open Keychain Access, find \"{label} Safe Storage\", remove any existing entry for FETCH, then try again and choose \"Always Allow\" when asked."
        )
    } else if lower.contains("could not find") || lower.contains("no such file") || lower.contains("does not exist") {
        format!("Could not find {label}'s cookie data on this computer. Make sure {label} is installed and you've visited at least one site in it.")
    } else {
        clean_ytdlp_error(stderr)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CookieImportResult {
    path: String,
    cookie_count: usize,
    warning: Option<String>,
}

#[tauri::command]
fn browser_is_running(browser: String) -> bool {
    is_browser_running(&browser)
}

/// Snapshots cookies from `browser` into `cookies_file_path()`, closing and
/// reopening the browser around the extraction if it's currently running.
/// `probe_url` (the link the user is actually trying to download, when
/// available) is only used to run yt-dlp against something real and to
/// check the right site's cookies actually came through — the extraction
/// itself always grabs every cookie the browser has, not just that site's.
#[tauri::command]
async fn import_cookies_from_browser(
    id: String,
    jobs: State<'_, Jobs>,
    cancelled: State<'_, CancelledIds>,
    browser: String,
    probe_url: Option<String>,
) -> Result<CookieImportResult, String> {
    let label = browser_app_name(&browser)
        .ok_or_else(|| format!("Unsupported browser: {browser}"))?
        .to_string();

    let was_running = is_browser_running(&browser);
    if was_running {
        // `quit_browser` shells out (osascript on macOS) and can block for a
        // while if e.g. a macOS Automation permission alert is waiting for a
        // click — run it off the async runtime and cap how long we'll wait,
        // so a stuck system dialog can't hang the whole import forever.
        let browser_for_quit = browser.clone();
        match tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::task::spawn_blocking(move || quit_browser(&browser_for_quit)),
        )
        .await
        {
            Err(_) => {
                cancelled.0.lock().unwrap().remove(&id);
                return Err(format!(
                    "Timed out asking {label} to close — check for a permission popup (Automation access) behind other windows, then try again."
                ));
            }
            Ok(join_result) => {
                if let Err(e) = join_result.map_err(|e| format!("Internal error closing {label}: {e}")) {
                    cancelled.0.lock().unwrap().remove(&id);
                    return Err(e);
                }
            }
        }
    }

    // Closing the browser can take a while (up to the 15s above); if STOP
    // was clicked during that stretch, there was no process yet to kill —
    // bail out now instead of still going on to spawn yt-dlp.
    let was_cancelled = cancelled.0.lock().unwrap().remove(&id);
    let result = if was_cancelled {
        Err("Cancelled.".to_string())
    } else {
        extract_cookies(&id, &jobs, &cancelled, &browser, &label, probe_url).await
    };
    cancelled.0.lock().unwrap().remove(&id);

    if was_running {
        reopen_browser(&browser);
    }

    result
}

/// Lets the "STOP" button in the import popup kill an in-flight cookie
/// extraction. Marks `id` cancelled unconditionally (checked at each phase
/// boundary above/below) and, if a child process is already registered,
/// kills it immediately too — same mechanism as `cancel_analyze`.
#[tauri::command]
fn cancel_cookie_import(jobs: State<'_, Jobs>, cancelled: State<'_, CancelledIds>, id: String) -> Result<(), String> {
    cancelled.0.lock().unwrap().insert(id.clone());
    if let Some(mut child) = jobs.0.lock().unwrap().remove(&id) {
        let _ = child.start_kill();
    }
    Ok(())
}

async fn extract_cookies(
    id: &str,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    browser: &str,
    label: &str,
    probe_url: Option<String>,
) -> Result<CookieImportResult, String> {
    let bin = ytdlp()?;
    let out_path = cookies_file_path();
    std::fs::create_dir_all(out_path.parent().unwrap())
        .map_err(|e| format!("Cannot create {}: {e}", out_path.display()))?;

    let url = probe_url
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "https://www.youtube.com/watch?v=BaW_jenozKc".to_string());
    let out_path_str = out_path.to_string_lossy().into_owned();

    // `Command::spawn()` itself can block for a long stretch here (the same
    // macOS re-verification tax as everywhere else yt-dlp runs) with no
    // process yet to register/kill — run it on a blocking thread so a STOP
    // click during that window is at least noticed the moment it returns,
    // rather than only after the extraction that follows.
    let mut child = tokio::task::spawn_blocking({
        let bin = bin.clone();
        let browser = browser.to_string();
        let out_path_str = out_path_str.clone();
        let url = url.clone();
        move || {
            Command::new(&bin)
                .args([
                    "--cookies-from-browser",
                    &browser,
                    "--cookies",
                    &out_path_str,
                    "--skip-download",
                    "--simulate",
                    "--no-warnings",
                    &url,
                ])
                .env("PATH", augmented_path())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
        }
    })
    .await
    .map_err(|e| format!("Internal error launching yt-dlp: {e}"))?
    .map_err(|e| format!("Failed to launch yt-dlp: {e}"))?;

    if cancelled.0.lock().unwrap().contains(id) {
        let _ = child.start_kill();
        return Err("Cancelled.".to_string());
    }

    // Drain both pipes concurrently so a chatty stdout can't block the
    // child on a full pipe buffer while we're only reading stderr.
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    if let Some(mut stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
        });
    }

    // Register the child under `id` so `cancel_cookie_import` can steal it
    // out of the map and kill it — same pattern `analyze()` uses below. If
    // STOP landed in the narrow gap between the check above and this
    // insert, catch it here too instead of leaving an unkillable orphan.
    jobs.0.lock().unwrap().insert(id.to_string(), child);
    if cancelled.0.lock().unwrap().contains(id) {
        if let Some(mut child) = jobs.0.lock().unwrap().remove(id) {
            let _ = child.start_kill();
        }
        return Err("Cancelled.".to_string());
    }

    // yt-dlp launches can be slow to even start on a cold macOS security
    // scan of the portable binary, and a hidden Keychain prompt can block
    // it indefinitely. There's a manual STOP button for that, but if
    // nobody's watching, still cap the wait so this can't hang forever.
    let stderr_buf = match tokio::time::timeout(std::time::Duration::from_secs(180), stderr_handle).await {
        Ok(joined) => joined.unwrap_or_default(),
        Err(_) => {
            if let Some(mut child) = jobs.0.lock().unwrap().remove(id) {
                let _ = child.start_kill();
            }
            return Err(format!(
                "Timed out after 3 minutes waiting for yt-dlp to read {label}'s cookies. If a macOS Keychain permission popup is hidden behind another window, approve it and try again."
            ));
        }
    };

    // The child closing its stderr pipe (above) means it's either exited or
    // been killed — reclaim it from `jobs` to reap the exit status. If it's
    // no longer there, `cancel_cookie_import` got to it first.
    let mut child = jobs
        .0
        .lock()
        .unwrap()
        .remove(id)
        .ok_or_else(|| "Cancelled.".to_string())?;
    let _ = child.wait().await;

    // yt-dlp writes the cookiejar out on exit regardless of whether the
    // probe URL itself resolved, so a real (non-empty) file means the
    // browser extraction succeeded even if the process exit code didn't.
    let size = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        let stderr = String::from_utf8_lossy(&stderr_buf);
        return Err(classify_cookie_extraction_error(&stderr, label));
    }

    let content = std::fs::read_to_string(&out_path).unwrap_or_default();
    let cookie_count = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .count();

    let domain_hint = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .map(|h| h.trim_start_matches("www.").to_string());
    let warning = match &domain_hint {
        Some(d) if !d.is_empty() && !content.contains(d.as_str()) => Some(format!(
            "Imported {cookie_count} cookies from {label}, but none matched {d} — make sure you're logged into that site in {label}."
        )),
        _ => None,
    };

    Ok(CookieImportResult {
        path: out_path_str,
        cookie_count,
        warning,
    })
}

// ── commands ─────────────────────────────────────────────────────────────

#[tauri::command]
fn check_binaries() -> Binaries {
    let yt = resolve_binary("yt-dlp");
    let ff = resolve_binary("ffmpeg");
    let sp = resolve_binary("spotdl");
    Binaries {
        yt_dlp: yt.is_some(),
        ffmpeg: ff.is_some(),
        spotdl: sp.is_some(),
        yt_dlp_path: yt.map(|p| p.to_string_lossy().into_owned()),
        ffmpeg_path: ff.map(|p| p.to_string_lossy().into_owned()),
        spotdl_path: sp.map(|p| p.to_string_lossy().into_owned()),
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
    cookie_file: Option<String>,
) -> Result<AnalyzeResult, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("Empty link.".into());
    }
    if url_source(&url) == Source::Spotify {
        return analyze_spotify(id, jobs, url).await;
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
    args.extend(cookie_args(cookie_mode.as_deref(), cookie_file.as_deref()));
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

// ── Spotify (spotDL) analyze ──────────────────────────────────────────────
// `spotdl save <url> --save-file <f>` writes a JSON array of song objects
// (one per track for an album/playlist) with Spotify's own metadata. We turn
// that into the same AnalyzeResult the yt-dlp path produces, so the frontend
// renders it through the identical preview / quality-picker / playlist UI —
// just audio-only (no video tiers), since a Spotify link is always audio.

async fn analyze_spotify(
    id: String,
    jobs: State<'_, Jobs>,
    url: String,
) -> Result<AnalyzeResult, String> {
    let bin = spotdl()?;
    let tmp = std::env::temp_dir().join(format!(
        "fetch-spotdl-{}-{}.spotdl",
        std::process::id(),
        Local::now().format("%H%M%S%3f")
    ));

    let args: Vec<String> = vec![
        "save".into(),
        url.clone(),
        "--save-file".into(),
        tmp.to_string_lossy().into_owned(),
    ];

    let mut child = Command::new(&bin)
        .args(&args)
        .env("PATH", augmented_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| spotdl_launch_error(e, &bin))?;

    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;

    // register for cancellation (a fresh analyze kills this one via cancel_analyze)
    jobs.0.lock().unwrap().insert(id.clone(), child);

    // Drain both pipes so spotDL never blocks writing to a full one.
    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let mut stdout_buf = Vec::new();
    let _ = stdout.read_to_end(&mut stdout_buf).await;

    let mut child = jobs
        .0
        .lock()
        .unwrap()
        .remove(&id)
        .ok_or("Analysis was cancelled.")?;
    let status = child
        .wait()
        .await
        .map_err(|e| format!("spotDL did not exit cleanly: {e}"))?;
    let stderr_buf = stderr_handle.await.unwrap_or_default();

    // spotDL writes the metadata to the save-file, not stdout. Read it back,
    // then clean up regardless of outcome.
    let file = std::fs::read_to_string(&tmp);
    let _ = std::fs::remove_file(&tmp);

    let json: serde_json::Value = match file {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("Could not parse spotDL output: {e}"))?,
        Err(_) => {
            let err = String::from_utf8_lossy(&stderr_buf);
            return Err(if status.success() {
                "spotDL couldn't read anything from that Spotify link.".to_string()
            } else {
                clean_spotdl_error(&err)
            });
        }
    };

    build_spotify_result(&json, &url)
}

fn clean_spotdl_error(raw: &str) -> String {
    // spotDL doesn't prefix errors like yt-dlp's "ERROR:", so surface the last
    // line that looks like an error rather than the whole log.
    for line in raw.lines().rev() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if l.contains("Error")
            || l.contains("error")
            || l.contains("No results")
            || l.contains("not found")
        {
            return l.to_string();
        }
    }
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        "spotDL could not read that Spotify link.".to_string()
    } else {
        trimmed.lines().last().unwrap_or("Unknown error").to_string()
    }
}

/// The audio tiers offered for every source. ORIGINAL keeps the source codec
/// untouched (no re-encode → truly best quality, smallest file); M4A extracts
/// to AAC; the MP3 tiers re-encode at a fixed bitrate (for device
/// compatibility — note the source is already lossy, so a higher MP3 bitrate
/// only means a bigger file, not more real quality). The `id` encodes the
/// choice ("original" | "m4a" | "mp3-320" | …) and the downloader (yt-dlp or
/// spotDL) maps it to the right flags. Sizes are estimated only when we know
/// the source audio size + duration (a single yt-dlp video); playlists and
/// Spotify pass None and just omit the estimate.
fn audio_tiers(duration: Option<f64>, best_audio_bytes: Option<u64>) -> Vec<QualityOption> {
    let mp3_bytes = |kbps: u64| duration.map(|d| (kbps as f64 * 1000.0 / 8.0 * d) as u64);
    let mk = |id: &str, label: &str, bytes: Option<u64>, sel: &str| QualityOption {
        id: id.into(),
        label: label.into(),
        height: None,
        approx_bytes: bytes,
        format_selector: sel.into(),
        kind: "audio".into(),
    };
    vec![
        mk("original", "ORIGINAL · BEST", best_audio_bytes, "ba/b"),
        mk("m4a", "M4A · BEST", best_audio_bytes, "ba[ext=m4a]/ba/b"),
        mk("mp3-320", "MP3 · 320K", mp3_bytes(320), "ba/b"),
        mk("mp3-256", "MP3 · 256K", mp3_bytes(256), "ba/b"),
        mk("mp3-192", "MP3 · 192K", mp3_bytes(192), "ba/b"),
        mk("mp3-128", "MP3 · 128K", mp3_bytes(128), "ba/b"),
    ]
}

/// Audio tiers for a Spotify link — same list, but spotDL exposes no per-format
/// sizes so all estimates are None. The `format_selector` is unused for spotDL
/// (it downloads by `--format`/`--bitrate`, mapped from the tier `id`).
fn spotify_audio_options() -> Vec<QualityOption> {
    audio_tiers(None, None)
}

fn build_spotify_result(json: &serde_json::Value, url: &str) -> Result<AnalyzeResult, String> {
    let songs = json
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or("No tracks found for that Spotify link.")?;

    let str_at = |s: &serde_json::Value, k: &str| {
        s.get(k).and_then(|v| v.as_str()).map(|v| v.to_string())
    };
    let artist_of = |s: &serde_json::Value| {
        str_at(s, "artist").or_else(|| {
            s.get("artists")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|v| v.to_string())
        })
    };
    // spotDL stores duration in seconds, but guard in case a build reports ms.
    let dur_of = |s: &serde_json::Value| {
        s.get("duration").and_then(|v| v.as_f64()).map(|d| if d > 86_400.0 { d / 1000.0 } else { d })
    };

    // A single track → the plain "video" (audio) preview screen; an
    // album/playlist → the playlist screen with per-track selection.
    if songs.len() == 1 {
        let s = &songs[0];
        return Ok(AnalyzeResult {
            kind: "video".to_string(),
            webpage_url: str_at(s, "url").unwrap_or_else(|| url.to_string()),
            title: str_at(s, "name").unwrap_or_else(|| "Untitled".to_string()),
            uploader: artist_of(s),
            duration_seconds: dur_of(s),
            view_count: None,
            thumbnail: str_at(s, "cover_url"),
            extractor: "Spotify".to_string(),
            video_options: vec![],
            // Duration lets the MP3 tiers show a size estimate (bitrate × time);
            // ORIGINAL/M4A stay blank since spotDL doesn't expose the source size.
            audio_options: audio_tiers(dur_of(s), None),
            entries: vec![],
            playlist_count: None,
        });
    }

    let entries: Vec<PlaylistEntry> = songs
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let name = str_at(s, "name").unwrap_or_else(|| format!("Track {}", i + 1));
            let title = match artist_of(s) {
                Some(a) => format!("{a} — {name}"),
                None => name,
            };
            PlaylistEntry {
                index: s.get("list_position").and_then(|v| v.as_u64()).unwrap_or((i as u64) + 1),
                title,
                duration_seconds: dur_of(s),
                thumbnail: str_at(s, "cover_url"),
                url: str_at(s, "url"),
            }
        })
        .collect();

    let title = str_at(&songs[0], "list_name")
        .or_else(|| str_at(&songs[0], "album_name"))
        .unwrap_or_else(|| "Spotify Playlist".to_string());

    Ok(AnalyzeResult {
        kind: "playlist".to_string(),
        webpage_url: url.to_string(),
        title,
        uploader: None,
        duration_seconds: None,
        view_count: None,
        thumbnail: str_at(&songs[0], "cover_url"),
        extractor: "Spotify".to_string(),
        video_options: vec![],
        audio_options: spotify_audio_options(),
        playlist_count: Some(entries.len() as u64),
        entries,
    })
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
                            url: None,
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
    (video, audio_tiers(None, None))
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

    let audio = audio_tiers(
        duration,
        if best_audio_bytes > 0 { Some(best_audio_bytes) } else { None },
    );

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
    #[serde(default)]
    quality_id: String,    // the picked QualityOption.id (e.g. "1080p", "original") — stamped into the filename so re-downloading at a different quality doesn't collide with an earlier one
    output_dir: String,
    write_thumbnail: bool, // save the thumbnail as its own image file
    write_description: bool, // save the description to a .description text file
    write_subs: bool,      // save subtitles as their own .srt file
    playlist_items: Option<String>, // e.g. "1,3,4-6" for playlist selections
    cookie_mode: Option<String>,
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
    // Actual bytes on disk (summed across every file this job wrote), so the
    // frontend can show the real size instead of the pre-download estimate.
    filesize: Option<u64>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ErrorPayload {
    id: String,
    message: String,
}

#[derive(Default)]
struct Jobs(Mutex<HashMap<String, Child>>);

/// Ids the user has asked to cancel via `cancel_cookie_import`, checked at
/// every phase boundary of `import_cookies_from_browser`. Needed because the
/// yt-dlp child isn't registered in `Jobs` (and so isn't killable) for the
/// first stretch of the operation — closing the browser, and even just
/// `Command::spawn()` itself, can each block for many seconds (macOS
/// re-verifying the portable binary) before there's any process to kill.
#[derive(Default)]
struct CancelledIds(Mutex<std::collections::HashSet<String>>);

/// Turns a picked QualityOption.id into the short, filename-safe label
/// stamped in front of the title (e.g. "1080p - My Video.mp4"). Keeps
/// same-day re-downloads at a different quality from colliding with an
/// earlier file instead of overwriting it.
fn quality_stamp(id: &str) -> String {
    match id {
        "" => String::new(),
        "best" => "Best".into(),
        "original" => "Original".into(),
        "m4a" => "M4A".into(),
        _ => match id.strip_prefix("mp3-") {
            Some(kbps) => format!("MP3 {kbps}"),
            None => id.into(), // "1080p", "720p", "2160p", ... already filename-safe
        },
    }
}

#[tauri::command]
async fn download(
    app: AppHandle,
    jobs: State<'_, Jobs>,
    req: DownloadRequest,
) -> Result<(), String> {
    if url_source(&req.url) == Source::Spotify {
        return download_spotify(app, jobs, req).await;
    }
    let bin = ytdlp()?;
    std::fs::create_dir_all(&req.output_dir)
        .map_err(|e| format!("Cannot create download folder: {e}"))?;

    // Every video gets its own folder, keyed only on its title (no date
    // prefix) so re-downloading the same video later reuses the same folder
    // instead of spawning a new one per day — the OS's own "date modified"
    // already tells you when a folder was last touched. For playlists
    // yt-dlp fills in %(title)s per entry, so each video still lands in its
    // own folder rather than being pooled together. The quality is stamped
    // onto the filename itself so re-downloading the same video at a
    // different quality lands next to the earlier file instead of colliding
    // with it.
    let stamp = quality_stamp(&req.quality_id);
    let filename = if stamp.is_empty() {
        "%(title)s.%(ext)s".to_string()
    } else {
        format!("{stamp} - %(title)s.%(ext)s")
    };
    let out_tmpl = format!(
        "{}/%(title)s/{filename}",
        req.output_dir.trim_end_matches('/')
    );

    let mut args: Vec<String> = vec![
        req.url.clone(),
        "-f".into(),
        req.format_selector.clone(),
        "-o".into(),
        out_tmpl,
        // Re-downloading the same title/quality combo already on disk is
        // skipped rather than overwritten.
        "--no-overwrites".into(),
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
    args.extend(cookie_args(req.cookie_mode.as_deref(), req.cookie_file.as_deref()));

    if req.kind == "audio" {
        let af = req.audio_format.as_deref().unwrap_or("original");
        args.push("-x".into());
        if af == "original" {
            // No --audio-format → yt-dlp keeps the source codec (opus/m4a) and
            // doesn't re-encode: highest fidelity, smallest file.
        } else if let Some(kbps) = af.strip_prefix("mp3-") {
            args.push("--audio-format".into());
            args.push("mp3".into());
            // Force the label's bitrate for real (K suffix = constant bitrate).
            args.push("--audio-quality".into());
            args.push(format!("{kbps}K"));
        } else {
            // "m4a" (or a legacy "mp3" with no bitrate) → extract to that codec.
            args.push("--audio-format".into());
            args.push(af.into());
        }
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
        // Real size on disk = sum of every media file this job produced (one
        // per video for a playlist), rather than the pre-download estimate.
        let total_bytes: u64 = all_paths
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
            .sum();
        app.emit(
            "dl-done",
            DonePayload {
                id: id.clone(),
                filepath: final_path,
                filesize: (total_bytes > 0).then_some(total_bytes),
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

// ── Spotify (spotDL) download ─────────────────────────────────────────────
// spotDL matches each Spotify track to YouTube Music, downloads it, and
// embeds Spotify's tags + cover. It has no machine-parsable byte-level
// progress like yt-dlp's --progress-template, so progress here is coarse:
// one step per track finished (i/N), which is fine for audio (each track is
// quick). Emits the same dl-progress / dl-done / dl-error events as the
// yt-dlp path, so the frontend drives it through the identical download UI.
async fn download_spotify(
    app: AppHandle,
    jobs: State<'_, Jobs>,
    req: DownloadRequest,
) -> Result<(), String> {
    let bin = spotdl()?;
    std::fs::create_dir_all(&req.output_dir)
        .map_err(|e| format!("Cannot create download folder: {e}"))?;

    let started = SystemTime::now();
    // Same "title/quality - title.ext" convention as the yt-dlp path, but
    // with spotDL's own template placeholders ({title}, {output-ext}).
    let stamp = quality_stamp(&req.quality_id);
    let filename = if stamp.is_empty() {
        "{title}.{output-ext}".to_string()
    } else {
        format!("{stamp} - {{title}}.{{output-ext}}")
    };
    let out_tmpl = format!(
        "{}/{{title}}/{filename}",
        req.output_dir.trim_end_matches('/')
    );

    // A playlist/album download passes the exact track URLs the user ticked
    // (newline-joined by the frontend); a single track just uses req.url.
    let mut queries: Vec<String> = Vec::new();
    if let Some(items) = &req.playlist_items {
        for line in items.split('\n') {
            let t = line.trim();
            if !t.is_empty() {
                queries.push(t.to_string());
            }
        }
    }
    if queries.is_empty() {
        queries.push(req.url.clone());
    }
    let total = queries.len();

    // Map the shared tier id onto spotDL's --format / --bitrate. "original"
    // keeps the native YouTube codec (opus) with no bitrate re-encode; the mp3
    // tiers force their bitrate; m4a extracts to AAC.
    let id = req.audio_format.as_deref().unwrap_or("original");
    let (fmt, bitrate): (&str, Option<String>) = if id == "original" {
        ("opus", None)
    } else if let Some(kbps) = id.strip_prefix("mp3-") {
        ("mp3", Some(format!("{kbps}k")))
    } else if id == "m4a" {
        ("m4a", None)
    } else {
        ("mp3", None)
    };
    let mut args: Vec<String> = vec!["download".into()];
    args.extend(queries);
    args.push("--output".into());
    args.push(out_tmpl);
    args.push("--format".into());
    args.push(fmt.into());
    if let Some(b) = bitrate {
        args.push("--bitrate".into());
        args.push(b);
    }
    // Don't re-download something already there.
    args.push("--overwrite".into());
    args.push("skip".into());
    // Point spotDL at our copy of ffmpeg so it doesn't depend on a system one.
    if let Some(ff) = resolve_binary("ffmpeg") {
        args.push("--ffmpeg".into());
        args.push(ff.to_string_lossy().into_owned());
    }
    // Reuse the "subtitle file" toggle as "save synced lyrics (.lrc)".
    if req.write_subs {
        args.push("--generate-lrc".into());
    }

    let mut child = Command::new(&bin)
        .args(&args)
        .env("PATH", augmented_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| spotdl_launch_error(e, &bin))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;

    jobs.0.lock().unwrap().insert(req.id.clone(), child);
    let id = req.id.clone();

    // kick the UI into its "downloading" state right away
    app.emit(
        "dl-progress",
        ProgressPayload {
            id: id.clone(),
            percent: 0.0,
            speed: format!("0/{total} tracks"),
            eta: String::new(),
            stage: "downloading".into(),
        },
    )
    .ok();

    let err_app = app.clone();
    let err_id = id.clone();
    let stderr_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut buf = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            buf.push_str(&line);
            buf.push('\n');
            let _ = err_app.emit("dl-log", (err_id.clone(), line));
        }
        buf
    });

    let mut done_count = 0usize;
    let mut reader = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = reader.next_line().await {
        let l = line.trim_start();
        // spotDL prints one of these per track as it finishes.
        if l.starts_with("Downloaded ") || l.starts_with("Skipping ") {
            done_count += 1;
            let percent = if total > 0 {
                (done_count as f64 / total as f64 * 100.0).min(100.0)
            } else {
                0.0
            };
            app.emit(
                "dl-progress",
                ProgressPayload {
                    id: id.clone(),
                    percent,
                    speed: format!("{done_count}/{total} tracks"),
                    eta: String::new(),
                    stage: "downloading".into(),
                },
            )
            .ok();
        }
        let _ = app.emit("dl-log", (id.clone(), line));
    }

    let mut child = jobs
        .0
        .lock()
        .unwrap()
        .remove(&id)
        .ok_or("job was cancelled")?;
    let status = child
        .wait()
        .await
        .map_err(|e| format!("spotDL did not exit cleanly: {e}"))?;
    let err_text = stderr_handle.await.unwrap_or_default();

    // spotDL exits 0 even when a track couldn't be matched, so "did anything
    // actually land on disk?" is the real success signal. `done_count > 0`
    // also covers a re-download where every track was skipped (files already
    // present, so nothing is newer than `started`).
    let (produced, total_bytes) = find_media_since(Path::new(&req.output_dir), started);

    if status.success() && (produced.is_some() || done_count > 0) {
        app.emit(
            "dl-done",
            DonePayload {
                id: id.clone(),
                filepath: produced.map(|p| p.to_string_lossy().into_owned()),
                filesize: (total_bytes > 0).then_some(total_bytes),
            },
        )
        .ok();
        Ok(())
    } else {
        let message = if !status.success() {
            if err_text.trim().is_empty() {
                "spotDL download failed.".to_string()
            } else {
                clean_spotdl_error(&err_text)
            }
        } else {
            "No matching track was found on YouTube for this Spotify link.".to_string()
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

/// Audio files under `dir` (a few levels deep) modified at or after `since` —
/// used to recover what spotDL just wrote, since it doesn't print the final
/// file path in a parsable form the way yt-dlp does. Returns the newest such
/// file (for "reveal in folder") and the total bytes across all of them (for
/// the real size, e.g. every track of a playlist).
fn find_media_since(dir: &Path, since: SystemTime) -> (Option<PathBuf>, u64) {
    const EXTS: [&str; 7] = ["mp3", "m4a", "opus", "flac", "ogg", "wav", "aac"];
    fn walk(
        dir: &Path,
        since: SystemTime,
        depth: u8,
        best: &mut Option<(SystemTime, PathBuf)>,
        total: &mut u64,
    ) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if depth < 3 {
                    walk(&path, since, depth + 1, best, total);
                }
                continue;
            }
            let is_media = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|e| EXTS.contains(&e.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            if !is_media {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.modified().map(|m| m >= since).unwrap_or(false) {
                *total += meta.len();
                if best.as_ref().map(|(t, _)| meta.modified().map(|m| m > *t).unwrap_or(false)).unwrap_or(true) {
                    if let Ok(m) = meta.modified() {
                        *best = Some((m, path));
                    }
                }
            }
        }
    }
    let mut best = None;
    let mut total = 0u64;
    walk(dir, since, 0, &mut best, &mut total);
    (best.map(|(_, p)| p), total)
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
    /// Whether `brew` itself resolves right now — checked upfront so the
    /// frontend can point at the Homebrew install flow *before* the user
    /// hits "Install" and gets a mid-install failure, rather than only
    /// finding out reactively from an install-error.
    brew_available: bool,
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
        brew_available: cfg!(target_os = "macos") && resolve_binary("brew").is_some(),
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
    run_streamed_command(app, cmd, "Homebrew reported an error. See the log above.").await
}

/// Runs `cmd`, streaming its stdout/stderr to the frontend as `install-log`
/// events as they arrive (used for both Homebrew and pip installs), and
/// returns `fail_msg` if it exits non-zero.
async fn run_streamed_command(app: &AppHandle, mut cmd: Command, fail_msg: &str) -> Result<(), String> {
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
        return Err(fail_msg.to_string());
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

/// Runs `dest --version` right after a download so an architecture mismatch
/// (e.g. an arm64-only release asset landing on an Intel Mac, or vice versa)
/// is caught immediately as an install error instead of surfacing later, mid
/// analyze/download, as a bare "Bad CPU type in executable (os error 86)".
/// Returns the raw spawn error so callers can tell an arch mismatch (os error
/// 86) apart from anything else and react differently (e.g. fall back to pip).
#[cfg(target_os = "macos")]
async fn check_mac_exec(dest: &Path) -> std::io::Result<()> {
    Command::new(dest).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().await.map(|_| ())
}

/// Parses "Python 3.9.6" (the format `python3 --version` prints to stdout
/// since Python 3.4) into `(major, minor)`.
#[cfg(target_os = "macos")]
fn parse_python_version(s: &str) -> Option<(u32, u32)> {
    let rest = s.strip_prefix("Python ")?;
    let mut parts = rest.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Installs spotDL via pip when the portable binary can't run on this Mac's
/// CPU — the upstream project only publishes one macOS asset per release, so
/// if it doesn't match our architecture there's no other portable download
/// to fall back to. `pip` builds/pulls a wheel for whatever Python is
/// actually running, sidestepping the architecture problem entirely.
#[cfg(target_os = "macos")]
async fn pip_install_spotdl(app: &AppHandle) -> Result<PathBuf, String> {
    let mut python = resolve_binary("python3");

    // spotDL's yt-dlp dependency hard-errors on Python < 3.10 ("Support for
    // Python version 3.9 has been deprecated"), which would otherwise turn a
    // "successful" pip install into a binary that fails on every real
    // download. macOS ships a 3.9 python3 via the Xcode Command Line Tools —
    // and on a machine where those tools were never installed, that same
    // path is a non-functional placeholder that prints nothing and exits
    // rather than reporting a version — so treat "too old" and "unusable"
    // the same way: get (or upgrade to) a real 3.10+ via Homebrew.
    let usable = match &python {
        Some(p) => {
            let ver_str = Command::new(p)
                .arg("--version")
                .output()
                .await
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default();
            matches!(parse_python_version(&ver_str), Some((major, minor)) if (major, minor) >= (3, 10))
        }
        None => false,
    };

    if !usable {
        let brew = resolve_binary("brew").ok_or(
            "spotDL needs Python 3.10+, which isn't installed, and Homebrew isn't either — so FETCH \
             can't get one automatically. Install Python (e.g. from https://python.org) or Homebrew, \
             then try again.",
        )?;
        app.emit("install-log", "Installing a newer Python via Homebrew (spotDL needs 3.10+)…").ok();
        let mut cmd = Command::new(&brew);
        cmd.args(["install", "python3"]).env("PATH", augmented_path()).env("HOMEBREW_NO_AUTO_UPDATE", "1");
        run_streamed_command(app, cmd, "Homebrew couldn't install python3. See the log above.").await?;
        python = resolve_binary("python3");
    }

    let python = python.ok_or("Couldn't locate python3 after installing it via Homebrew.")?;

    // A plain `pip install --user` fails on Homebrew's Python with "error:
    // externally-managed-environment" (PEP 668) — Homebrew's site-packages
    // refuses pip installs outside a virtual environment. A private venv
    // under FETCH's own bin dir sidesteps that entirely and keeps spotDL's
    // dependency tree self-contained, the same way yt-dlp/ffmpeg are already
    // sandboxed there instead of touching the system Python.
    let venv_dir = portable_bin_dir().join("spotdl-venv");
    let mut cmd = Command::new(&python);
    cmd.args(["-m", "venv", "--clear"]).arg(&venv_dir);
    run_streamed_command(app, cmd, "Couldn't create a Python environment for spotDL. See the log above.").await?;

    let mut cmd = Command::new(venv_dir.join("bin").join("pip"));
    cmd.args(["install", "--upgrade", "spotdl"]);
    run_streamed_command(
        app,
        cmd,
        "pip install spotdl failed. See the log above, or run `pip install spotdl` yourself in a terminal.",
    )
    .await?;

    let script = venv_dir.join("bin").join("spotdl");
    if !script.is_file() {
        return Err(format!("spotDL installed via pip, but the script wasn't found at {}.", script.display()));
    }
    Ok(script)
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

            app.emit("install-log", "Verifying yt-dlp runs on this Mac…").ok();
            if let Err(e) = check_mac_exec(&dest).await {
                let _ = std::fs::remove_file(&dest);
                return Err(format!(
                    "Downloaded yt-dlp but it won't run on this Mac's CPU ({}): {e}",
                    std::env::consts::ARCH
                ));
            }

            app.emit("install-log", format!("Saved to {}", dest.display())).ok();
        } else if tool == "spotdl" {
            let bin_dir = portable_bin_dir();
            std::fs::create_dir_all(&bin_dir)
                .map_err(|e| format!("Couldn't create {}: {e}", bin_dir.display()))?;
            let dest = bin_dir.join("spotdl");

            app.emit("install-log", "Resolving latest spotDL release…").ok();
            let (_, url) = spotdl_release().await?;
            app.emit("install-log", "Downloading spotDL (portable binary)…").ok();
            mac_curl_download(&url, &dest).await?;

            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&dest).map_err(|e| e.to_string())?.permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dest, perms).map_err(|e| e.to_string())?;

            app.emit("install-log", "Verifying spotDL runs on this Mac…").ok();
            if let Err(e) = check_mac_exec(&dest).await {
                let _ = std::fs::remove_file(&dest);
                // Upstream only ships one macOS asset per release, so an
                // arch mismatch (os error 86) has no other portable download
                // to fall back to — go through pip instead, which builds/
                // pulls a wheel for whatever Python is actually installed.
                if e.raw_os_error() == Some(86) {
                    app.emit(
                        "install-log",
                        format!(
                            "spotDL's binary doesn't run on this Mac's CPU ({}) — the latest release only \
                             ships one for the other architecture. Falling back to `pip install spotdl`…",
                            std::env::consts::ARCH
                        ),
                    )
                    .ok();
                    let script = pip_install_spotdl(app).await?;
                    std::os::unix::fs::symlink(&script, &dest)
                        .map_err(|e| format!("Couldn't link spotDL: {e}"))?;
                    app.emit("install-log", format!("Linked spotDL (via pip) at {}", dest.display())).ok();
                } else {
                    return Err(format!("Downloaded spotDL but couldn't run it: {e}"));
                }
            } else {
                app.emit("install-log", format!("Saved to {}", dest.display())).ok();
            }
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
            "spotdl" => {
                app.emit("install-log", "Resolving latest spotDL release…").ok();
                let (_, url) = spotdl_release().await?;
                app.emit("install-log", "Downloading spotdl.exe…").ok();
                curl_download(&url, &bin_dir.join("spotdl.exe")).await?;
                app.emit("install-log", format!("Saved to {}", bin_dir.join("spotdl.exe").display())).ok();
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
    spotdl: ToolUpdate,
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

/// Latest spotDL release tag + the download URL of the standalone binary for
/// this platform. spotDL ships PyInstaller-built executables (no Python
/// needed) whose asset names embed the version and OS/arch, so we can't guess
/// a stable `latest/download/…` URL — we resolve it from the releases API.
async fn spotdl_release() -> Result<(String, String), String> {
    let body =
        curl_get("https://api.github.com/repos/spotDL/spotify-downloader/releases/latest").await?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let assets = json
        .get("assets")
        .and_then(|v| v.as_array())
        .ok_or("Unexpected response from GitHub")?;

    let want = if cfg!(windows) {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        return Err("spotDL only ships portable binaries for Windows and macOS. Install it with `pip install spotdl`.".to_string());
    };

    let arch = std::env::consts::ARCH; // "aarch64" | "x86_64"
    let mut fallback: Option<String> = None;
    for asset in assets {
        let name = asset.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let url = asset
            .get("browser_download_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if url.is_empty() || !name.contains(want) {
            continue;
        }
        if cfg!(windows) {
            return Ok((tag, url.to_string()));
        }
        // macOS: prefer an asset matching this arch, else fall back to any
        // darwin build (older releases shipped a single x86_64 binary that
        // runs under Rosetta on Apple Silicon).
        let arch_match = match arch {
            "aarch64" => name.contains("arm64") || name.contains("aarch64"),
            _ => !name.contains("arm64") && !name.contains("aarch64"),
        };
        if arch_match {
            return Ok((tag, url.to_string()));
        }
        fallback.get_or_insert_with(|| url.to_string());
    }
    fallback
        .map(|u| (tag, u))
        .ok_or_else(|| "Couldn't find a spotDL download for this platform in the latest release.".to_string())
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
    if let Some(path) = resolve_binary("spotdl") {
        out.spotdl.source = tool_source(&path).to_string();
        out.spotdl.current = binary_version_output(&path, "--version")
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

    if !out.spotdl.source.is_empty() {
        if let Ok((tag, _)) = spotdl_release().await {
            let latest = tag.trim_start_matches('v').to_string();
            out.spotdl.update_available =
                out.spotdl.current.as_deref().map(|c| c != latest).unwrap_or(false);
            out.spotdl.latest = Some(latest);
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
        // "other": found on PATH but not installed by us (a system copy from
        // winget/choco/pip/etc.). On Windows we don't touch it — we drop our
        // own portable copy into portable_bin_dir(), which resolve_binary()
        // and augmented_path() both search *first*, so it shadows the system
        // one and the update takes effect. Elsewhere we can't do that safely,
        // so we tell the user to update it themselves (no Homebrew wording on
        // platforms that don't have Homebrew).
        _ => {
            #[cfg(windows)]
            {
                install_tools_portable(&app, std::slice::from_ref(&tool)).await
            }
            #[cfg(target_os = "macos")]
            {
                Err(format!("{tool} wasn't installed by FETCH or Homebrew — update it manually."))
            }
            #[cfg(all(not(windows), not(target_os = "macos")))]
            {
                Err(format!("{tool} wasn't installed by FETCH — update it with your package manager."))
            }
        }
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
        .manage(CancelledIds::default())
        .invoke_handler(tauri::generate_handler![
            check_binaries,
            default_download_dir,
            browser_is_running,
            import_cookies_from_browser,
            cancel_cookie_import,
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
