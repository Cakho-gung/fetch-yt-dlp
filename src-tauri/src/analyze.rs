// URL metadata analysis via yt-dlp (`-J`) and spotDL (`save`).

use crate::binaries::{spotdl, spotdl_launch_error, url_source, ytdlp, Source};
use crate::cookies::{cookie_args, resolve_browser_profile_arg};
use crate::installer::curl_get_timeout;
use crate::process::{
    cancel_job, is_cancelled, reclaim_job, register_job, tokio_cmd, CancelledIds, JobGuard, Jobs,
};
use chrono::Local;
use serde::Serialize;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tauri::State;
use tokio::io::AsyncReadExt;

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QualityOption {
    id: String,
    label: String,
    height: Option<u64>,
    approx_bytes: Option<u64>,
    format_selector: String,
    kind: String, // "video" | "audio"
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlaylistEntry {
    index: u64,
    title: String,
    duration_seconds: Option<f64>,
    thumbnail: Option<String>,
    url: Option<String>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeResult {
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

/// Percent-encodes a URL for query-string embedding in oEmbed calls.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

/// Converts a `vimeo.com/<id>` (or `vimeo.com/<id>/<hash>`) URL into a
/// `https://player.vimeo.com/video/<id>` URL so `yt-dlp` uses `VimeoPlayerIE`
/// instead of the broken/login-gated `VimeoIE` web client.
pub(crate) fn vimeo_player_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if !lower.contains("vimeo.com/") {
        return None;
    }
    let after_scheme = trimmed.split("://").nth(1)?;
    let (path_part, query_part) = match after_scheme.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (after_scheme, None),
    };
    let mut segs = path_part.split('/').skip(1).filter(|s| !s.is_empty());
    let mut video_id: Option<&str> = None;
    let mut path_hash: Option<&str> = None;
    for seg in &mut segs {
        if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
            video_id = Some(seg);
            if let Some(next) = segs.next() {
                if next.len() >= 6 && next.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    path_hash = Some(next);
                }
            }
            break;
        }
    }
    let id = video_id?;
    let query_hash = query_part.and_then(|q| {
        q.split('&')
            .find_map(|pair| pair.strip_prefix("h="))
            .map(|h| h.split('#').next().unwrap_or(h))
            .filter(|h| !h.is_empty())
    });
    let hash = query_hash.or(path_hash);
    match hash {
        Some(h) => Some(format!("https://player.vimeo.com/video/{id}?h={h}")),
        None => Some(format!("https://player.vimeo.com/video/{id}")),
    }
}

/// Pulls `https://player.vimeo.com/video/...` (including any `?h=<hash>`) from
/// Vimeo's oEmbed `html` iframe snippet.
fn extract_vimeo_iframe_src(html: &str) -> Option<String> {
    let marker = "https://player.vimeo.com/video/";
    let start = html.find(marker)?;
    let rest = &html[start..];
    let end = rest.find(['"', '\'', ' ']).unwrap_or(rest.len());
    Some(rest[..end].replace("&amp;", "&"))
}

/// Extracts an 11-char YouTube video ID from `watch?v=`, `youtu.be/`, or `/shorts/`
/// when the URL is not a playlist (`list=`), enabling < 300ms oEmbed preview.
pub(crate) fn extract_youtube_single_id(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    if lower.contains("list=") {
        return None;
    }
    let clean_id = |raw: &str| -> Option<String> {
        let id: String = raw
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        (id.len() == 11).then_some(id)
    };
    if let Some(pos) = lower.find("youtu.be/") {
        return clean_id(&url[pos + 9..]);
    }
    if let Some(pos) = lower.find("/shorts/") {
        return clean_id(&url[pos + 8..]);
    }
    if lower.contains("youtube.com") {
        if let Some(q_idx) = url.find('?') {
            for pair in url[q_idx + 1..].split('&') {
                if let Some(val) = pair.strip_prefix("v=") {
                    return clean_id(val);
                }
            }
        }
    }
    None
}

