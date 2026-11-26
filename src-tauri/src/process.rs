// Subprocess spawning, cancellation tracking, install locking, and CR/LF stream reading.

use crate::binaries::augmented_path;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};
use tokio::process::{Child, Command};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Creates a `tokio::process::Command` configured for non-interactive
/// background execution: null stdin (so no CLI ever stalls waiting for a
/// prompt), augmented PATH, UTF-8 Python stdio, and `CREATE_NO_WINDOW` on
/// Windows so child CLIs never flash console windows.
pub(crate) fn tokio_cmd(bin: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(bin);
    cmd.stdin(Stdio::null())
        .env("PATH", augmented_path())
        .env("PYTHONUTF8", "1")
        .env("PYTHONIOENCODING", "utf-8");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Synchronous counterpart of `tokio_cmd` for quick OS queries (`pgrep`,
/// `osascript`, `tasklist`, `taskkill`, `xattr`).
pub(crate) fn std_cmd(bin: impl AsRef<OsStr>) -> std::process::Command {
    let mut cmd = std::process::Command::new(bin);
    cmd.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

#[derive(Default)]
pub struct Jobs(pub(crate) Mutex<HashMap<String, Child>>);

/// Ids the user has asked to cancel, checked at every phase boundary and
/// immediately after `spawn()` so a cancel landing before `jobs.insert()`
/// never leaves an orphaned child running.
#[derive(Default)]
pub struct CancelledIds(pub(crate) Mutex<HashSet<String>>);

#[derive(Default)]
pub struct InstallLock(pub(crate) AtomicBool);

pub(crate) struct InstallGuard<'a>(&'a InstallLock);

impl<'a> InstallGuard<'a> {
    pub(crate) fn acquire(lock: &'a InstallLock) -> Result<Self, String> {
        lock.0
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| Self(lock))
            .map_err(|_| "Another install or update is already in progress.".to_string())
    }
}

impl Drop for InstallGuard<'_> {
    fn drop(&mut self) {
        self.0 .0.store(false, Ordering::SeqCst);
    }
}

pub(crate) struct JobGuard<'a> {
    id: String,
    jobs: &'a Jobs,
    cancelled: &'a CancelledIds,
}

impl<'a> JobGuard<'a> {
    pub(crate) fn new(id: &str, jobs: &'a Jobs, cancelled: &'a CancelledIds) -> Self {
        Self {
            id: id.to_string(),
            jobs,
            cancelled,
        }
    }
}

impl Drop for JobGuard<'_> {
    fn drop(&mut self) {
        if let Some(mut child) = self.jobs.0.lock().unwrap().remove(&self.id) {
            let _ = child.start_kill();
        }
        self.cancelled.0.lock().unwrap().remove(&self.id);
    }
}

pub(crate) fn is_cancelled(cancelled: &CancelledIds, id: &str) -> bool {
    cancelled.0.lock().unwrap().contains(id)
}

pub(crate) fn cancel_job(jobs: &Jobs, cancelled: &CancelledIds, id: &str) {
    cancelled.0.lock().unwrap().insert(id.to_string());
    if let Some(mut child) = jobs.0.lock().unwrap().remove(id) {
        let _ = child.start_kill();
    }
}

pub(crate) fn register_job(
    jobs: &Jobs,
    cancelled: &CancelledIds,
    id: &str,
    child: Child,
) -> Result<(), String> {
    jobs.0.lock().unwrap().insert(id.to_string(), child);
    if is_cancelled(cancelled, id) {
        if let Some(mut child) = jobs.0.lock().unwrap().remove(id) {
            let _ = child.start_kill();
        }
        return Err("Cancelled.".to_string());
    }
    Ok(())
}

pub(crate) fn reclaim_job(jobs: &Jobs, id: &str, cancel_msg: &str) -> Result<Child, String> {
    jobs.0
        .lock()
        .unwrap()
        .remove(id)
        .ok_or_else(|| cancel_msg.to_string())
}

/// Strips ANSI CSI escape sequences (e.g. `\x1b[34m`, `\x1b[1m`, `\x1b[0m`)
/// and stray ESC/control bytes so CLI logs render cleanly in `<pre>`.
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for nc in chars.by_ref() {
                    if ('@'..='~').contains(&nc) {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                chars.next();
                for nc in chars.by_ref() {
                    if nc == '\x07' {
                        break;
                    }
                }
            }
            continue;
        }
        if !c.is_control() || c == '\t' {
            out.push(c);
        }
    }
    out
}

/// Reads the next `\n`- or `\r`-terminated chunk from a buffered stream so
/// CLI progress updates (yt-dlp, spotDL, ffmpeg, brew) stream immediately
/// without blocking until a newline, while capping per-line memory at 64 KiB.
pub(crate) async fn read_line_or_cr<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<Option<String>> {
    const MAX_LINE_BYTES: usize = 64 * 1024;
    buf.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            return Ok(Some(String::from_utf8_lossy(buf).into_owned()));
        }
        if let Some(pos) = available.iter().position(|&b| b == b'\n' || b == b'\r') {
            let delim = available[pos];
            let take = pos.min(MAX_LINE_BYTES.saturating_sub(buf.len()));
            buf.extend_from_slice(&available[..take]);
            reader.consume(pos + 1);
            if delim == b'\r' {
                if let Ok(next) = reader.fill_buf().await {
                    if next.first() == Some(&b'\n') {
                        reader.consume(1);
                    }
                }
            }
            return Ok(Some(String::from_utf8_lossy(buf).into_owned()));
        }
        let len = available.len();
        if buf.len() < MAX_LINE_BYTES {
            let take = len.min(MAX_LINE_BYTES - buf.len());
            buf.extend_from_slice(&available[..take]);
        }
        reader.consume(len);
    }
}
