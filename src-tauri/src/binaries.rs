// Binary resolution, PATH discovery (macOS/Linux + Windows Registry),
// OS-standard app data directory, and legacy file migration.

use crate::process::std_cmd;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

pub(crate) const APP_ID: &str = "com.fetch.downloader"; // must match tauri.conf.json "identifier"

pub(crate) fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// OS-standard app-owned data directory:
/// - macOS: `~/Library/Application Support/com.fetch.downloader`
/// - Windows: `%LOCALAPPDATA%\com.fetch.downloader`
/// - Linux: `$XDG_DATA_HOME/com.fetch.downloader` or `~/.local/share/com.fetch.downloader`
pub(crate) fn app_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        dirs_home()
            .map(|h| h.join("Library").join("Application Support").join(APP_ID))
            .unwrap_or_else(|| PathBuf::from(".").join(APP_ID))
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| dirs_home().map(|h| h.join("AppData").join("Local")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_ID)
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs_home().map(|h| h.join(".local").join("share")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join(APP_ID)
    }
}

/// App-owned folder that holds our own copies of yt-dlp/ffmpeg/spotdl.
/// Nothing is installed system-wide, so uninstalling FETCH just means
/// deleting its own folders — no leftover PATH entries or registry keys.
pub(crate) fn portable_bin_dir() -> PathBuf {
    app_data_dir().join("bin")
}

/// One-time best-effort migration from legacy paths used by earlier versions:
/// - macOS `~/com.fetch.downloader/bin` -> `~/Library/Application Support/com.fetch.downloader/bin`
/// - `~/Downloads/Fetch/cookies.txt` -> `<app_data_dir>/cookies.txt`
pub(crate) fn migrate_legacy_files() {
    #[cfg(target_os = "macos")]
    if let Some(home) = dirs_home() {
        let legacy_root = home.join(APP_ID);
        let legacy_bin = legacy_root.join("bin");
        if legacy_bin.is_dir() {
            let new_bin = portable_bin_dir();
            if std::fs::create_dir_all(&new_bin).is_ok() {
                if let Ok(rd) = std::fs::read_dir(&legacy_bin) {
                    for entry in rd.flatten() {
                        let name = entry.file_name();
                        let src = entry.path();
                        let dst = new_bin.join(&name);
                        if dst.exists() || dst.symlink_metadata().is_ok() {
                            continue;
                        }
                        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
                            if let Ok(target) = std::fs::read_link(&src) {
                                let new_target = if target.starts_with(&legacy_bin) {
                                    if let Ok(rel) = target.strip_prefix(&legacy_bin) {
                                        new_bin.join(rel)
                                    } else {
                                        target
                                    }
                                } else {
                                    target
                                };
                                let _ = std::os::unix::fs::symlink(&new_target, &dst);
                                let _ = std::fs::remove_file(&src);
                            }
                        } else {
                            let _ = std::fs::rename(&src, &dst);
                        }
                    }
                }
                // Fix venv shebang if spotdl-venv was migrated via pip fallback.
                let new_venv_script = new_bin.join("spotdl-venv").join("bin").join("spotdl");
                if new_venv_script.is_file() {
                    if let Ok(text) = std::fs::read_to_string(&new_venv_script) {
                        let old_prefix = format!("#!{}", legacy_bin.join("spotdl-venv").display());
                        let new_prefix = format!("#!{}", new_bin.join("spotdl-venv").display());
                        if text.starts_with(&old_prefix) {
                            let updated = text.replacen(&old_prefix, &new_prefix, 1);
                            let _ = std::fs::write(&new_venv_script, updated);
                        }
                    }
                }
                let _ = std::fs::remove_dir(&legacy_bin);
                let _ = std::fs::remove_dir(&legacy_root);
            }
        }
    }

    let legacy_cookies = PathBuf::from(default_download_dir()).join("cookies.txt");
    let new_cookies = app_data_dir().join("cookies.txt");
    if legacy_cookies.is_file() && !new_cookies.exists() {
        if let Some(parent) = new_cookies.parent() {
            if std::fs::create_dir_all(parent).is_ok()
                && std::fs::rename(&legacy_cookies, &new_cookies).is_err()
                && std::fs::copy(&legacy_cookies, &new_cookies).is_ok()
            {
                let _ = std::fs::remove_file(&legacy_cookies);
            }
        }
    }
}

