// In-app tool installation (portable static binaries + pip fallback for spotDL)
// and version/update checking.

use crate::binaries::{invalidate_binary_health, portable_bin_dir, resolve_binary};
use crate::process::{
    cancel_job, is_cancelled, read_line_or_cr, reclaim_job, register_job, std_cmd, strip_ansi,
    tokio_cmd, CancelledIds, InstallGuard, InstallLock, JobGuard, Jobs,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::Command;

const INSTALL_JOB_ID: &str = "__install__";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Installers {
    available: bool,
    manager: String, // "mac" | "fetch" | ""
    platform: String,
    brew_available: bool,
}

#[tauri::command]
pub fn detect_installers() -> Installers {
    let platform = std::env::consts::OS.to_string();
    let (available, manager) = if cfg!(target_os = "macos") {
        (true, "mac")
    } else if cfg!(windows) {
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
pub async fn install_tools(
    app: AppHandle,
    jobs: State<'_, Jobs>,
    cancelled: State<'_, CancelledIds>,
    install_lock: State<'_, InstallLock>,
    tools: Vec<String>,
) -> Result<(), String> {
    if tools.is_empty() {
        return Ok(());
    }
    let _lock = InstallGuard::acquire(&install_lock)?;
    let _job = JobGuard::new(INSTALL_JOB_ID, &jobs, &cancelled);

    #[cfg(target_os = "macos")]
    let result = install_tools_macos(&app, &jobs, &cancelled, INSTALL_JOB_ID, &tools).await;

    #[cfg(windows)]
    let result = install_tools_portable(&app, &jobs, &cancelled, INSTALL_JOB_ID, &tools).await;

    #[cfg(all(not(target_os = "macos"), not(windows)))]
    let result: Result<(), String> = Err(
        "Automatic install is only supported on macOS and Windows. Install yt-dlp and ffmpeg with your package manager."
            .to_string(),
    );

    if let Err(msg) = &result {
        app.emit("install-error", msg.clone()).ok();
        return result;
    }
    app.emit("install-done", ()).ok();
    Ok(())
}

/// Ensures `tool` (`"ffmpeg"` or `"yt-dlp"`) is installed/updated into
/// `portable_bin_dir()` using `job_id` so the caller (e.g. `download()`) can
/// self-heal broken/missing/outdated tools while remaining cancelable.
pub(crate) async fn ensure_tool_installed(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
    tool: &str,
) -> Result<(), String> {
    let tools = [tool.to_string()];
    #[cfg(target_os = "macos")]
    {
        install_tools_macos(app, jobs, cancelled, job_id, &tools).await
    }
    #[cfg(windows)]
    {
        install_tools_portable(app, jobs, cancelled, job_id, &tools).await
    }
    #[cfg(all(not(target_os = "macos"), not(windows)))]
    {
        let _ = (app, jobs, cancelled, job_id, tools);
        Err(format!("Automatic install of {tool} is only supported on macOS and Windows."))
    }
}

#[tauri::command]
pub fn cancel_install(jobs: State<'_, Jobs>, cancelled: State<'_, CancelledIds>) -> Result<(), String> {
    cancel_job(&jobs, &cancelled, INSTALL_JOB_ID);
    Ok(())
}

async fn run_streamed_command(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
    mut cmd: Command,
    fail_msg: &str,
) -> Result<(), String> {
    if is_cancelled(cancelled, job_id) {
        return Err("Cancelled.".to_string());
    }
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to launch installer: {e}"))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    register_job(jobs, cancelled, job_id, child)?;

    let a2 = app.clone();
    let err_handle = tokio::spawn(async move {
        let mut r = BufReader::new(stderr);
        let mut buf = Vec::new();
        while let Ok(Some(l)) = read_line_or_cr(&mut r, &mut buf).await {
            let clean = strip_ansi(&l);
            if !clean.trim().is_empty() {
                a2.emit("install-log", clean).ok();
            }
        }
    });
    let mut r = BufReader::new(stdout);
    let mut buf = Vec::new();
    while let Ok(Some(l)) = read_line_or_cr(&mut r, &mut buf).await {
        let clean = strip_ansi(&l);
        if !clean.trim().is_empty() {
            app.emit("install-log", clean).ok();
        }
    }
    let _ = err_handle.await;

    let mut child = reclaim_job(jobs, job_id, "Cancelled.")?;
    let status = child
        .wait()
        .await
        .map_err(|e| format!("Installer did not exit cleanly: {e}"))?;
    if !status.success() {
        return Err(fail_msg.to_string());
    }
    Ok(())
}

/// Temp path for atomic downloads (`yt-dlp.exe.part`, `yt-dlp.part`) so
/// `with_extension("part")` never strips `.exe` on Windows.
pub(crate) fn part_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    dest.with_file_name(format!("{name}.part"))
}

/// Extracts the percentage number from a `curl -#` progress line (e.g. `#### 42.5%`).
fn parse_curl_percent(line: &str) -> Option<u32> {
    let pct_idx = line.find('%')?;
    let prefix = line[..pct_idx].trim();
    let num_str = prefix.split_whitespace().last()?;
    let val = num_str.parse::<f64>().ok()?;
    Some(val.clamp(0.0, 100.0).floor() as u32)
}

/// Shared cancelable `curl` download with milestone progress logs (every 20%)
/// writing atomically via `<dest>.part`.
async fn curl_download_with_bin(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
    curl: &Path,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    if is_cancelled(cancelled, job_id) {
        return Err("Cancelled.".to_string());
    }
    let tmp = part_path(dest);
    let _ = std::fs::remove_file(&tmp);

    let mut child = tokio_cmd(curl)
        .args([
            "-L",
            "-#",
            "--fail",
            "--connect-timeout",
            "15",
            "--speed-time",
            "30",
            "--speed-limit",
            "1000",
            "-o",
        ])
        .arg(&tmp)
        .arg(url)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to launch curl: {e}"))?;

    let stderr = child.stderr.take().ok_or("no stderr")?;
    register_job(jobs, cancelled, job_id, child)?;

    let mut r = BufReader::new(stderr);
    let mut buf = Vec::new();
    let mut last_bucket: i32 = -1;
    let mut err_tail = String::new();
    while let Ok(Some(line)) = read_line_or_cr(&mut r, &mut buf).await {
        let clean = strip_ansi(&line);
        let trimmed = clean.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(pct) = parse_curl_percent(trimmed) {
            let bucket = (pct / 20) as i32;
            if bucket > last_bucket && pct > 0 {
                last_bucket = bucket;
                let shown = (bucket * 20).min(100);
                app.emit("install-log", format!("  … {shown}%")).ok();
            }
        } else if !trimmed.starts_with('#') {
            err_tail = trimmed.to_string();
        }
    }

    let mut child = match reclaim_job(jobs, job_id, "Cancelled.") {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    };
    let status = child
        .wait()
        .await
        .map_err(|e| format!("curl did not exit cleanly: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        let detail = if err_tail.is_empty() {
            String::new()
        } else {
            format!(" ({err_tail})")
        };
        return Err(format!("Download failed: {url}{detail}"));
    }
    if dest.exists() {
        let _ = std::fs::remove_file(dest);
    }
    std::fs::rename(&tmp, dest).map_err(|e| format!("Couldn't save {}: {e}", dest.display()))?;
    invalidate_binary_health(dest);
    Ok(())
}

#[cfg(target_os = "macos")]
async fn mac_curl_download(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    let curl = resolve_binary("curl")
        .ok_or("curl not found. It ships with macOS — check it hasn't been removed.")?;
    curl_download_with_bin(app, jobs, cancelled, job_id, &curl, url, dest).await
}

#[cfg(target_os = "macos")]
fn clear_mac_quarantine(dest: &Path) {
    let _ = std_cmd("xattr").args(["-cr"]).arg(dest).output();
}

#[cfg(target_os = "macos")]
async fn check_mac_exec(dest: &Path, version_arg: &str) -> std::io::Result<()> {
    let fut = tokio_cmd(dest)
        .arg(version_arg)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match tokio::time::timeout(Duration::from_secs(20), fut).await {
        Ok(res) => res.map(|_| ()),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Timed out verifying downloaded binary",
        )),
    }
}