async fn try_fast_oembed(url: &str) -> Option<AnalyzeResult> {
    let lower = url.to_ascii_lowercase();
    if lower.contains("list=") {
        return None;
    }

    if let Some(vid) = extract_youtube_single_id(url) {
        let oembed_url = format!(
            "https://www.youtube.com/oembed?url=https://www.youtube.com/watch?v={vid}&format=json"
        );
        let body = curl_get_timeout(&oembed_url, 3).await.ok()?;
        let json: serde_json::Value = serde_json::from_str(&body).ok()?;
        let title = json.get("title").and_then(|v| v.as_str())?.to_string();
        let uploader = json
            .get("author_name")
            .and_then(|v| v.as_str())
            .map(String::from);
        let thumbnail = json
            .get("thumbnail_url")
            .and_then(|v| v.as_str())
            .map(String::from)
            .or_else(|| Some(format!("https://i.ytimg.com/vi/{vid}/hqdefault.jpg")));

        return Some(AnalyzeResult {
            kind: "video".to_string(),
            webpage_url: format!("https://www.youtube.com/watch?v={vid}"),
            title,
            uploader,
            duration_seconds: None,
            view_count: None,
            thumbnail,
            extractor: "Youtube".to_string(),
            video_options: vec![],
            audio_options: audio_tiers(None, None),
            entries: vec![],
            playlist_count: None,
        });
    }

    let enc = encode_uri_component(url);
    if lower.contains("vimeo.com/") {
        let oembed_url = format!("https://vimeo.com/api/oembed.json?url={enc}");
        if let Ok(body) = curl_get_timeout(&oembed_url, 3).await {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                if let Some(title) = json.get("title").and_then(|v| v.as_str()) {
                    let duration = json.get("duration").and_then(|v| v.as_f64());
                    let uploader = json
                        .get("author_name")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let thumbnail = json
                        .get("thumbnail_url")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let player_url = json
                        .get("html")
                        .and_then(|v| v.as_str())
                        .and_then(extract_vimeo_iframe_src)
                        .or_else(|| vimeo_player_url(url))
                        .unwrap_or_else(|| url.to_string());

                    return Some(AnalyzeResult {
                        kind: "video".to_string(),
                        webpage_url: player_url,
                        title: title.to_string(),
                        uploader,
                        duration_seconds: duration,
                        view_count: None,
                        thumbnail,
                        extractor: "Vimeo".to_string(),
                        video_options: vec![],
                        audio_options: audio_tiers(duration, None),
                        entries: vec![],
                        playlist_count: None,
                    });
                }
            }
        }
        return None;
    }

    let (oembed_url, extractor, _audio_only) = if lower.contains("tiktok.com/") {
        (format!("https://www.tiktok.com/oembed?url={enc}"), "TikTok", false)
    } else if lower.contains("soundcloud.com/") {
        (
            format!("https://soundcloud.com/oembed?format=json&url={enc}"),
            "SoundCloud",
            true,
        )
    } else if lower.contains("dailymotion.com/") || lower.contains("dai.ly/") {
        (
            format!("https://www.dailymotion.com/services/oembed?url={enc}"),
            "Dailymotion",
            false,
        )
    } else {
        (format!("https://noembed.com/embed?url={enc}"), "", false)
    };

    let body = curl_get_timeout(&oembed_url, 3).await.ok()?;
    let json: serde_json::Value = serde_json::from_str(&body).ok()?;
    if json.get("error").is_some() {
        return None;
    }
    let title = json.get("title").and_then(|v| v.as_str())?.to_string();
    let uploader = json
        .get("author_name")
        .and_then(|v| v.as_str())
        .map(String::from);
    let thumbnail = json
        .get("thumbnail_url")
        .and_then(|v| v.as_str())
        .map(String::from);
    let resolved_ext = if !extractor.is_empty() {
        extractor.to_string()
    } else {
        json.get("provider_name")
            .and_then(|v| v.as_str())
            .unwrap_or("source")
            .to_string()
    };

    Some(AnalyzeResult {
        kind: "video".to_string(),
        webpage_url: url.to_string(),
        title,
        uploader,
        duration_seconds: None,
        view_count: None,
        thumbnail,
        extractor: resolved_ext,
        video_options: vec![],
        audio_options: audio_tiers(None, None),
        entries: vec![],
        playlist_count: None,
    })
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QuickMeta {
    title: String,
    uploader: Option<String>,
    thumbnail: Option<String>,
    duration_seconds: Option<f64>,
    extractor: String,
}

