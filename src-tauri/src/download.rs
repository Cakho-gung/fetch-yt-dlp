// Media downloads (yt-dlp + spotDL), progress streaming, post-download
// description tidying, and revealing output files in Finder/Explorer.

use crate::analyze::{
    build_attempts, clean_spotdl_error, clean_ytdlp_error, should_try_next, url_is_vimeo, YtAttempt,
};
use crate::binaries::{
    default_download_dir, resolve_binary, spotdl, spotdl_launch_error, url_source, ytdlp, Source,
};
use crate::installer::ensure_tool_installed;
use crate::process::{
    cancel_job, is_cancelled, read_line_or_cr, reclaim_job, register_job, tokio_cmd, CancelledIds,
    JobGuard, Jobs,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::SystemTime;
use tauri::{AppHandle, Emitter, State};
use tokio::io::BufReader;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRequest {
    id: String,
    url: String,
    format_selector: String,
    kind: String,                 // "video" | "audio"
    audio_format: Option<String>, // "original" | "m4a" | "mp3-320" | ... when kind == audio
    #[serde(default)]
    quality_id: String,
    output_dir: String,
    write_thumbnail: bool,
    write_description: bool,
    write_subs: bool,
    playlist_items: Option<String>,
    cookie_mode: Option<String>,
    cookie_file: Option<String>,
    #[serde(default)]
    cookie_browser: Option<String>,
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
    filesize: Option<u64>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct ErrorPayload {
    id: String,
    message: String,
}

fn quality_stamp(id: &str) -> String {
    match id {
        "" => String::new(),
        "best" => "Best".into(),
        "original" => "Original".into(),
        "m4a" => "M4A".into(),
        _ => match id.strip_prefix("mp3-") {
            Some(kbps) => format!("MP3 {kbps}"),
            None => id.into(),
        },
    }
}

#[tauri::command]
pub async fn download(
    app: AppHandle,
    jobs: State<'_, Jobs>,
    cancelled: State<'_, CancelledIds>,
    req: DownloadRequest,
) -> Result<(), String> {
    let _guard = JobGuard::new(&req.id, &jobs, &cancelled);

    if url_source(&req.url) == Source::Spotify {
        return download_spotify(app, &jobs, &cancelled, req).await;
    }
    let mut bin = ytdlp()?;
    std::fs::create_dir_all(&req.output_dir)
        .map_err(|e| format!("Cannot create download folder: {e}"))?;

    // Self-heal missing/broken ffmpeg (e.g. half-upgraded Homebrew dylibs)
    // before invoking yt-dlp so format merging (`bv*+ba`) always works.
    if resolve_binary("ffmpeg").is_none() {
        let _ = app.emit(
            "dl-progress",
            ProgressPayload {
                id: req.id.clone(),
                percent: 0.0,
                speed: String::new(),
                eta: String::new(),
                stage: "installing ffmpeg…".into(),
            },
        );
        ensure_tool_installed(&app, &jobs, &cancelled, &req.id, "ffmpeg").await?;
    }

    let vimeo = url_is_vimeo(&req.url);
    let attempts = build_attempts(
        &req.url,
        req.cookie_mode.as_deref(),
        req.cookie_file.as_deref(),
        req.cookie_browser.as_deref(),
    );

    let mut first_err: Option<String> = None;
    let mut succeeded = None;
    for att in &attempts {
        match run_ytdlp_download(&app, &jobs, &cancelled, &bin, &req, att).await {
            Ok(done) => {
                succeeded = Some(done);
                break;
            }
            Err(e) => {
                if is_cancelled(&cancelled, &req.id) || e == "job was cancelled" {
                    return Err(e);
                }
                let retry = should_try_next(vimeo, &e);
                if first_err.is_none() {
                    first_err = Some(e);
                }
                if !retry {
                    break;
                }
            }
        }
    }

    // Self-heal an outdated yt-dlp: fetch the latest release and retry once.
    let format_err = first_err
        .as_deref()
        .map(|e| e.to_lowercase().contains("requested format is not available"))
        .unwrap_or(false);
    if succeeded.is_none() && format_err {
        let _ = app.emit(
            "dl-progress",
            ProgressPayload {
                id: req.id.clone(),
                percent: 0.0,
                speed: String::new(),
                eta: String::new(),
                stage: "updating yt-dlp…".into(),
            },
        );
        if ensure_tool_installed(&app, &jobs, &cancelled, &req.id, "yt-dlp")
            .await
            .is_ok()
        {
            if let Ok(fresh_bin) = ytdlp() {
                bin = fresh_bin;
                match run_ytdlp_download(&app, &jobs, &cancelled, &bin, &req, &attempts[0]).await {
                    Ok(done) => succeeded = Some(done),
                    Err(e) => {
                        if is_cancelled(&cancelled, &req.id) || e == "job was cancelled" {
                            return Err(e);
                        }
                    }
                }
            }
        }
    }

    let res: Result<(Option<String>, Vec<String>), String> = match succeeded {
        Some(done) => Ok(done),
        None => Err(first_err.unwrap_or_default()),
    };

    match res {
        Ok((final_path, all_paths)) => {
            if req.write_description {
                for p in &all_paths {
                    tidy_description_file(p);
                }
            }
            let total_bytes: u64 = all_paths
                .iter()
                .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
                .sum();
            app.emit(
                "dl-done",
                DonePayload {
                    id: req.id.clone(),
                    filepath: final_path,
                    filesize: (total_bytes > 0).then_some(total_bytes),
                },
            )
            .ok();
            Ok(())
        }
        Err(raw_err) => {
            if is_cancelled(&cancelled, &req.id) || raw_err == "job was cancelled" {
                return Err(raw_err);
            }
            let message = if raw_err.trim().is_empty() {
                "Download failed.".to_string()
            } else {
                clean_ytdlp_error(&raw_err)
            };
            app.emit(
                "dl-error",
                ErrorPayload {
                    id: req.id,
                    message: message.clone(),
                },
            )
            .ok();
            Err(message)
        }
    }
}

async fn run_ytdlp_download(
    app: &AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    bin: &Path,
    req: &DownloadRequest,
    attempt: &YtAttempt,
) -> Result<(Option<String>, Vec<String>), String> {
    if is_cancelled(cancelled, &req.id) {
        return Err("job was cancelled".into());
    }

    let stamp = quality_stamp(&req.quality_id);
    let filename = if stamp.is_empty() {
        "%(title)s.%(ext)s".to_string()
    } else {
        format!("{stamp} - %(title)s.%(ext)s")
    };
    let out_tmpl = Path::new(&req.output_dir)
        .join("%(title)s")
        .join(&filename)
        .to_string_lossy()
        .into_owned();

    let mut args: Vec<String> = vec![
        attempt.url.clone(),
        "-f".into(),
        req.format_selector.clone(),
        "-o".into(),
        out_tmpl,
        "--windows-filenames".into(),
        "--trim-filenames".into(),
        "180".into(),
        "--socket-timeout".into(),
        "20".into(),
        "--no-overwrites".into(),
        "--newline".into(),
        "--no-color".into(),
        "--no-warnings".into(),
        "--progress".into(),
        "--progress-template".into(),
        "download:FETCHPROG|%(progress._percent_str)s|%(progress._speed_str)s|%(progress._eta_str)s".into(),
        "--no-simulate".into(),
        "--print".into(),
        "before_dl:FETCHBEFORE|%(filepath)s".into(),
        "--print".into(),
        "after_move:FETCHFILE|%(filepath)s".into(),
    ];
    if let Some(ff) = resolve_binary("ffmpeg") {
        args.push("--ffmpeg-location".into());
        args.push(ff.to_string_lossy().into_owned());
    }
    args.extend(attempt.cookie_args.iter().cloned());
    args.extend(attempt.extra_args.iter().cloned());

    if req.kind == "audio" {
        let af = req.audio_format.as_deref().unwrap_or("original");
        args.push("-x".into());
        if af == "original" {
            // Keep source codec untouched.
        } else if let Some(kbps) = af.strip_prefix("mp3-") {
            args.push("--audio-format".into());
            args.push("mp3".into());
            args.push("--audio-quality".into());
            args.push(format!("{kbps}K"));
        } else {
            args.push("--audio-format".into());
            args.push(af.into());
        }
    } else {
        args.push("--merge-output-format".into());
        args.push("mp4".into());
    }
    args.push("--embed-metadata".into());
    args.push("--embed-thumbnail".into());
    args.push("--convert-thumbnails".into());
    args.push("jpg".into());

    if req.write_thumbnail {
        args.push("--write-thumbnail".into());
    }
    if req.write_description {
        args.push("--write-description".into());
    }
    if req.write_subs && req.kind == "video" {
        args.push("--write-subs".into());
        args.push("--write-auto-subs".into());
        args.push("--sub-langs".into());
        args.push("en.*,vi.*,en,vi".into());
        args.push("--convert-subs".into());
        args.push("srt".into());
    }
    if let Some(items) = &req.playlist_items {
        if !items.is_empty() {
            args.push("--playlist-items".into());
            args.push(items.clone());
        }
    } else {
        args.push("--no-playlist".into());
    }

    let mut child = tokio_cmd(bin)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to launch yt-dlp: {e}"))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;

    register_job(jobs, cancelled, &req.id, child)
        .map_err(|_| "job was cancelled".to_string())?;

    let id = req.id.clone();
    let mut final_path: Option<String> = None;
    let mut before_paths: Vec<String> = Vec::new();
    let mut all_paths: Vec<String> = Vec::new();

    let err_app = app.clone();
    let err_id = id.clone();
    let stderr_handle = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line_buf = Vec::new();
        let mut buf = String::new();
        while let Ok(Some(line)) = read_line_or_cr(&mut reader, &mut line_buf).await {
            if line.trim().starts_with("ERROR:") {
                buf.push_str(&line);
                buf.push('\n');
            }
            if !line.trim().is_empty() {
                let _ = err_app.emit("dl-log", (err_id.clone(), line));
            }
        }
        buf
    });

    let mut reader = BufReader::new(stdout);
    let mut line_buf = Vec::new();
    while let Ok(Some(line)) = read_line_or_cr(&mut reader, &mut line_buf).await {
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
            if !p.is_empty() {
                all_paths.push(p.clone());
                final_path = Some(p);
            }
        } else if let Some(rest) = line.strip_prefix("FETCHBEFORE|") {
            let p = rest.trim().to_string();
            if !p.is_empty() {
                before_paths.push(p);
            }
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

    let mut child = reclaim_job(jobs, &id, "job was cancelled")?;
    let status = child
        .wait()
        .await
        .map_err(|e| format!("yt-dlp did not exit cleanly: {e}"))?;
    let err_text = stderr_handle.await.unwrap_or_default();

    if status.success() {
        for bp in before_paths {
            if Path::new(&bp).is_file() && !all_paths.contains(&bp) {
                all_paths.push(bp);
            }
        }
        if final_path.is_none() {
            final_path = all_paths.last().cloned();
        }
        Ok((final_path, all_paths))
    } else {
        Err(err_text)
    }
}

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

async fn download_spotify(
    app: AppHandle,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    req: DownloadRequest,
) -> Result<(), String> {
    let bin = spotdl()?;
    std::fs::create_dir_all(&req.output_dir)
        .map_err(|e| format!("Cannot create download folder: {e}"))?;

    let started = SystemTime::now();
    let stamp = quality_stamp(&req.quality_id);
    let filename = if stamp.is_empty() {
        "{title}.{output-ext}".to_string()
    } else {
        format!("{stamp} - {{title}}.{{output-ext}}")
    };
    let out_tmpl = Path::new(&req.output_dir)
        .join("{title}")
        .join(&filename)
        .to_string_lossy()
        .into_owned();

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
    args.push("--overwrite".into());
    args.push("skip".into());
    if let Some(ff) = resolve_binary("ffmpeg") {
        args.push("--ffmpeg".into());
        args.push(ff.to_string_lossy().into_owned());
    }
    if req.write_subs {
        args.push("--generate-lrc".into());
    }

    let mut child = tokio_cmd(&bin)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| spotdl_launch_error(e, &bin))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;

    register_job(jobs, cancelled, &req.id, child)?;
    let id = req.id.clone();

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
        let mut reader = BufReader::new(stderr);
        let mut line_buf = Vec::new();
        let mut buf = String::new();
        while let Ok(Some(line)) = read_line_or_cr(&mut reader, &mut line_buf).await {
            buf.push_str(&line);
            buf.push('\n');
            if !line.trim().is_empty() {
                let _ = err_app.emit("dl-log", (err_id.clone(), line));
            }
        }
        buf
    });

    let mut done_count = 0usize;
    let mut reader = BufReader::new(stdout);
    let mut line_buf = Vec::new();
    while let Ok(Some(line)) = read_line_or_cr(&mut reader, &mut line_buf).await {
        let l = line.trim_start();
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
        if !l.is_empty() {
            let _ = app.emit("dl-log", (id.clone(), line));
        }
    }

    let mut child = reclaim_job(jobs, &id, "job was cancelled")?;
    let status = child
        .wait()
        .await
        .map_err(|e| format!("spotDL did not exit cleanly: {e}"))?;
    let err_text = stderr_handle.await.unwrap_or_default();

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
                if best
                    .as_ref()
                    .map(|(t, _)| meta.modified().map(|m| m > *t).unwrap_or(false))
                    .unwrap_or(true)
                {
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
pub fn cancel_download(
    jobs: State<'_, Jobs>,
    cancelled: State<'_, CancelledIds>,
    id: String,
) -> Result<(), String> {
    cancel_job(&jobs, &cancelled, &id);
    Ok(())
}

#[tauri::command]
pub fn reveal_in_folder(path: String) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let normalized = path.replace('/', "\\");
    #[cfg(not(target_os = "windows"))]
    let normalized = path;

    let mut p = PathBuf::from(&normalized);
    if !p.exists() {
        while let Some(parent) = p.parent() {
            if parent.exists() {
                p = parent.to_path_buf();
                break;
            }
            p = parent.to_path_buf();
        }
        if !p.exists() {
            p = PathBuf::from(default_download_dir());
        }
    }

    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        if p.is_file() {
            cmd.arg("-R");
        }
        cmd.arg(&p);
        cmd.spawn().map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        if p.is_file() {
            let escaped = p.to_string_lossy().replace('"', "");
            std::process::Command::new("explorer")
                .raw_arg(format!("/select,\"{escaped}\""))
                .spawn()
                .map_err(|e| e.to_string())?;
        } else {
            std::process::Command::new("explorer")
                .arg(&p)
                .spawn()
                .map_err(|e| e.to_string())?;
        }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let target = if p.is_file() {
            p.parent().unwrap_or(&p).to_path_buf()
        } else {
            p
        };
        std::process::Command::new("xdg-open")
            .arg(target)
            .spawn()
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}