#[cfg(target_os = "macos")]
fn parse_python_version(s: &str) -> Option<(u32, u32)> {
    let rest = s.strip_prefix("Python ")?;
    let mut parts = rest.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

#[cfg(target_os = "macos")]
async fn pip_install_spotdl(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
) -> Result<PathBuf, String> {
    let mut python = resolve_binary("python3");

    let usable = match &python {
        Some(p) => {
            let ver_str = tokio_cmd(p)
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
        app.emit(
            "install-log",
            "Installing a newer Python via Homebrew (spotDL needs 3.10+)…",
        )
        .ok();
        let mut cmd = tokio_cmd(&brew);
        cmd.args(["install", "python3"])
            .env("HOMEBREW_NO_AUTO_UPDATE", "1");
        run_streamed_command(
            app,
            jobs,
            cancelled,
            job_id,
            cmd,
            "Homebrew couldn't install python3. See the log above.",
        )
        .await?;
        python = resolve_binary("python3");
    }

    let python = python.ok_or("Couldn't locate python3 after installing it via Homebrew.")?;

    let venv_dir = portable_bin_dir().join("spotdl-venv");
    let mut cmd = tokio_cmd(&python);
    cmd.args(["-m", "venv", "--clear"]).arg(&venv_dir);
    run_streamed_command(
        app,
        jobs,
        cancelled,
        job_id,
        cmd,
        "Couldn't create a Python environment for spotDL. See the log above.",
    )
    .await?;

    let mut cmd = tokio_cmd(venv_dir.join("bin").join("pip"));
    cmd.args(["install", "--upgrade", "spotdl"]);
    run_streamed_command(
        app,
        jobs,
        cancelled,
        job_id,
        cmd,
        "pip install spotdl failed. See the log above, or run `pip install spotdl` yourself in a terminal.",
    )
    .await?;

    let script = venv_dir.join("bin").join("spotdl");
    if !script.is_file() {
        return Err(format!(
            "spotDL installed via pip, but the script wasn't found at {}.",
            script.display()
        ));
    }
    Ok(script)
}

#[cfg(target_os = "macos")]
fn set_mac_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).map_err(|e| e.to_string())?;
    clear_mac_quarantine(path);
    invalidate_binary_health(path);
    Ok(())
}