/// Instant (~200 ms) title/thumbnail/uploader preview via oEmbed, shown while
/// the authoritative `yt-dlp -J` run (which gives the real quality list) is
/// still working. Never provides quality tiers.
#[tauri::command]
pub async fn quick_meta(url: String) -> Option<QuickMeta> {
    let url = url.trim().to_string();
    if url.is_empty() || url_source(&url) == Source::Spotify {
        return None;
    }
    let r = try_fast_oembed(&url).await?;
    Some(QuickMeta {
        title: r.title,
        uploader: r.uploader,
        thumbnail: r.thumbnail,
        duration_seconds: r.duration_seconds,
        extractor: r.extractor,
    })
}

pub(crate) fn url_is_vimeo(url: &str) -> bool {
    url.to_ascii_lowercase().contains("vimeo.com")
}

/// One way of invoking yt-dlp for a link: which URL, which cookie flags and
/// which extra flags. Analyze and download walk the same ordered list so a
/// site-specific workaround found for one is used by the other.
#[derive(Clone)]
pub(crate) struct YtAttempt {
    pub(crate) url: String,
    pub(crate) cookie_args: Vec<String>,
    pub(crate) extra_args: Vec<String>,
}

pub(crate) fn build_attempts(
    url: &str,
    mode: Option<&str>,
    file: Option<&str>,
    browser: Option<&str>,
) -> Vec<YtAttempt> {
    let file_args = cookie_args(mode, file);
    let vimeo = url_is_vimeo(url);
    let referer: Vec<String> = if vimeo {
        vec!["--referer".into(), "https://vimeo.com/".into()]
    } else {
        vec![]
    };
    let mk = |u: &str, c: Vec<String>, x: Vec<String>| YtAttempt {
        url: u.to_string(),
        cookie_args: c,
        extra_args: x,
    };

    let mut out = vec![mk(url, file_args.clone(), referer.clone())];
    if vimeo {
        // 2) read the browser's own cookie store directly (picks the right profile)
        if mode == Some("file") {
            if let Some(b) = browser.filter(|b| !b.is_empty()) {
                let spec = resolve_browser_profile_arg(b, Some("vimeo.com"));
                out.push(mk(
                    url,
                    vec!["--cookies-from-browser".into(), spec],
                    referer.clone(),
                ));
            }
        }
        // 3) embedded player URL (works for many public / embeddable videos)
        if let Some(p) = vimeo_player_url(url) {
            out.push(mk(&p, file_args.clone(), referer.clone()));
        }
        // 4) plain, no cookies
        if !file_args.is_empty() {
            out.push(mk(url, vec![], referer));
        }
    } else {
        if !file_args.is_empty() {
            out.push(mk(url, vec![], vec![]));
        }
        let lower = url.to_ascii_lowercase();
        if lower.contains("youtube.com") || lower.contains("youtu.be") {
            out.push(mk(
                url,
                vec![],
                vec![
                    "--extractor-args".into(),
                    "youtube:player_client=ios,mweb,tv_simply,web".into(),
                ],
            ));
        }
    }
    out
}

/// Whether a failed attempt is worth retrying with the next strategy.
pub(crate) fn should_try_next(is_vimeo: bool, err: &str) -> bool {
    let l = err.to_lowercase();
    if l.contains("timed out after") || l.contains("cancelled") {
        return false;
    }
    if is_vimeo {
        return true;
    }
    l.contains("requested format is not available")
        || l.contains("logged-in")
        || l.contains("http error 401")
        || l.contains("http error 403")
        || l.contains("sign in")
}

