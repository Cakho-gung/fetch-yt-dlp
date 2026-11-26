// Browser cookie snapshot extraction into an app-owned Netscape cookies.txt.

use crate::analyze::clean_ytdlp_error;
use crate::binaries::{app_data_dir, ytdlp};
use crate::process::{
    cancel_job, is_cancelled, reclaim_job, register_job, std_cmd, tokio_cmd, CancelledIds,
    JobGuard, Jobs,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tauri::State;
use tokio::io::AsyncReadExt;

pub(crate) fn cookies_file_path() -> PathBuf {
    app_data_dir().join("cookies.txt")
}

/// Builds the yt-dlp cookie flags for the current settings.
/// If a stored legacy path (e.g. `~/Downloads/Fetch/cookies.txt`) no longer
/// exists after migration, transparently falls back to `cookies_file_path()`.
pub(crate) fn cookie_args(mode: Option<&str>, file: Option<&str>) -> Vec<String> {
    if mode != Some("file") {
        return vec![];
    }
    let migrated = cookies_file_path();
    let resolved = match file.filter(|s| !s.is_empty()) {
        Some(f) if Path::new(f).is_file() => f.to_string(),
        _ if migrated.is_file() => migrated.to_string_lossy().into_owned(),
        Some(f) => f.to_string(),
        None => return vec![],
    };
    vec!["--cookies".into(), resolved]
}

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

#[cfg(target_os = "windows")]
fn browser_process_image(browser: &str) -> Option<&'static str> {
    match browser {
        "chrome" => Some("chrome.exe"),
        "edge" => Some("msedge.exe"),
        "firefox" => Some("firefox.exe"),
        "brave" => Some("brave.exe"),
        "opera" => Some("opera.exe"),
        "vivaldi" => Some("vivaldi.exe"),
        _ => None,
    }
}

#[cfg(target_os = "macos")]
fn pgrep_running(name: &str) -> bool {
    std_cmd("pgrep")
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
    std_cmd("tasklist")
        .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains(&image.to_lowercase()))
        .unwrap_or(false)
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn is_browser_running(_browser: &str) -> bool {
    false
}