/// Downloads a pre-built static macOS `ffmpeg` or `ffprobe` binary.
/// - `x86_64` (Intel): tries `evermeet.cx` zip first (latest release), falls back to GitHub `ffmpeg-static` `.gz`.
/// - `aarch64` (Apple Silicon): tries GitHub `ffmpeg-static` native `arm64` `.gz` first, falls back to `evermeet.cx`.
#[cfg(target_os = "macos")]
async fn download_mac_ffmpeg_binary(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
    name: &str,
    dest: &Path,
) -> Result<(), String> {
    let arch = std::env::consts::ARCH;
    let gh_arch = if arch == "aarch64" { "arm64" } else { "x64" };
    let gh_url = format!(
        "https://github.com/eugeneware/ffmpeg-static/releases/latest/download/{name}-darwin-{gh_arch}.gz"
    );
    let evermeet_url = format!("https://evermeet.cx/ffmpeg/getrelease/{name}/zip");

    let tmp_dir = std::env::temp_dir().join(format!("fetch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;

    let try_evermeet = async {
        app.emit("install-log", format!("Downloading {name} (macOS static zip)…"))
            .ok();
        let zip_path = tmp_dir.join(format!("{name}.zip"));
        mac_curl_download(app, jobs, cancelled, job_id, &evermeet_url, &zip_path).await?;

        app.emit("install-log", format!("Extracting {name}…")).ok();
        let status = tokio_cmd("/usr/bin/ditto")
            .args(["-x", "-k"])
            .arg(&zip_path)
            .arg(&tmp_dir)
            .status()
            .await
            .map_err(|e| format!("Failed to extract {name}: {e}"))?;
        if !status.success() {
            return Err(format!("Failed to extract {name}.zip"));
        }
        let extracted = tmp_dir.join(name);
        if !extracted.is_file() {
            return Err(format!("{name} binary not found inside archive"));
        }
        if dest.exists() {
            let _ = std::fs::remove_file(dest);
        }
        if std::fs::rename(&extracted, dest).is_err() {
            std::fs::copy(&extracted, dest)
                .map_err(|e| format!("Couldn't save {}: {e}", dest.display()))?;
        }
        set_mac_executable(dest)?;
        Ok::<(), String>(())
    };

    let try_gh_gz = async {
        app.emit(
            "install-log",
            format!("Downloading {name} (static binary for {gh_arch})…"),
        )
        .ok();
        let gz_path = tmp_dir.join(format!("{name}.gz"));
        mac_curl_download(app, jobs, cancelled, job_id, &gh_url, &gz_path).await?;

        app.emit("install-log", format!("Decompressing {name}…")).ok();
        let status = tokio_cmd("/usr/bin/gunzip")
            .arg("-f")
            .arg(&gz_path)
            .status()
            .await
            .map_err(|e| format!("Failed to decompress {name}: {e}"))?;
        if !status.success() {
            return Err(format!("Failed to decompress {name}.gz"));
        }
        let extracted = tmp_dir.join(name);
        if !extracted.is_file() {
            return Err(format!("Decompressed {name} binary missing"));
        }
        if dest.exists() {
            let _ = std::fs::remove_file(dest);
        }
        if std::fs::rename(&extracted, dest).is_err() {
            std::fs::copy(&extracted, dest)
                .map_err(|e| format!("Couldn't save {}: {e}", dest.display()))?;
        }
        set_mac_executable(dest)?;
        Ok::<(), String>(())
    };

    let res = if arch == "aarch64" {
        match try_gh_gz.await {
            Ok(()) => Ok(()),
            Err(e) if is_cancelled(cancelled, job_id) || e == "Cancelled." => Err(e),
            Err(first_err) => {
                app.emit(
                    "install-log",
                    format!("Primary mirror failed ({first_err}), trying fallback mirror…"),
                )
                .ok();
                try_evermeet.await
            }
        }
    } else {
        match try_evermeet.await {
            Ok(()) => Ok(()),
            Err(e) if is_cancelled(cancelled, job_id) || e == "Cancelled." => Err(e),
            Err(first_err) => {
                app.emit(
                    "install-log",
                    format!("Primary mirror failed ({first_err}), trying GitHub mirror…"),
                )
                .ok();
                try_gh_gz.await
            }
        }
    };

    let _ = std::fs::remove_dir_all(&tmp_dir);
    res
}

#[cfg(target_os = "macos")]
async fn install_ffmpeg_macos(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
    bin_dir: &Path,
) -> Result<(), String> {
    let ffmpeg_dest = bin_dir.join("ffmpeg");
    let ffprobe_dest = bin_dir.join("ffprobe");

    download_mac_ffmpeg_binary(app, jobs, cancelled, job_id, "ffmpeg", &ffmpeg_dest).await?;
    if let Err(e) = check_mac_exec(&ffmpeg_dest, "-version").await {
        let _ = std::fs::remove_file(&ffmpeg_dest);
        invalidate_binary_health(&ffmpeg_dest);
        return Err(format!("Downloaded ffmpeg but it couldn't run: {e}"));
    }

    if let Err(e) =
        download_mac_ffmpeg_binary(app, jobs, cancelled, job_id, "ffprobe", &ffprobe_dest).await
    {
        if is_cancelled(cancelled, job_id) || e == "Cancelled." {
            return Err(e);
        }
        app.emit(
            "install-log",
            format!("Note: ffprobe skipped ({e}); ffmpeg itself is ready."),
        )
        .ok();
    }

    app.emit("install-log", format!("Saved ffmpeg to {}", bin_dir.display()))
        .ok();
    Ok(())
}

#[cfg(target_os = "macos")]
async fn install_tools_macos(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
    tools: &[String],
) -> Result<(), String> {
    let bin_dir = portable_bin_dir();
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| format!("Couldn't create {}: {e}", bin_dir.display()))?;

    for tool in tools {
        if is_cancelled(cancelled, job_id) {
            return Err("Cancelled.".to_string());
        }
        match tool.as_str() {
            "yt-dlp" => {
                let dest = bin_dir.join("yt-dlp");
                // Try the 3 MB Python zipapp first (starts in ~0.2s vs 6-10s
                // Gatekeeper unpack tax on PyInstaller `yt-dlp_macos`).
                app.emit("install-log", "Downloading yt-dlp (fast zipapp)…")
                    .ok();
                let zipapp_ok = match mac_curl_download(
                    app,
                    jobs,
                    cancelled,
                    job_id,
                    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp",
                    &dest,
                )
                .await
                {
                    Ok(()) => {
                        set_mac_executable(&dest)?;
                        match check_mac_exec(&dest, "--version").await {
                            Ok(()) => true,
                            Err(_) => {
                                let _ = std::fs::remove_file(&dest);
                                invalidate_binary_health(&dest);
                                false
                            }
                        }
                    }
                    Err(e) if is_cancelled(cancelled, job_id) || e == "Cancelled." => return Err(e),
                    Err(_) => false,
                };

                if !zipapp_ok {
                    app.emit(
                        "install-log",
                        "Python 3.10+ not found — downloading standalone yt-dlp_macos binary…",
                    )
                    .ok();
                    mac_curl_download(
                        app,
                        jobs,
                        cancelled,
                        job_id,
                        "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_macos",
                        &dest,
                    )
                    .await?;
                    set_mac_executable(&dest)?;

                    app.emit("install-log", "Verifying yt-dlp runs on this Mac…")
                        .ok();
                    if let Err(e) = check_mac_exec(&dest, "--version").await {
                        let _ = std::fs::remove_file(&dest);
                        invalidate_binary_health(&dest);
                        return Err(format!(
                            "Downloaded yt-dlp but it won't run on this Mac's CPU ({}): {e}",
                            std::env::consts::ARCH
                        ));
                    }
                }
                app.emit("install-log", format!("Saved to {}", dest.display()))
                    .ok();
            }
            "ffmpeg" => {
                install_ffmpeg_macos(app, jobs, cancelled, job_id, &bin_dir).await?;
            }
            "spotdl" => {
                let dest = bin_dir.join("spotdl");
                app.emit("install-log", "Resolving latest spotDL release…")
                    .ok();
                let (_, url) = spotdl_release().await?;
                app.emit("install-log", "Downloading spotDL (portable binary)…")
                    .ok();
                mac_curl_download(app, jobs, cancelled, job_id, &url, &dest).await?;
                set_mac_executable(&dest)?;

                app.emit("install-log", "Verifying spotDL runs on this Mac…")
                    .ok();
                if let Err(e) = check_mac_exec(&dest, "--version").await {
                    let _ = std::fs::remove_file(&dest);
                    invalidate_binary_health(&dest);
                    if e.raw_os_error() == Some(86) {
                        app.emit(
                            "install-log",
                            format!(
                                "spotDL's binary doesn't run on this Mac's CPU ({}) — falling back to `pip install spotdl`…",
                                std::env::consts::ARCH
                            ),
                        )
                        .ok();
                        let script = pip_install_spotdl(app, jobs, cancelled, job_id).await?;
                        std::os::unix::fs::symlink(&script, &dest)
                            .map_err(|e| format!("Couldn't link spotDL: {e}"))?;
                        invalidate_binary_health(&dest);
                        app.emit(
                            "install-log",
                            format!("Linked spotDL (via pip) at {}", dest.display()),
                        )
                        .ok();
                    } else {
                        return Err(format!("Downloaded spotDL but couldn't run it: {e}"));
                    }
                } else {
                    app.emit("install-log", format!("Saved to {}", dest.display()))
                        .ok();
                }
            }
            other => {
                app.emit("install-log", format!("Skipping unknown tool: {other}"))
                    .ok();
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn system32_bin(name: &str) -> Option<PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let cand = root.join("System32").join(format!("{name}.exe"));
    cand.is_file().then_some(cand)
}

#[cfg(windows)]
async fn curl_download(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
    url: &str,
    dest: &Path,
) -> Result<(), String> {
    let curl = system32_bin("curl")
        .or_else(|| resolve_binary("curl"))
        .ok_or("curl.exe not found. It ships with Windows 10/11 — check it hasn't been removed.")?;
    curl_download_with_bin(app, jobs, cancelled, job_id, &curl, url, dest).await
}

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
async fn install_tools_portable(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    job_id: &str,
    tools: &[String],
) -> Result<(), String> {
    let bin_dir = portable_bin_dir();
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| format!("Couldn't create {}: {e}", bin_dir.display()))?;

    for tool in tools {
        if is_cancelled(cancelled, job_id) {
            return Err("Cancelled.".to_string());
        }
        match tool.as_str() {
            "yt-dlp" => {
                app.emit("install-log", "Downloading yt-dlp.exe…").ok();
                curl_download(
                    app,
                    jobs,
                    cancelled,
                    job_id,
                    "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe",
                    &bin_dir.join("yt-dlp.exe"),
                )
                .await?;
                app.emit(
                    "install-log",
                    format!("Saved to {}", bin_dir.join("yt-dlp.exe").display()),
                )
                .ok();
            }
            "ffmpeg" => {
                app.emit(
                    "install-log",
                    "Downloading ffmpeg (~90 MB, can take a few minutes)…",
                )
                .ok();
                let tmp_dir =
                    std::env::temp_dir().join(format!("fetch-ffmpeg-{}", std::process::id()));
                std::fs::create_dir_all(&tmp_dir).map_err(|e| e.to_string())?;
                let zip_path = tmp_dir.join("ffmpeg.zip");

                let dl = curl_download(
                    app,
                    jobs,
                    cancelled,
                    job_id,
                    "https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip",
                    &zip_path,
                )
                .await;
                if let Err(e) = dl {
                    let _ = std::fs::remove_dir_all(&tmp_dir);
                    return Err(e);
                }

                app.emit("install-log", "Extracting ffmpeg…").ok();
                let tar = system32_bin("tar").ok_or(
                    "tar.exe not found in System32. It ships with Windows 10/11 — check it hasn't been removed.",
                )?;
                let status = tokio_cmd(&tar)
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
                        let dst = bin_dir.join(name);
                        std::fs::copy(&src, &dst)
                            .map_err(|e| format!("Couldn't copy {name}: {e}"))?;
                        invalidate_binary_health(&dst);
                        found += 1;
                    }
                }
                let _ = std::fs::remove_dir_all(&tmp_dir);
                if found < 2 {
                    return Err(
                        "ffmpeg.exe/ffprobe.exe weren't found in the downloaded archive."
                            .to_string(),
                    );
                }
                app.emit(
                    "install-log",
                    format!("Saved ffmpeg + ffprobe to {}", bin_dir.display()),
                )
                .ok();
            }
            "spotdl" => {
                app.emit("install-log", "Resolving latest spotDL release…")
                    .ok();
                let (_, url) = spotdl_release().await?;
                app.emit("install-log", "Downloading spotdl.exe…").ok();
                curl_download(
                    app,
                    jobs,
                    cancelled,
                    job_id,
                    &url,
                    &bin_dir.join("spotdl.exe"),
                )
                .await?;
                app.emit(
                    "install-log",
                    format!("Saved to {}", bin_dir.join("spotdl.exe").display()),
                )
                .ok();
            }
            other => {
                app.emit("install-log", format!("Skipping unknown tool: {other}"))
                    .ok();
            }
        }
    }
    Ok(())
}

#[derive(Serialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ToolUpdate {
    current: Option<String>,
    latest: Option<String>,
    update_available: bool,
    source: String, // "portable" | "brew" | "other" | ""
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateCheck {
    yt_dlp: ToolUpdate,
    ffmpeg: ToolUpdate,
    spotdl: ToolUpdate,
}

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

pub(crate) async fn curl_get_timeout(url: &str, max_secs: u64) -> Result<String, String> {
    #[cfg(windows)]
    let curl = system32_bin("curl")
        .or_else(|| resolve_binary("curl"))
        .ok_or("curl.exe not found.")?;
    #[cfg(not(windows))]
    let curl = resolve_binary("curl").ok_or("curl not found.")?;

    let max_time = max_secs.to_string();
    let output = tokio_cmd(&curl)
        .args([
            "-L",
            "-sS",
            "--fail",
            "--connect-timeout",
            "5",
            "--max-time",
            &max_time,
            "-H",
            "User-Agent: fetch-app",
        ])
        .arg(url)
        .output()
        .await
        .map_err(|e| format!("Failed to launch curl: {e}"))?;
    if !output.status.success() {
        return Err(format!("Request failed: {url}"));
    }
    String::from_utf8(output.stdout).map_err(|e| e.to_string())
}

async fn curl_get(url: &str) -> Result<String, String> {
    curl_get_timeout(url, 15).await
}

async fn latest_ytdlp_version() -> Result<String, String> {
    let body = curl_get("https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest").await?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    json.get("tag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Unexpected response from GitHub".to_string())
}

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
        return Err(
            "spotDL only ships portable binaries for Windows and macOS. Install it with `pip install spotdl`."
                .to_string(),
        );
    };

    let arch = std::env::consts::ARCH;
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
        let arch_match = match arch {
            "aarch64" => name.contains("arm64") || name.contains("aarch64"),
            _ => !name.contains("arm64") && !name.contains("aarch64"),
        };
        if arch_match {
            return Ok((tag, url.to_string()));
        }
        fallback.get_or_insert_with(|| url.to_string());
    }
    fallback.map(|u| (tag, u)).ok_or_else(|| {
        "Couldn't find a spotDL download for this platform in the latest release.".to_string()
    })
}

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
    let mut child = tokio_cmd(path)
        .arg(arg)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let mut stderr = child.stderr.take()?;
    let err_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let out_fut = async {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    };
    let out_buf = tokio::time::timeout(Duration::from_secs(10), out_fut)
        .await
        .ok()?;
    let _ = child.wait().await;
    let err_buf = err_handle.await.unwrap_or_default();

    let stdout_str = String::from_utf8_lossy(&out_buf);
    if !stdout_str.trim().is_empty() {
        Some(stdout_str.into_owned())
    } else {
        Some(String::from_utf8_lossy(&err_buf).into_owned())
    }
}