#[tauri::command]
pub async fn analyze(
    id: String,
    jobs: State<'_, Jobs>,
    cancelled: State<'_, CancelledIds>,
    url: String,
    cookie_mode: Option<String>,
    cookie_file: Option<String>,
    cookie_browser: Option<String>,
) -> Result<AnalyzeResult, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("Empty link.".into());
    }
    let _guard = JobGuard::new(&id, &jobs, &cancelled);

    if url_source(&url) == Source::Spotify {
        return analyze_spotify(&id, &jobs, &cancelled, url).await;
    }

    let bin = ytdlp()?;
    let vimeo = url_is_vimeo(&url);
    let attempts = build_attempts(
        &url,
        cookie_mode.as_deref(),
        cookie_file.as_deref(),
        cookie_browser.as_deref(),
    );

    let mut first_err: Option<String> = None;
    for att in &attempts {
        let mut args: Vec<String> = vec![
            "-J".into(),
            "--no-warnings".into(),
            "--no-check-formats".into(),
            "--socket-timeout".into(),
            "20".into(),
            "--flat-playlist".into(),
            "--playlist-items".into(),
            "1:60".into(),
        ];
        args.extend(att.extra_args.iter().cloned());
        args.extend(att.cookie_args.iter().cloned());
        args.push(att.url.clone());

        match run_ytdlp_json(&id, &jobs, &cancelled, &bin, &args).await {
            Ok(buf) => {
                let json: serde_json::Value = serde_json::from_slice(&buf)
                    .map_err(|e| format!("Could not parse yt-dlp output: {e}"))?;
                let mut result = build_analyze_result(&json, &att.url);
                if att.url != url && result.kind == "video" {
                    // remember the URL that actually worked so Download starts there
                    result.webpage_url = att.url.clone();
                }
                return Ok(result);
            }
            Err(e) => {
                if is_cancelled(&cancelled, &id) || e == "Analysis was cancelled." {
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
    Err(clean_ytdlp_error(&first_err.unwrap_or_default()))
}

async fn run_ytdlp_json(
    id: &str,
    jobs: &Jobs,
    cancelled: &CancelledIds,
    bin: &Path,
    args: &[String],
) -> Result<Vec<u8>, String> {
    if is_cancelled(cancelled, id) {
        return Err("Analysis was cancelled.".into());
    }
    let mut child = tokio_cmd(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("Failed to launch yt-dlp: {e}"))?;

    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;

    register_job(jobs, cancelled, id, child).map_err(|_| "Analysis was cancelled.".to_string())?;

    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let stdout_fut = async {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    };

    let stdout_buf = match tokio::time::timeout(Duration::from_secs(120), stdout_fut).await {
        Ok(buf) => buf,
        Err(_) => {
            if let Some(mut c) = jobs.0.lock().unwrap().remove(id) {
                let _ = c.start_kill();
            }
            return Err("Timed out after 2 minutes analyzing that link.".into());
        }
    };

    let mut child = reclaim_job(jobs, id, "Analysis was cancelled.")?;
    let status = child
        .wait()
        .await
        .map_err(|e| format!("yt-dlp did not exit cleanly: {e}"))?;
    let stderr_buf = stderr_handle.await.unwrap_or_default();

    if !status.success() {
        return Err(String::from_utf8_lossy(&stderr_buf).into_owned());
    }
    Ok(stdout_buf)
}

#[tauri::command]
pub fn cancel_analyze(
    jobs: State<'_, Jobs>,
    cancelled: State<'_, CancelledIds>,
    id: String,
) -> Result<(), String> {
    cancel_job(&jobs, &cancelled, &id);
    Ok(())
}

pub(crate) fn clean_ytdlp_error(raw: &str) -> String {
    let mut msg = None;
    for line in raw.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("ERROR:") {
            msg = Some(rest.trim().to_string());
            break;
        }
    }
    let out = msg.unwrap_or_else(|| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            "yt-dlp could not read that link.".to_string()
        } else {
            trimmed.lines().next().unwrap_or("Unknown error").to_string()
        }
    });
    if out.to_lowercase().contains("requested format is not available") {
        format!("{out} — try updating yt-dlp (Settings → Tools) or disabling Cookies.")
    } else {
        out
    }
}