#[cfg(target_os = "macos")]
fn quit_browser(browser: &str) -> Result<(), String> {
    let app_name = browser_app_name(browser).ok_or_else(|| format!("Unsupported browser: {browser}"))?;
    let output = std_cmd("osascript")
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
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline && pgrep_running(app_name) {
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn quit_browser(browser: &str) -> Result<(), String> {
    if let Some(image) = browser_process_image(browser) {
        let _ = std_cmd("taskkill").args(["/IM", image]).output();
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline && is_browser_running(browser) {
            std::thread::sleep(Duration::from_millis(250));
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
        let _ = std_cmd("open").args(["-a", app_name]).spawn();
    }
}

#[cfg(target_os = "windows")]
fn reopen_browser(browser: &str) {
    if let Some(image) = browser_process_image(browser) {
        let _ = std_cmd("cmd").args(["/C", "start", "", image]).spawn();
    }
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn reopen_browser(_browser: &str) {}

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

/// Returns the Chromium User Data root directory for `browser` so we can
/// locate multi-profile cookies (`Default`, `Profile 1`, `Profile 2`, ...).
fn chromium_user_data_dir(browser: &str) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let app_support = crate::binaries::dirs_home()?
            .join("Library")
            .join("Application Support");
        let sub = match browser {
            "chrome" => "Google/Chrome",
            "edge" => "Microsoft Edge",
            "brave" => "BraveSoftware/Brave-Browser",
            "vivaldi" => "Vivaldi",
            "opera" => "com.operasoftware.Opera",
            _ => return None,
        };
        Some(app_support.join(sub))
    }
    #[cfg(windows)]
    {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| crate::binaries::dirs_home().map(|h| h.join("AppData").join("Local")))?;
        let sub = match browser {
            "chrome" => r"Google\Chrome\User Data",
            "edge" => r"Microsoft\Edge\User Data",
            "brave" => r"BraveSoftware\Brave-Browser\User Data",
            "vivaldi" => r"Vivaldi\User Data",
            _ => return None,
        };
        Some(local.join(sub))
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        let _ = browser;
        None
    }
}

/// Resolves `--cookies-from-browser` argument, auto-detecting the most recently
/// active Chromium profile (e.g. `vivaldi:Profile 1`) instead of blindly
/// assuming `Default`.
pub(crate) fn resolve_browser_profile_arg(browser: &str, domain: Option<&str>) -> String {
    let Some(root) = chromium_user_data_dir(browser) else {
        return browser.to_string();
    };
    let Ok(rd) = std::fs::read_dir(&root) else {
        return browser.to_string();
    };
    let needle: Option<Vec<u8>> = domain
        .map(|d| d.trim_start_matches("www.").as_bytes().to_vec())
        .filter(|n| !n.is_empty());

    // (profile has the domain, cookie db size, name) — prefer profiles that
    // actually hold cookies for the site, then the one with the most cookies.
    let mut best: Option<(bool, u64, String)> = None;
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name != "Default" && !name.starts_with("Profile ") {
            continue;
        }
        let Some(db) = ["Network/Cookies", "Cookies"]
            .iter()
            .map(|rel| p.join(rel))
            .find(|cand| cand.is_file())
        else {
            continue;
        };
        let Ok(meta) = std::fs::metadata(&db) else {
            continue;
        };
        if meta.len() == 0 {
            continue;
        }
        let has_domain = match (&needle, meta.len() <= 64 * 1024 * 1024) {
            (Some(n), true) => std::fs::read(&db)
                .map(|bytes| bytes.windows(n.len()).any(|w| w == n.as_slice()))
                .unwrap_or(false),
            _ => false,
        };
        let better = match &best {
            None => true,
            Some((bd, bl, _)) => (has_domain, meta.len()) > (*bd, *bl),
        };
        if better {
            best = Some((has_domain, meta.len(), name.to_string()));
        }
    }

    match best {
        Some((_, _, profile)) => format!("{browser}:{profile}"),
        None => browser.to_string(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CookieImportResult {
    path: String,
    cookie_count: usize,
    warning: Option<String>,
}

#[tauri::command]
pub fn browser_is_running(browser: String) -> bool {
    is_browser_running(&browser)
}

#[tauri::command]
pub async fn import_cookies_from_browser(
    id: String,
    jobs: State<'_, Jobs>,
    cancelled: State<'_, CancelledIds>,
    browser: String,
    probe_url: Option<String>,
) -> Result<CookieImportResult, String> {
    let label = browser_app_name(&browser)
        .ok_or_else(|| format!("Unsupported browser: {browser}"))?
        .to_string();

    let _guard = JobGuard::new(&id, &jobs, &cancelled);

    let was_running = is_browser_running(&browser);
    if was_running {
        let browser_for_quit = browser.clone();
        match tokio::time::timeout(
            Duration::from_secs(15),
            tokio::task::spawn_blocking(move || quit_browser(&browser_for_quit)),
        )
        .await
        {
            Err(_) => {
                return Err(format!(
                    "Timed out asking {label} to close — check for a permission popup (Automation access) behind other windows, then try again."
                ));
            }
            Ok(Err(e)) => {
                return Err(format!("Internal error closing {label}: {e}"));
            }
            Ok(Ok(Err(e))) => {
                return Err(e);
            }
            Ok(Ok(Ok(()))) => {}
        }
    }

    let result = if is_cancelled(&cancelled, &id) {
        Err("Cancelled.".to_string())
    } else {
        extract_cookies(&id, &jobs, &cancelled, &browser, &label, probe_url).await
    };

    if was_running {
        reopen_browser(&browser);
    }

    result
}

#[tauri::command]
pub fn cancel_cookie_import(
    jobs: State<'_, Jobs>,
    cancelled: State<'_, CancelledIds>,
    id: String,
) -> Result<(), String> {
    cancel_job(&jobs, &cancelled, &id);
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
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Cannot create {}: {e}", out_path.display()))?;
    }

    // Write to a fresh .part file so a failed extraction never reports a
    // stale cookies.txt from a previous run as a false success.
    let tmp_path = out_path.with_file_name("cookies.txt.part");
    let _ = std::fs::remove_file(&tmp_path);

    let clean_probe = probe_url.filter(|u| !u.trim().is_empty());
    let domain_hint: Option<String> = clean_probe.as_deref().and_then(|u| {
        u.split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .map(|h| h.trim_start_matches("www.").to_string())
    });
    let browser_spec = resolve_browser_profile_arg(browser, domain_hint.as_deref());
    let tmp_path_str = tmp_path.to_string_lossy().into_owned();
    let out_path_str = out_path.to_string_lossy().into_owned();

    let mut child = tokio::task::spawn_blocking({
        let bin = bin.clone();
        let browser_spec = browser_spec.clone();
        let tmp_path_str = tmp_path_str.clone();
        move || {
            // `--flat-playlist --playlist-items 0` against a dummy loopback
            // URL lets yt-dlp initialize the browser cookiejar and flush it
            // to `--cookies` on exit in ~0.3s without running slow video extractors.
            tokio_cmd(&bin)
                .args([
                    "--cookies-from-browser",
                    &browser_spec,
                    "--cookies",
                    &tmp_path_str,
                    "--skip-download",
                    "--simulate",
                    "--flat-playlist",
                    "--playlist-items",
                    "0",
                    "http://127.0.0.1:1",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
        }
    })
    .await
    .map_err(|e| format!("Internal error launching yt-dlp: {e}"))?
    .map_err(|e| format!("Failed to launch yt-dlp: {e}"))?;

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

    register_job(jobs, cancelled, id, child)?;

    let stderr_buf = match tokio::time::timeout(Duration::from_secs(180), stderr_handle).await {
        Ok(joined) => joined.unwrap_or_default(),
        Err(_) => {
            if let Some(mut child) = jobs.0.lock().unwrap().remove(id) {
                let _ = child.start_kill();
            }
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!(
                "Timed out after 3 minutes waiting for yt-dlp to read {label}'s cookies. If a macOS Keychain permission popup is hidden behind another window, approve it and try again."
            ));
        }
    };

    let mut child = reclaim_job(jobs, id, "Cancelled.")?;
    let _ = child.wait().await;

    let size = std::fs::metadata(&tmp_path).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        let _ = std::fs::remove_file(&tmp_path);
        let stderr = String::from_utf8_lossy(&stderr_buf);
        return Err(classify_cookie_extraction_error(&stderr, label));
    }

    let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
    let cookie_count = content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .count();

    if cookie_count == 0 {
        let _ = std::fs::remove_file(&tmp_path);
        let stderr = String::from_utf8_lossy(&stderr_buf);
        if !stderr.trim().is_empty() {
            return Err(classify_cookie_extraction_error(&stderr, label));
        }
        return Err(format!(
            "No cookies were found in {label}. Make sure you're logged into sites in {label} and try again."
        ));
    }

    if out_path.exists() {
        let _ = std::fs::remove_file(&out_path);
    }
    std::fs::rename(&tmp_path, &out_path)
        .map_err(|e| format!("Could not save cookies file: {e}"))?;

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