pub(crate) fn parse_leading_version(s: &str) -> Option<String> {
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

/// Numeric dot-version comparison (`current < latest`) so minor/local newer
/// versions never trigger false update prompts.
pub(crate) fn version_is_older(current: &str, latest: &str) -> bool {
    let parse_parts = |s: &str| -> Vec<u64> {
        s.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|p| {
                let digits: String = p.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits.parse::<u64>().unwrap_or(0)
            })
            .collect()
    };
    let a = parse_parts(current);
    let b = parse_parts(latest);
    let len = a.len().max(b.len());
    for i in 0..len {
        let av = *a.get(i).unwrap_or(&0);
        let bv = *b.get(i).unwrap_or(&0);
        if av != bv {
            return av < bv;
        }
    }
    false
}

#[tauri::command]
pub async fn installed_versions() -> UpdateCheck {
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
pub async fn check_for_updates() -> UpdateCheck {
    let mut out = installed_versions().await;

    let ytdlp_fut = async {
        if !out.yt_dlp.source.is_empty() {
            latest_ytdlp_version().await.ok()
        } else {
            None
        }
    };

    let ffmpeg_fut = async {
        if !out.ffmpeg.source.is_empty() {
            latest_ffmpeg_version().await.ok()
        } else {
            None
        }
    };

    let spotdl_fut = async {
        if !out.spotdl.source.is_empty() {
            spotdl_release()
                .await
                .ok()
                .map(|(tag, _)| tag.trim_start_matches('v').to_string())
        } else {
            None
        }
    };

    let (latest_ytdlp, latest_ffmpeg, latest_spotdl) =
        tokio::join!(ytdlp_fut, ffmpeg_fut, spotdl_fut);

    if let Some(latest) = latest_ytdlp {
        out.yt_dlp.update_available = out
            .yt_dlp
            .current
            .as_deref()
            .map(|c| version_is_older(c, &latest))
            .unwrap_or(false);
        out.yt_dlp.latest = Some(latest);
    }
    if let Some(latest) = latest_ffmpeg {
        out.ffmpeg.update_available = out
            .ffmpeg
            .current
            .as_deref()
            .map(|c| version_is_older(c, &latest))
            .unwrap_or(false);
        out.ffmpeg.latest = Some(latest);
    }
    if let Some(latest) = latest_spotdl {
        out.spotdl.update_available = out
            .spotdl
            .current
            .as_deref()
            .map(|c| version_is_older(c, &latest))
            .unwrap_or(false);
        out.spotdl.latest = Some(latest);
    }

    out
}

#[tauri::command]
pub async fn update_tool(
    app: AppHandle,
    jobs: State<'_, Jobs>,
    cancelled: State<'_, CancelledIds>,
    install_lock: State<'_, InstallLock>,
    tool: String,
) -> Result<(), String> {
    let _lock = InstallGuard::acquire(&install_lock)?;
    let _job = JobGuard::new(INSTALL_JOB_ID, &jobs, &cancelled);

    // Always install/update portably into `portable_bin_dir()`, which is
    // searched first by `resolve_binary()` and `augmented_path()` — so even
    // if an older copy came from Homebrew or system PATH, updating drops a
    // fast pre-built static binary in place without running `brew upgrade`.
    #[cfg(target_os = "macos")]
    let result = install_tools_macos(
        &app,
        &jobs,
        &cancelled,
        INSTALL_JOB_ID,
        std::slice::from_ref(&tool),
    )
    .await;

    #[cfg(windows)]
    let result = install_tools_portable(
        &app,
        &jobs,
        &cancelled,
        INSTALL_JOB_ID,
        std::slice::from_ref(&tool),
    )
    .await;

    #[cfg(all(not(target_os = "macos"), not(windows)))]
    let result: Result<(), String> = Err(format!(
        "{tool} wasn't installed by FETCH — update it with your package manager."
    ));

    if let Err(msg) = &result {
        app.emit("install-error", msg.clone()).ok();
        return result;
    }
    app.emit("install-done", ()).ok();
    Ok(())
}

#[tauri::command]
pub fn restart_app(app: AppHandle) {
    app.restart();
}

/// Background startup migration on macOS: if `portable_bin_dir()/yt-dlp` is
/// still the 35 MB PyInstaller binary (which suffers a 6-10s Gatekeeper scan
/// on every launch), silently replace it with the 3 MB Python zipapp if
/// Python 3.10+ works on this machine.
#[cfg(target_os = "macos")]
pub(crate) async fn maybe_upgrade_slow_mac_ytdlp() {
    let dest = portable_bin_dir().join("yt-dlp");
    let Ok(meta) = std::fs::metadata(&dest) else {
        return;
    };
    if meta.len() <= 10 * 1024 * 1024 {
        return;
    }
    let Some(curl) = resolve_binary("curl") else {
        return;
    };
    let tmp = dest.with_file_name("yt-dlp.zipapp.part");
    let _ = std::fs::remove_file(&tmp);
    let ok = tokio_cmd(&curl)
        .args([
            "-L",
            "-sS",
            "--fail",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "-o",
        ])
        .arg(&tmp)
        .arg("https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if set_mac_executable(&tmp).is_err() || check_mac_exec(&tmp, "--version").await.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, &dest).is_ok() {
        invalidate_binary_health(&dest);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}