async fn analyze_spotify(
    id: &str,
    jobs: &Jobs,
    cancelled: &CancelledIds,
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

    let mut child = tokio_cmd(&bin)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| spotdl_launch_error(e, &bin))?;

    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;

    register_job(jobs, cancelled, id, child)?;

    let stderr_handle = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        buf
    });
    let stdout_fut = async {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf).await;
        buf
    };
    if tokio::time::timeout(Duration::from_secs(120), stdout_fut)
        .await
        .is_err()
    {
        if let Some(mut c) = jobs.0.lock().unwrap().remove(id) {
            let _ = c.start_kill();
        }
        let _ = std::fs::remove_file(&tmp);
        return Err("Timed out after 2 minutes reading that Spotify link.".into());
    }

    let mut child = reclaim_job(jobs, id, "Analysis was cancelled.")?;
    let status = child
        .wait()
        .await
        .map_err(|e| format!("spotDL did not exit cleanly: {e}"))?;
    let stderr_buf = stderr_handle.await.unwrap_or_default();

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

pub(crate) fn clean_spotdl_error(raw: &str) -> String {
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
        mk("original", "ORIGINAL · BEST", best_audio_bytes, "ba/b/best"),
        mk("m4a", "M4A · BEST", best_audio_bytes, "ba[ext=m4a]/ba/b/best"),
        mk("mp3-320", "MP3 · 320K", mp3_bytes(320), "ba/b/best"),
        mk("mp3-256", "MP3 · 256K", mp3_bytes(256), "ba/b/best"),
        mk("mp3-192", "MP3 · 192K", mp3_bytes(192), "ba/b/best"),
        mk("mp3-128", "MP3 · 128K", mp3_bytes(128), "ba/b/best"),
    ]
}

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
    let dur_of = |s: &serde_json::Value| {
        s.get("duration")
            .and_then(|v| v.as_f64())
            .map(|d| if d > 86_400.0 { d / 1000.0 } else { d })
    };

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
                index: s
                    .get("list_position")
                    .and_then(|v| v.as_u64())
                    .unwrap_or((i as u64) + 1),
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

fn generic_options() -> (Vec<QualityOption>, Vec<QualityOption>) {
    let video = vec![
        QualityOption { id: "best".into(), label: "BEST".into(), height: None, approx_bytes: None, format_selector: "bv*+ba/b".into(), kind: "video".into() },
        QualityOption { id: "1080p".into(), label: "1080P".into(), height: Some(1080), approx_bytes: None, format_selector: "bv*[height<=1080]+ba/b[height<=1080]/bv*+ba/b".into(), kind: "video".into() },
        QualityOption { id: "720p".into(), label: "720P".into(), height: Some(720), approx_bytes: None, format_selector: "bv*[height<=720]+ba/b[height<=720]/bv*+ba/b".into(), kind: "video".into() },
        QualityOption { id: "480p".into(), label: "480P".into(), height: Some(480), approx_bytes: None, format_selector: "bv*[height<=480]+ba/b[height<=480]/bv*+ba/b".into(), kind: "video".into() },
    ];
    (video, audio_tiers(None, None))
}

fn derive_options(
    json: &serde_json::Value,
    duration: Option<f64>,
) -> (Vec<QualityOption>, Vec<QualityOption>) {
    let empty = vec![];
    let formats = json
        .get("formats")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty);

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
                let tbr = f.get("tbr").and_then(|v| v.as_f64())?;
                let dur = duration?;
                Some((tbr * 1000.0 / 8.0 * dur) as u64)
            })
    };

    let best_audio_bytes = formats
        .iter()
        .filter(|f| !has_video(f) && has_audio(f))
        .filter_map(fmt_bytes)
        .max()
        .unwrap_or(0);

    let bytes_for_height = |cap: u64| -> Option<u64> {
        formats
            .iter()
            .filter(|f| has_video(f) && !has_audio(f))
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
            format_selector: format!("bv*[height<={cap}]+ba/b[height<={cap}]/bv*+ba/b"),
            kind: "video".into(),
        });
    }

    let audio = audio_tiers(
        duration,
        if best_audio_bytes > 0 { Some(best_audio_bytes) } else { None },
    );

    (video, audio)
}
