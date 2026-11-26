// FETCH — mini downloader backend.
// Thin Rust layer over `yt-dlp` (+ `ffmpeg` for merging/extraction, `spotdl` for Spotify).

mod analyze;
mod binaries;
mod cookies;
mod download;
mod installer;
mod process;

use process::{CancelledIds, InstallLock, Jobs};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    binaries::migrate_legacy_files();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(Jobs::default())
        .manage(CancelledIds::default())
        .manage(InstallLock::default())
        .setup(|_app| {
            #[cfg(target_os = "macos")]
            tauri::async_runtime::spawn(installer::maybe_upgrade_slow_mac_ytdlp());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            binaries::check_binaries,
            binaries::default_download_dir,
            cookies::browser_is_running,
            cookies::import_cookies_from_browser,
            cookies::cancel_cookie_import,
            analyze::analyze,
            analyze::quick_meta,
            analyze::cancel_analyze,
            download::download,
            download::cancel_download,
            download::reveal_in_folder,
            installer::detect_installers,
            installer::install_tools,
            installer::cancel_install,
            installer::installed_versions,
            installer::check_for_updates,
            installer::update_tool,
            installer::restart_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use tokio::io::BufReader;

    #[test]
    fn part_path_preserves_exe_extension() {
        let p = Path::new("yt-dlp.exe");
        assert_eq!(installer::part_path(p), PathBuf::from("yt-dlp.exe.part"));
        let mac = Path::new("/tmp/yt-dlp");
        assert_eq!(installer::part_path(mac), PathBuf::from("/tmp/yt-dlp.part"));
    }

    #[test]
    fn url_source_detects_spotify_and_ytdlp() {
        assert_eq!(
            binaries::url_source("https://open.spotify.com/track/123"),
            binaries::Source::Spotify
        );
        assert_eq!(
            binaries::url_source("spotify:track:123"),
            binaries::Source::Spotify
        );
        assert_eq!(
            binaries::url_source("https://www.youtube.com/watch?v=abc"),
            binaries::Source::YtDlp
        );
    }

    #[test]
    fn parse_leading_version_extracts_semver() {
        assert_eq!(
            installer::parse_leading_version("ffmpeg version 7.1.1 Copyright (c) 2000-2024"),
            Some("7.1.1".to_string())
        );
        assert_eq!(
            installer::parse_leading_version("spotdl 4.2.10"),
            Some("4.2.10".to_string())
        );
        assert_eq!(installer::parse_leading_version("no version"), None);
    }

    #[test]
    fn strip_ansi_removes_escape_sequences() {
        let raw = "\x1b[34m==>\x1b[0m \x1b[1mUpgrading ffmpeg\x1b[0m";
        assert_eq!(process::strip_ansi(raw), "==> Upgrading ffmpeg");
    }

    #[test]
    fn vimeo_player_url_rewrites_standard_and_unlisted_urls() {
        assert_eq!(
            analyze::vimeo_player_url("https://vimeo.com/1161382260"),
            Some("https://player.vimeo.com/video/1161382260".to_string())
        );
        assert_eq!(
            analyze::vimeo_player_url("https://vimeo.com/1161382260/abcdef1234"),
            Some("https://player.vimeo.com/video/1161382260?h=abcdef1234".to_string())
        );
        assert_eq!(
            analyze::vimeo_player_url("https://www.youtube.com/watch?v=0DXkqHwmmtg"),
            None
        );
    }

    #[test]
    fn extract_youtube_single_id_handles_watch_shorts_and_playlists() {
        assert_eq!(
            analyze::extract_youtube_single_id("https://www.youtube.com/watch?v=0DXkqHwmmtg&t=1076s"),
            Some("0DXkqHwmmtg".to_string())
        );
        assert_eq!(
            analyze::extract_youtube_single_id("https://youtu.be/0DXkqHwmmtg?si=abc"),
            Some("0DXkqHwmmtg".to_string())
        );
        assert_eq!(
            analyze::extract_youtube_single_id("https://www.youtube.com/watch?v=0DXkqHwmmtg&list=PL123"),
            None
        );
    }

    #[test]
    fn version_is_older_compares_numeric_segments() {
        assert!(installer::version_is_older("7.1.1", "8.0.0"));
        assert!(installer::version_is_older("2025.01.15", "2025.02.01"));
        assert!(!installer::version_is_older("8.0.1", "8.0.0"));
        assert!(!installer::version_is_older("8.0", "8.0.0"));
    }

    #[test]
    fn portable_bin_dir_uses_os_app_data() {
        let bin = binaries::portable_bin_dir();
        assert!(bin.ends_with("com.fetch.downloader/bin") || bin.ends_with(r"com.fetch.downloader\bin"));
        #[cfg(target_os = "macos")]
        assert!(bin.to_string_lossy().contains("Library/Application Support/com.fetch.downloader/bin"));
    }

    #[tokio::test]
    async fn read_line_or_cr_splits_cr_crlf_and_lf() {
        let data = b"FETCHPROG|10%|1MB/s|00:05\rFETCHPROG|50%|2MB/s|00:02\r\nDone\nTail";
        let mut reader = BufReader::new(&data[..]);
        let mut buf = Vec::new();
        let mut lines = Vec::new();
        while let Some(l) = process::read_line_or_cr(&mut reader, &mut buf)
            .await
            .unwrap()
        {
            lines.push(l);
        }
        assert_eq!(
            lines,
            vec![
                "FETCHPROG|10%|1MB/s|00:05",
                "FETCHPROG|50%|2MB/s|00:02",
                "Done",
                "Tail"
            ]
        );
    }
}