pub(crate) fn extra_bin_dirs() -> Vec<PathBuf> {
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

/// Windows caches PATH in each process's environment block at creation time.
/// Read the live value straight from the registry so tools installed by
/// winget/choco after app launch are still picked up.
#[cfg(windows)]
fn live_path_dirs() -> Vec<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    fn read(key: &RegKey, subkey: &str) -> Option<String> {
        key.open_subkey(subkey)
            .ok()?
            .get_value::<String, _>("Path")
            .ok()
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
    if let Some(raw) = read(&RegKey::predef(HKEY_CURRENT_USER), "Environment") {
        dirs.extend(std::env::split_paths(&expand(&raw)));
    }
    if let Some(raw) = read(
        &RegKey::predef(HKEY_LOCAL_MACHINE),
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

type HealthCache = HashMap<PathBuf, (u64, Option<SystemTime>, bool)>;

fn health_cache() -> &'static Mutex<HealthCache> {
    static CACHE: OnceLock<Mutex<HealthCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Drops any cached health status for `path` (called after installing/updating
/// a binary in place).
pub(crate) fn invalidate_binary_health(path: &Path) {
    health_cache().lock().unwrap().remove(path);
}

/// Verifies that a discovered `yt-dlp` / `ffmpeg` / `spotdl` binary actually
/// starts and exits 0 (catching e.g. a broken Homebrew `ffmpeg` whose dylibs
/// were half-upgraded). Result is cached by `(path, file_size, mtime)`.
fn is_binary_healthy(path: &Path, name: &str) -> bool {
    if !matches!(name, "yt-dlp" | "ffmpeg" | "spotdl") {
        return true;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let len = meta.len();
    let mtime = meta.modified().ok();
    if let Some(&(c_len, c_mtime, ok)) = health_cache().lock().unwrap().get(path) {
        if c_len == len && c_mtime == mtime {
            return ok;
        }
    }
    let arg = if name == "ffmpeg" { "-version" } else { "--version" };
    let ok = std_cmd(path)
        .arg(arg)
        .env("PATH", augmented_path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    health_cache()
        .lock()
        .unwrap()
        .insert(path.to_path_buf(), (len, mtime, ok));
    ok
}

/// Resolve a binary to an absolute path if we can find it in PATH or a known dir.
pub(crate) fn resolve_binary(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    for dir in extra_bin_dirs() {
        let cand = dir.join(&exe);
        if cand.is_file() && is_binary_healthy(&cand, name) {
            return Some(cand);
        }
    }
    for dir in live_path_dirs() {
        let cand = dir.join(&exe);
        if cand.is_file() && is_binary_healthy(&cand, name) {
            return Some(cand);
        }
    }
    None
}

/// PATH string that includes our extra dirs, so yt-dlp can locate ffmpeg.
pub(crate) fn augmented_path() -> String {
    let mut parts: Vec<PathBuf> = extra_bin_dirs();
    parts.extend(live_path_dirs());
    std::env::join_paths(parts)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub(crate) fn ytdlp() -> Result<PathBuf, String> {
    resolve_binary("yt-dlp").ok_or_else(|| {
        "yt-dlp not found. Use FETCH's installer, or `brew install yt-dlp`, then restart FETCH.".into()
    })
}

pub(crate) fn spotdl() -> Result<PathBuf, String> {
    resolve_binary("spotdl").ok_or_else(|| {
        "spotDL isn't installed yet — it's what lets FETCH grab Spotify links. Install it from the prompt above (or Settings), then try again.".into()
    })
}

/// Turns a failed `spawn()` of the spotDL binary into an actionable message.
/// A raw os error 86 on macOS means the binary's CPU architecture doesn't
/// match this machine.
pub(crate) fn spotdl_launch_error(e: std::io::Error, bin: &Path) -> String {
    if e.raw_os_error() == Some(86) {
        let _ = std::fs::remove_file(bin);
        "spotDL's binary doesn't run on this Mac's CPU. Reinstall it from the prompt (or Settings) — \
         it'll detect that and fall back to a pip install automatically."
            .to_string()
    } else {
        format!("Failed to launch spotDL: {e}")
    }
}

#[derive(PartialEq, Debug)]
pub(crate) enum Source {
    Spotify,
    YtDlp,
}

pub(crate) fn url_source(url: &str) -> Source {
    let u = url.trim().to_ascii_lowercase();
    if u.starts_with("spotify:") || u.contains("spotify.com") {
        Source::Spotify
    } else {
        Source::YtDlp
    }
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Binaries {
    yt_dlp: bool,
    ffmpeg: bool,
    spotdl: bool,
    yt_dlp_path: Option<String>,
    ffmpeg_path: Option<String>,
    spotdl_path: Option<String>,
}

#[tauri::command]
pub fn check_binaries() -> Binaries {
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
pub fn default_download_dir() -> String {
    let base = dirs_home().unwrap_or_else(|| PathBuf::from("."));
    base.join("Downloads").join("Fetch").to_string_lossy().into_owned()
}
