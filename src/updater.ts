// ── FETCH app updater ────────────────────────────────────────────────────
// Checks for a new version of FETCH itself (the Tauri app, not yt-dlp/ffmpeg)
// on startup. If an update is available, shows a custom modal that matches
// the app's "Industry" design system instead of a native OS dialog.

import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

// ── state ────────────────────────────────────────────────────────────────
let pendingUpdate: Update | null = null;
let updatePhase: "idle" | "checking" | "available" | "downloading" | "installing" | "error" = "idle";
let downloadProgress = 0; // 0-100
let downloadedBytes = 0;
let totalBytes = 0;
let errorMessage = "";

// ── SVG icons (same inline Lucide approach as main.ts) ───────────────────
function svg(path: string, size = 16, sw = 2): string {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="${sw}" stroke-linecap="round" stroke-linejoin="round" style="display:inline-block;vertical-align:middle;flex-shrink:0">${path}</svg>`;
}
const IC_ROCKET   = `<path d="M4.5 16.5c-1.5 1.26-2 5-2 5s3.74-.5 5-2c.71-.84.7-2.13-.09-2.91a2.18 2.18 0 0 0-2.91-.09z"/><path d="m12 15-3-3a22 22 0 0 1 2-3.95A12.88 12.88 0 0 1 22 2c0 2.72-.78 7.5-6 11a22.35 22.35 0 0 1-4 2z"/><path d="M9 12H4s.55-3.03 2-4c1.62-1.08 5 0 5 0"/><path d="M12 15v5s3.03-.55 4-2c1.08-1.62 0-5 0-5"/>`;
const IC_DOWNLOAD = `<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/>`;
const IC_X        = `<path d="M18 6 6 18M6 6l12 12"/>`;
const IC_ALERT    = `<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3Z"/><path d="M12 9v4"/><path d="M12 17h.01"/>`;
const IC_LOADER   = `<path d="M21 12a9 9 0 1 1-6.219-8.56"/>`;

function fmtBytes(b: number): string {
  if (b <= 0) return "0 B";
  const u = ["B", "KB", "MB", "GB"];
  let i = 0, n = b;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n < 10 && i > 1 ? n.toFixed(1) : Math.round(n)} ${u[i]}`;
}

// ── overlay management ───────────────────────────────────────────────────
function getOverlay(): HTMLElement | null {
  return document.getElementById("updateOverlay");
}

function renderUpdateModal() {
  const overlay = getOverlay();
  if (!overlay) return;

  let body = "";
  let footer = "";

  switch (updatePhase) {
    case "available":
      body = `
        <div style="display:flex;align-items:center;gap:var(--space-3);margin-bottom:var(--space-2)">
          <span class="update-icon">${svg(IC_ROCKET, 22, 2)}</span>
          <div>
            <div class="update-ver-label">NEW VERSION AVAILABLE</div>
            <div class="update-ver" style="font:600 20px var(--font-heading);letter-spacing:-.02em">
              v${pendingUpdate?.version ?? "?"}
            </div>
          </div>
        </div>
        <div class="update-body-text" style="font-size:13.5px;color:color-mix(in srgb,var(--color-text) 75%,transparent);line-height:1.5">
          A newer build of FETCH is ready. The update will download and install
          automatically — the app restarts once it's done.
        </div>`;
      footer = `
        <button class="btn btn-secondary" id="updateSkip">NOT NOW</button>
        <button class="btn btn-primary" id="updateInstall">${svg(IC_DOWNLOAD, 14, 2.5)} UPDATE NOW</button>`;
      break;

    case "downloading":
      body = `
        <div style="display:flex;align-items:center;gap:var(--space-3);margin-bottom:var(--space-3)">
          <span class="update-icon spin">${svg(IC_LOADER, 20, 2.5)}</span>
          <div>
            <div class="update-ver-label">DOWNLOADING UPDATE</div>
            <div style="font:500 12px var(--font-mono);color:color-mix(in srgb,var(--color-text) var(--muted-alpha),transparent)">
              v${pendingUpdate?.version ?? "?"}
            </div>
          </div>
        </div>
        <div class="bar" style="margin-bottom:var(--space-2)">
          <span style="width:${downloadProgress}%"></span>
        </div>
        <div class="bar-meta">
          <span>${fmtBytes(downloadedBytes)} / ${totalBytes > 0 ? fmtBytes(totalBytes) : "—"}</span>
          <span class="pct">${Math.round(downloadProgress)}%</span>
        </div>`;
      break;

    case "installing":
      body = `
        <div style="display:flex;align-items:center;gap:var(--space-3)">
          <span class="update-icon spin">${svg(IC_LOADER, 20, 2.5)}</span>
          <div>
            <div class="update-ver-label">INSTALLING UPDATE</div>
            <div style="font:500 12px var(--font-mono);color:color-mix(in srgb,var(--color-text) var(--muted-alpha),transparent)">
              Restarting FETCH…
            </div>
          </div>
        </div>`;
      break;

    case "error":
      body = `
        <div style="display:flex;align-items:center;gap:var(--space-3);margin-bottom:var(--space-2)">
          <span class="update-icon update-icon-error">${svg(IC_ALERT, 20, 2)}</span>
          <div class="update-ver-label">UPDATE FAILED</div>
        </div>
        <div style="font-size:13px;color:color-mix(in srgb,var(--color-text) 75%,transparent);line-height:1.5;word-break:break-word">
          ${escHtml(errorMessage)}
        </div>`;
      footer = `
        <button class="btn btn-secondary" id="updateDismiss">DISMISS</button>
        <button class="btn btn-primary" id="updateRetry">TRY AGAIN</button>`;
      break;

    default:
      return;
  }

  overlay.innerHTML = `
    <div class="modal update-modal">
      <div class="modal-head">
        <span style="font:600 13px var(--font-heading);letter-spacing:.06em">APP UPDATE</span>
        ${updatePhase === "available" || updatePhase === "error"
          ? `<span class="icon-x" id="updateClose">${svg(IC_X, 14)}</span>`
          : ""}
      </div>
      <div class="modal-body">${body}</div>
      ${footer ? `<div class="modal-foot" style="gap:var(--space-2)">${footer}</div>` : ""}
    </div>`;

  // wire buttons
  overlay.querySelector("#updateClose")?.addEventListener("click", closeUpdateModal);
  overlay.querySelector("#updateSkip")?.addEventListener("click", closeUpdateModal);
  overlay.querySelector("#updateDismiss")?.addEventListener("click", closeUpdateModal);
  overlay.querySelector("#updateInstall")?.addEventListener("click", doUpdate);
  overlay.querySelector("#updateRetry")?.addEventListener("click", doUpdate);
}

function escHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!)
  );
}

function openUpdateModal() {
  const overlay = getOverlay();
  if (!overlay) return;
  overlay.classList.remove("hidden");
  renderUpdateModal();
  requestAnimationFrame(() => overlay.classList.add("open"));
}

function closeUpdateModal() {
  const overlay = getOverlay();
  if (!overlay) return;
  overlay.classList.remove("open");
  setTimeout(() => {
    overlay.classList.add("hidden");
    overlay.innerHTML = "";
    updatePhase = "idle";
  }, 180);
}

// ── update flow ──────────────────────────────────────────────────────────
async function doUpdate() {
  if (!pendingUpdate) return;
  updatePhase = "downloading";
  downloadProgress = 0;
  downloadedBytes = 0;
  totalBytes = 0;
  renderUpdateModal();

  try {
    await pendingUpdate.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          totalBytes = event.data.contentLength ?? 0;
          break;
        case "Progress":
          downloadedBytes += event.data.chunkLength;
          if (totalBytes > 0) {
            downloadProgress = Math.min(100, (downloadedBytes / totalBytes) * 100);
          }
          renderUpdateModal();
          break;
        case "Finished":
          updatePhase = "installing";
          renderUpdateModal();
          break;
      }
    });
    // If downloadAndInstall resolves, the update is installed — relaunch
    await relaunch();
  } catch (e) {
    updatePhase = "error";
    errorMessage = e instanceof Error ? e.message : String(e);
    renderUpdateModal();
  }
}

// ── public API ───────────────────────────────────────────────────────────
/** Called once at startup from main.ts. Silently checks for an app update;
 *  if one exists, opens the modal. */
export async function checkForAppUpdate() {
  try {
    updatePhase = "checking";
    const update = await check();
    if (update) {
      pendingUpdate = update;
      updatePhase = "available";
      openUpdateModal();
    } else {
      updatePhase = "idle";
    }
  } catch (e) {
    // Network errors during silent startup check → ignore
    console.warn("App update check failed:", e);
    updatePhase = "idle";
  }
}
