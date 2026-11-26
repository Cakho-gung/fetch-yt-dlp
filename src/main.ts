import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";
import { getCurrentWindow } from "@tauri-apps/api/window";
import type {
  AnalyzeResult,
  Binaries,
  CookieImportResult,
  HelpTopic,
  HistoryItem,
  Installers,
  QuickMeta,
  Screen,
  ToolUpdate,
  UpdateCheck,
} from "./types";
import {
  IC_ALERT,
  IC_ARROW_UP,
  IC_CHECK,
  IC_DOWNLOAD,
  IC_FOLDER,
  IC_LOADER,
  IC_REFRESH,
  IC_SETTINGS,
  IC_X,
  corners,
  svg,
} from "./icons";
import {
  $,
  BREW_INSTALL_CMD,
  COOKIE_BROWSERS,
  audioExtLabel,
  body,
  detectSource,
  esc,
  fmtBytes,
  fmtDuration,
  fmtViews,
  isCookieError,
  isFormatUnavailableError,
  isHomebrewMissingError,
  isLikelyMixUrl,
  isLikelyUrl,
  relTime,
} from "./utils";
import {
  canAnalyze,
  cookieParams,
  currentSelection,
  loadHistory,
  loadSettings,
  missingTools,
  persistHistory,
  persistSettings,
  state,
} from "./state";

// ── rendering ────────────────────────────────────────────────────────────
// tracks the last screen the entrance animation actually played for, so
// in-place updates (tab switch, quality pick, checkbox, playlist selection)
// don't replay the whole-block fade/slide — only a real screen change does.
let lastAnimScreen: Screen | null = null;
function render() {
  updateStatusTag();
  const el = body();
  // The playlist entry list re-renders from scratch on every small
  // interaction (ticking a row, select-all, switching tab/quality) since
  // it's all one innerHTML swap — save + restore its scroll position across
  // that so ticking a checkbox doesn't jump the list back to the top.
  const listScroll = el.querySelector<HTMLElement>(".playlist-rows")?.scrollTop;
  const urlEl = document.getElementById("urlInput") as HTMLInputElement | null;
  const urlHadFocus = !!urlEl && document.activeElement === urlEl;
  const urlSelStart = urlEl?.selectionStart ?? null;
  const urlSelEnd = urlEl?.selectionEnd ?? null;
  switch (state.screen) {
    case "empty":
    case "analyzing":
      el.innerHTML = viewEmpty();
      break;
    case "analyzed":
      el.innerHTML = viewAnalyzed();
      break;
    case "playlist":
      el.innerHTML = viewPlaylist();
      break;
    case "mix":
      el.innerHTML = viewMix();
      break;
    case "downloading":
      el.innerHTML = viewDownloading();
      break;
    case "installing":
      el.innerHTML = viewInstalling();
      break;
  }
  wireEvents();
  renderSettingsModal();
  if (urlHadFocus) {
    const n = document.getElementById("urlInput") as HTMLInputElement | null;
    if (n) {
      n.focus();
      if (urlSelStart !== null && urlSelEnd !== null) n.setSelectionRange(urlSelStart, urlSelEnd);
    }
  }
  if (listScroll) {
    const list = el.querySelector<HTMLElement>(".playlist-rows");
    if (list) list.scrollTop = listScroll;
  }
  // replay the entrance animation only on an actual screen change — toggling
  // the class forces a reflow so it retriggers even though #body itself never
  // leaves the DOM. In-place updates within the same screen (tab switch,
  // quality pick, checkboxes, playlist row toggles) skip this so the whole
  // block doesn't fade/slide again for every small interaction.
  if (state.screen !== lastAnimScreen) {
    el.classList.remove("anim-in");
    void el.offsetWidth;
    el.classList.add("anim-in");
    lastAnimScreen = state.screen;
  }
}

function updateStatusTag() {
  const tag = $("#statusTag");
  if (!tag) return;
  if (state.screen === "downloading") {
    tag.innerHTML = `<span class="tag tag-accent">1 ACTIVE</span>`;
  } else {
    const miss = missingTools();
    tag.innerHTML = miss.length
      ? `<span class="tag tag-neutral">${esc(miss[0])} MISSING</span>`
      : "";
  }
}

function missingBinariesWarn(): string {
  const need = missingTools();
  if (need.length === 0) return "";
  const canAuto = !!state.installers?.available;
  const mgr = state.installers?.manager || "brew";
  const needLabel = need.join(" + ").toUpperCase();

  let label: string;
  let desc: string;
  if (mgr === "fetch" || mgr === "mac") {
    label = `${svg(IC_DOWNLOAD, 13)} DOWNLOAD ${needLabel}`;
    desc = `FETCH downloads portable static binaries into its own folder — no system install, no compiling.`;
  } else {
    label = `${svg(IC_DOWNLOAD, 13)} INSTALL WITH BREW`;
    desc = `FETCH will run <code>brew</code> for you — ffmpeg can take a few minutes.`;
  }

  // Manual fallback (no supported auto-installer). spotDL isn't a brew formula,
  // so point at pip for it and brew for the rest.
  const manualBits: string[] = [];
  const brewables = need.filter((t) => t !== "spotdl");
  if (brewables.length) manualBits.push(`<code>brew install ${brewables.join(" ")}</code>`);
  if (need.includes("spotdl")) manualBits.push(`<code>pip install spotdl</code>`);

  let actions: string;
  if (canAuto) {
    actions = `<div style="display:flex;align-items:center;gap:var(--space-2);margin-top:var(--space-2)">
         <button class="btn btn-primary" id="installBtn" style="font-size:12.5px">${label}</button>
         <span class="text-muted" style="font-size:11.5px">${desc}</span>
       </div>`;
  } else {
    actions = `<br>Install with ${manualBits.join(" and ")}, then reopen FETCH (make sure they're on PATH).`;
  }
  return `<div class="warn">
    <span>${svg(IC_ALERT, 14)}</span>
    <div>Missing <b>${need.join(" + ")}</b> — this link won't analyze or download until installed.${actions}</div>
  </div>`;
}

function viewInstalling(): string {
  const stillMissing = missingTools();
  const targets = state.installTargets.length ? state.installTargets : stillMissing;
  const logLines = state.installLog.slice(-200).map(esc).join("\n");
  const done = state.installDone;
  const errored = !!state.installError;
  // Version updates re-resolve the binary on the very next command (no
  // caching to invalidate), so — unlike a first-time install — they never
  // need a restart.
  const needsRestart = done && !state.installIsUpdate && stillMissing.length > 0;
  const verb = state.installIsUpdate ? "UPDATING" : "INSTALLING";
  const doneLabel = state.installIsUpdate ? "UPDATED ✓" : "INSTALLED ✓";
  const failLabel = state.installIsUpdate ? "UPDATE FAILED" : "INSTALL FAILED";
  return `
  <div class="blueprint job" style="border-color:var(--color-accent)">${corners}
    <div class="job-top">
      <div class="job-info">
        <span class="job-name">${errored ? failLabel : done ? doneLabel : `${verb} ${targets.join(" + ").toUpperCase()}…`
    }</span>
        <span class="job-stage text-muted">${errored
      ? esc(state.installError)
      : needsRestart
        ? "FETCH needs to restart to pick this up"
        : done
          ? (state.installIsUpdate ? "up to date" : "tools are ready")
          : "downloading — please wait"
    }</span>
      </div>
      ${needsRestart
      ? `<button class="btn btn-primary" id="installRestart" style="display:inline-flex;align-items:center;gap:5px">${svg(IC_REFRESH, 13)} RESTART APP</button>`
      : done || errored
        ? `<div style="display:flex;gap:var(--space-2)">
               ${errored && isHomebrewMissingError(state.installError) ? `<button class="btn btn-secondary" id="installLearnHow">LEARN HOW</button>` : ""}
               <button class="btn ${done ? "btn-primary" : "btn-secondary"}" id="installBack">${done ? "CONTINUE" : "BACK"}</button>
             </div>`
        : `<button class="btn btn-secondary" id="installCancel">CANCEL</button>`
    }
    </div>
    ${done || errored
      ? ""
      : `<div class="bar indeterminate"><span></span></div>`
    }
    <pre class="install-log" id="installLog">${logLines || "starting…"}</pre>
  </div>`;
}

// Thumbnail + metadata are always embedded into the media file by the
// backend; these checkboxes only control whether extra sidecar files
// (thumbnail image, .info.json, .srt) get written alongside it.
function optsRow(): string {
  // Spotify (spotDL) always embeds cover art + full tags and has no
  // "description" concept, so the only extra sidecar it offers is synced
  // lyrics (.lrc) — reuse the same writeSubs flag / #cbSrt handler for it.
  if (state.analysis?.extractor === "Spotify") {
    const lrc = `<label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbSrt" ${state.writeSubs ? "checked" : ""}><span class="dot"></span>Lyrics (.lrc)</label>`;
    return `<div class="opts-row">${lrc}</div>`;
  }
  const thumb = `<label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbThumb" ${state.writeThumbnail ? "checked" : ""}><span class="dot"></span>Thumbnail file</label>`;
  const desc = `<label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbDesc" ${state.writeDescription ? "checked" : ""}><span class="dot"></span>Description file</label>`;
  const subs = `<label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbSrt" ${state.writeSubs ? "checked" : ""}><span class="dot"></span>Subtitle file</label>`;
  return `<div class="opts-row">${thumb}${desc}${state.tab === "video" ? subs : ""}</div>`;
}

// The Video/Audio toggle. For audio-only sources (Spotify) there are no video
// tiers, so only the Audio segment is shown.
function fmtSegHtml(): string {
  const hasVideo = (state.analysis?.videoOptions.length ?? 0) > 0;
  const audio = `<label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt" value="audio" ${state.tab === "audio" ? "checked" : ""}>Audio</label>`;
  if (!hasVideo) {
    return `<div class="seg" style="align-self:stretch">${audio}</div>`;
  }
  return `<div class="seg" style="align-self:stretch">
        <label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt" value="video" ${state.tab === "video" ? "checked" : ""}>Video</label>
        ${audio}
      </div>`;
}

function dirRow(): string {
  return `<div class="dir-row">
    <span class="text-muted mono">${svg(IC_FOLDER, 14)}</span>
    <span class="path" title="${esc(state.outputDir)}">${esc(state.outputDir)}</span>
    <button class="btn btn-ghost" id="pickDir">CHANGE…</button>
  </div>`;
}

// ── cookies: import-from-browser + manual file ──────────────────────────
function cookieFileSectionHtml(): string {
  return `<div style="margin-left:24px;display:flex;flex-direction:column;gap:var(--space-2);width:100%">
    <div class="dir-row">
      <span class="text-muted mono">${svg(IC_FOLDER, 14)}</span>
      <span class="path" title="${esc(state.cookieFile)}">${state.cookieFile ? esc(state.cookieFile) : "no cookie file yet"}</span>
      <button class="btn btn-ghost" id="pickCookieFile">CHOOSE FILE…</button>
    </div>
    <div class="dir-row">
      <button class="btn btn-ghost" id="openCookieImportBtn">${svg(IC_REFRESH, 13)} IMPORT FROM BROWSER…</button>
    </div>
    ${cookieImportStatusHtml()}
  </div>`;
}

function cookieImportStatusHtml(): string {
  const s = state.cookieImport;
  if (s.state === "idle" || s.state === "busy" || !s.message) return "";
  const icon = s.state === "error" ? svg(IC_ALERT, 12, 2) : svg(IC_CHECK, 12, 2.5);
  return `<p class="text-muted" style="font-size:11.5px;margin:0;display:flex;align-items:center;gap:6px">${icon}<span>${esc(s.message)}</span></p>`;
}

// A separate view swapped into the same #settingsOverlay modal (rather than
// a second overlay) — its own confirm step, since asking the user through
// window.confirm() gave no visual cue in the Tauri webview.
function cookieImportModalHtml(): string {
  const browser = state.cookieImportModal.browser;
  const label = browser[0].toUpperCase() + browser.slice(1);
  const s = state.cookieImport;
  const busy = s.state === "busy";
  const settled = s.state === "done" || s.state === "error";

  const bodyHtml = settled
    ? `<div class="settings-group">
        <p class="text-muted" style="font-size:12.5px;display:flex;align-items:center;gap:8px;margin:0">
          ${s.state === "error" ? svg(IC_ALERT, 16, 2) : svg(IC_CHECK, 16, 2.5)}
          <span>${esc(s.message)}</span>
        </p>
      </div>`
    : busy
      ? `<div class="settings-group">
          <p class="text-muted" style="font-size:12.5px;display:flex;align-items:center;gap:8px;margin:0">
            ${svg(IC_LOADER, 14)}<span>${esc(s.message || "Working…")}</span>
          </p>
          <p class="text-muted" style="font-size:11.5px;margin:var(--space-2) 0 0">
            ${state.cookieImportStopRequested
        ? "Stopping…"
        : "This can take a couple of minutes the first time — macOS has to verify the yt-dlp binary before it runs. Feel free to stop it below."
      }
          </p>
        </div>`
      : `<div class="settings-group">
          <span class="section-label">BROWSER</span>
          <select class="input" id="cookieImportBrowserSelect" style="width:100%;margin-top:var(--space-1)">
            ${COOKIE_BROWSERS.map(
        (b) => `<option value="${b}" ${browser === b ? "selected" : ""}>${b[0].toUpperCase()}${b.slice(1)}</option>`
      ).join("")}
          </select>
        </div>
        <div class="settings-group">
          <p class="text-muted" style="font-size:12.5px;margin:0">
            FETCH will close <b>${label}</b> if it's currently open, read its saved cookies, then reopen it automatically. Any unsaved tabs/forms in it could be lost.
          </p>
          <p class="text-muted" style="font-size:11.5px;margin:var(--space-2) 0 0">
            This can take up to 2–3 minutes on the first run.
          </p>
        </div>`;

  const foot = settled
    ? `${s.state === "error" ? `<button class="btn btn-ghost" id="cookieImportRetry">TRY AGAIN</button>` : ""}
       <button class="btn btn-primary" id="cookieImportCloseBtn">CLOSE</button>`
    : busy
      ? `<button class="btn btn-ghost" id="cookieImportStop" ${state.cookieImportStopRequested ? "disabled" : ""}>
           ${state.cookieImportStopRequested ? "STOPPING…" : "STOP"}
         </button>`
      : `<button class="btn btn-ghost" id="cookieImportCancel">CANCEL</button>
         <button class="btn btn-primary" id="cookieImportStart">CONTINUE</button>`;

  // No X while busy: closing mid-run would orphan a running extraction
  // (and a closed browser) with no visible way back in — STOP is the only
  // way out until it settles.
  return `
    <div class="modal">
      <div class="modal-head">
        <span class="section-label">IMPORT COOKIES FROM BROWSER</span>
        ${busy ? "" : `<span class="icon-x" id="cookieImportClose">${svg(IC_X, 14)}</span>`}
      </div>
      <div class="modal-body">${bodyHtml}</div>
      <div class="modal-foot">${foot}</div>
    </div>`;
}

function openCookieImportModal() {
  state.cookieImportModal = { open: true, browser: state.cookieBrowser, id: null };
  state.cookieImport = { state: "idle", message: "" };
  renderSettingsModal();
}

// ── settings modal ──────────────────────────────────────────────────────
function renderSettingsModal() {
  const overlay = document.getElementById("settingsOverlay");
  if (!overlay || !state.settingsOpen) return;
  if (state.cookieImportModal.open) {
    overlay.innerHTML = cookieImportModalHtml();
    wireCookieImportEvents();
    return;
  }
  overlay.innerHTML = `
    <div class="modal">
      <div class="modal-head">
        <span class="section-label">SETTINGS</span>
        <span class="icon-x" id="settingsClose">${svg(IC_X, 14)}</span>
      </div>
      <div class="modal-body">
        <div class="settings-group">
          <span class="section-label">DOWNLOAD LOCATION</span>
          <div class="dir-row">
            <span class="text-muted mono">${svg(IC_FOLDER, 14)}</span>
            <span class="path" title="${esc(state.outputDir)}">${esc(state.outputDir)}</span>
            <button class="btn btn-ghost" id="settingsPickDir">CHANGE…</button>
          </div>
        </div>
        <div class="settings-group">
          <span class="section-label">COOKIES</span>
          <p class="text-muted" style="font-size:12px;margin:var(--space-1) 0 var(--space-2)">Needed for age-restricted, members-only or private videos.</p>
          <div class="opts-row">
            <label class="radio" style="font-size:13px"><input type="radio" name="cookieMode" value="none" ${state.cookieMode === "none" ? "checked" : ""}><span class="dot"></span>None</label>
            <label class="radio" style="font-size:13px"><input type="radio" name="cookieMode" value="file" ${state.cookieMode === "file" ? "checked" : ""}><span class="dot"></span>Use cookies</label>
            ${state.cookieMode === "file" ? cookieFileSectionHtml() : ""}
          </div>
        </div>
        <div class="settings-group">
          <span class="section-label">TOOLS</span>
          ${toolUpdateRow("yt-dlp", state.updateCheck?.ytDlp)}
          ${toolUpdateRow("ffmpeg", state.updateCheck?.ffmpeg)}
          ${state.binaries?.spotdl ? toolUpdateRow("spotdl", state.updateCheck?.spotdl) : ""}
          <div class="dir-row" style="margin-top:var(--space-2)">
            <button class="btn btn-ghost" id="checkUpdatesBtn" ${state.checkingUpdates ? "disabled" : ""}>
              ${state.checkingUpdates ? "CHECKING…" : `${svg(IC_REFRESH, 13)} CHECK FOR UPDATES`}
            </button>
          </div>
        </div>
      </div>
      <div class="modal-foot">
        <button class="btn btn-primary" id="settingsDone">DONE</button>
      </div>
    </div>`;
  wireSettingsEvents();
}

function toolUpdateRow(tool: "yt-dlp" | "ffmpeg" | "spotdl", info: ToolUpdate | undefined): string {
  const label = tool;
  const current = info?.current ? esc(info.current) : "—";
  let status: string;
  if (!info) {
    status = "";
  } else if (info.updateAvailable) {
    status = `<span class="tag tag-accent">${esc(info.latest || "?")} available</span>`;
  } else if (info.latest) {
    status = `<span class="text-muted" style="font-size:11.5px">up to date</span>`;
  } else {
    status = `<span class="text-muted" style="font-size:11.5px">couldn't check</span>`;
  }
  const canUpdate = !!info?.updateAvailable && (!!state.installers?.available || info.source === "brew");
  return `<div class="dir-row" style="justify-content:space-between">
    <span style="display:flex;align-items:center;gap:var(--space-2)">
      <span class="mono" style="font-size:12.5px">${label} ${current}</span>
      ${status}
    </span>
    ${canUpdate
      ? `<button class="btn btn-ghost settings-update-btn" data-tool="${esc(tool)}" style="font-size:11.5px">UPDATE</button>`
      : ""
    }
  </div>`;
}

function wireSettingsEvents() {
  document.getElementById("settingsClose")?.addEventListener("click", closeSettings);
  document.getElementById("settingsDone")?.addEventListener("click", closeSettings);
  // onclick (not addEventListener): #settingsOverlay is a persistent node
  // that both this view and the cookie-import sub-view render into, so
  // addEventListener would stack a handler from each past render instead of
  // replacing it — including a stale unconditional-close one firing over a
  // busy import popup that's supposed to block closing.
  const overlay = document.getElementById("settingsOverlay");
  if (overlay) {
    overlay.onclick = (e) => {
      if (e.target === overlay) closeSettings();
    };
  }
  document.getElementById("settingsPickDir")?.addEventListener("click", pickDir);
  document.getElementById("pickCookieFile")?.addEventListener("click", pickCookieFile);
  document.getElementById("openCookieImportBtn")?.addEventListener("click", openCookieImportModal);
  document
    .querySelectorAll<HTMLInputElement>('input[name="cookieMode"]')
    .forEach((r) =>
      r.addEventListener("change", () => {
        state.cookieMode = r.value as "none" | "file";
        persistSettings();
        renderSettingsModal();
      })
    );
  document.getElementById("checkUpdatesBtn")?.addEventListener("click", async () => {
    state.checkingUpdates = true;
    renderSettingsModal();
    await checkForUpdates({ toast: false });
    state.checkingUpdates = false;
    renderSettingsModal();
  });
  document.querySelectorAll<HTMLButtonElement>(".settings-update-btn").forEach((btn) =>
    btn.addEventListener("click", () => {
      const tool = btn.dataset.tool!;
      closeSettings();
      startToolUpdate(tool);
    })
  );
}

function openSettings() {
  state.settingsOpen = true;
  const overlay = document.getElementById("settingsOverlay");
  if (!overlay) return;
  window.clearTimeout(settingsHideTimer);
  overlay.classList.remove("hidden");
  renderSettingsModal();
  requestAnimationFrame(() => overlay.classList.add("open"));
  loadInstalledVersions();
}
let settingsHideTimer: number | undefined;
function closeSettings() {
  state.settingsOpen = false;
  state.cookieImportModal.open = false;
  const overlay = document.getElementById("settingsOverlay");
  if (!overlay) return;
  overlay.classList.remove("open");
  settingsHideTimer = window.setTimeout(() => {
    overlay.classList.add("hidden");
    overlay.innerHTML = "";
  }, 180);
}

// ── help / troubleshooting modal ─────────────────────────────────────────
// Opened from the "LEARN MORE" button on an error toast (or "LEARN HOW" on a
// failed install). "download" covers the common reasons a download fails —
// cookies first, since that's the usual culprit for sites like Douyin/TikTok
// and for age-restricted/private videos. "homebrew" walks through installing
// Homebrew by hand when FETCH can't do it for the user.
let helpHideTimer: number | undefined;

function homebrewHelpHtml(): string {
  return `
    <div class="modal">
      <div class="modal-head">
        <span class="section-label">HOMEBREW REQUIRED</span>
        <span class="icon-x" id="helpClose">${svg(IC_X, 14)}</span>
      </div>
      <div class="modal-body">
        <div class="settings-group">
          <p class="text-muted" style="font-size:12.5px;margin:0 0 var(--space-2)">
            FETCH uses <b>Homebrew</b> to install ffmpeg on macOS — and, on Intel Macs, to get a
            modern-enough Python for Spotify links. It's a one-time setup.
          </p>
          <p class="text-muted" style="font-size:12.5px;margin:0 0 var(--space-1)">
            Open <b>Terminal</b>, paste this, and press Enter (it'll ask for your Mac password):
          </p>
          <div style="display:flex;gap:var(--space-2);align-items:flex-start;margin-bottom:var(--space-1)">
            <pre class="install-log" style="flex:1;margin:0;white-space:pre-wrap;word-break:break-all">${esc(BREW_INSTALL_CMD)}</pre>
            <button class="btn btn-secondary" id="helpCopyBrew" style="flex:none">COPY</button>
          </div>
          <p class="text-muted" style="font-size:11.5px;margin:0">
            Prefer to read first? <a href="#" id="helpBrewLink">brew.sh</a>
          </p>
        </div>
      </div>
      <div class="modal-foot">
        <button class="btn btn-primary" id="helpCheckAgain">✓ I'VE INSTALLED IT — CHECK AGAIN</button>
      </div>
    </div>`;
}

function helpModalHtml(highlightCookies: boolean): string {
  const hi = highlightCookies
    ? "border-color:var(--color-accent);background:var(--color-accent-100)"
    : "";
  return `
    <div class="modal">
      <div class="modal-head">
        <span class="section-label">WHY A DOWNLOAD FAILS</span>
        <span class="icon-x" id="helpClose">${svg(IC_X, 14)}</span>
      </div>
      <div class="modal-body">
        <div class="settings-group" style="border:1px solid var(--color-divider);${hi};padding:var(--space-3)">
          <span class="section-label">① COOKIES — the usual cause</span>
          <p class="text-muted" style="font-size:12.5px;margin:var(--space-1) 0 var(--space-2)">
            Some sites (Douyin, TikTok, plus age-restricted, members-only or private videos)
            need <b>fresh browser cookies</b> — even when you're not logged in. The site uses
            them as an anti-bot token.
          </p>
          <p class="text-muted" style="font-size:12.5px;margin:0 0 var(--space-2)">
            Log into that site in your browser once, then here:
            <b>Settings → Cookies → "Use cookies" → Import from browser</b> — FETCH reads the
            cookies straight from your browser (closing/reopening it if it's open) and saves
            them to a file, so you won't need the browser again until they expire.
          </p>
          <p class="text-muted" style="font-size:12.5px;margin:0 0 var(--space-2)">
            <b>If the import itself fails</b>: it'll tell you why (browser still in use, macOS
            Keychain blocked it, or no cookies found for that site). You can also point
            <b>"Choose file…"</b> at a <code>cookies.txt</code> exported by hand.
          </p>
          <button class="btn btn-primary" id="helpOpenSettings" style="font-size:24px;display:inline-flex;align-items:center;gap:8px">${svg(IC_SETTINGS, 22)} OPEN COOKIE SETTINGS</button>
        </div>
        <div class="settings-group">
          <span class="section-label">② OUT-OF-DATE TOOL</span>
          <p class="text-muted" style="font-size:12.5px;margin:var(--space-1) 0">
            Sites change constantly. If a link that used to work suddenly stops,
            update yt-dlp / spotDL in <b>Settings → Tools → Check for updates</b>.
          </p>
        </div>
        <div class="settings-group">
          <span class="section-label">③ REGION-LOCKED OR PRIVATE</span>
          <p class="text-muted" style="font-size:12.5px;margin:var(--space-1) 0">
            Some videos are blocked in your region or genuinely private —
            those can't be fetched no matter the settings.
          </p>
        </div>
      </div>
      <div class="modal-foot">
        <button class="btn btn-primary" id="helpDone">GOT IT</button>
      </div>
    </div>`;
}

function openHelp(topic: HelpTopic = "download", highlightCookies = false) {
  const overlay = document.getElementById("helpOverlay");
  if (!overlay) return;
  state.helpOpen = true;
  window.clearTimeout(helpHideTimer);
  overlay.classList.remove("hidden");
  overlay.innerHTML = topic === "homebrew" ? homebrewHelpHtml() : helpModalHtml(highlightCookies);
  document.getElementById("helpClose")?.addEventListener("click", closeHelp);
  if (topic === "homebrew") {
    document.getElementById("helpCopyBrew")?.addEventListener("click", copyBrewInstallCmd);
    document.getElementById("helpCheckAgain")?.addEventListener("click", recheckHomebrew);
    document.getElementById("helpBrewLink")?.addEventListener("click", (e) => {
      e.preventDefault();
      openUrl("https://brew.sh");
    });
  } else {
    document.getElementById("helpDone")?.addEventListener("click", closeHelp);
    document.getElementById("helpOpenSettings")?.addEventListener("click", () => {
      closeHelp();
      openSettings();
    });
  }
  // onclick (not addEventListener) so reopening the modal doesn't stack
  // duplicate backdrop handlers on the persistent overlay element.
  overlay.onclick = (e) => {
    if (e.target === overlay) closeHelp();
  };
  requestAnimationFrame(() => overlay.classList.add("open"));
}

async function copyBrewInstallCmd() {
  const btn = document.getElementById("helpCopyBrew");
  try {
    await navigator.clipboard.writeText(BREW_INSTALL_CMD);
    if (btn) {
      const original = btn.textContent;
      btn.textContent = "COPIED ✓";
      window.setTimeout(() => {
        if (document.getElementById("helpCopyBrew")) btn.textContent = original;
      }, 1500);
    }
  } catch { }
}

// Re-checks whether `brew` resolves now (and re-checks the tools themselves,
// in case the user also finished an install manually) without forcing a full
// app restart — the point of this button is to close the loop right here.
async function recheckHomebrew() {
  try {
    state.installers = await invoke<Installers>("detect_installers");
  } catch { }
  try {
    state.binaries = await invoke<Binaries>("check_binaries");
  } catch { }
  closeHelp();
  render();
}

function closeHelp() {
  state.helpOpen = false;
  const overlay = document.getElementById("helpOverlay");
  if (!overlay) return;
  overlay.classList.remove("open");
  helpHideTimer = window.setTimeout(() => {
    overlay.classList.add("hidden");
    overlay.innerHTML = "";
  }, 180);
}

async function pickCookieFile() {
  const picked = await openDialog({
    multiple: false,
    filters: [{ name: "Cookies", extensions: ["txt"] }],
  });
  if (typeof picked === "string") {
    state.cookieFile = picked;
    state.cookieImport = { state: "idle", message: "" };
    persistSettings();
    renderSettingsModal();
  }
}

function closeCookieImportModal() {
  state.cookieImportModal.open = false;
  renderSettingsModal();
}

function wireCookieImportEvents() {
  // onclick, not addEventListener — see the comment in wireSettingsEvents().
  const overlay = document.getElementById("settingsOverlay");
  if (overlay) {
    overlay.onclick = (e) => {
      if (e.target === overlay && state.cookieImport.state !== "busy") {
        closeCookieImportModal();
      }
    };
  }
  document.getElementById("cookieImportBrowserSelect")?.addEventListener("change", (e) => {
    state.cookieImportModal.browser = (e.target as HTMLSelectElement).value;
  });
  document.getElementById("cookieImportCancel")?.addEventListener("click", closeCookieImportModal);
  document.getElementById("cookieImportClose")?.addEventListener("click", closeCookieImportModal);
  document.getElementById("cookieImportCloseBtn")?.addEventListener("click", closeCookieImportModal);
  document.getElementById("cookieImportStart")?.addEventListener("click", importCookiesFromBrowser);
  document.getElementById("cookieImportStop")?.addEventListener("click", stopCookieImport);
  document.getElementById("cookieImportRetry")?.addEventListener("click", () => {
    state.cookieImport = { state: "idle", message: "" };
    renderSettingsModal();
  });
}

// Marks the import stopped right away rather than waiting on the backend
// round-trip — spawn()ing/killing yt-dlp can itself sit blocked for a while
// (macOS re-verifying the portable binary), so waiting for that confirmation
// before updating the UI made STOP feel just as stuck as the thing it's
// meant to escape. The kill request still goes out in the background: it's
// safe to walk away from, since `import_cookies_from_browser` reopens the
// browser and cleans up server-side regardless of whether anyone's still
// watching (see lib.rs) — `stillActive()` below just stops that eventual
// result from clobbering whatever the UI has moved on to.
function stopCookieImport() {
  const id = state.cookieImportModal.id;
  if (!id || state.cookieImportStopRequested) return;
  state.cookieImportStopRequested = true;
  state.cookieImportModal.id = null;
  state.cookieImport = { state: "error", message: "Stopped." };
  renderSettingsModal();
  invoke("cancel_cookie_import", { id }).catch(() => { });
}

// Snapshots cookies from the browser picked in the import popup into a file
// (closing/reopening the browser around it if it's currently running) and
// points cookieFile at the result — see `import_cookies_from_browser` in
// src-tauri/src/lib.rs. The popup itself (browser picker + "will close/
// reopen the browser" notice) is the confirmation step, so this runs
// straight through once "CONTINUE" is clicked.
async function importCookiesFromBrowser() {
  const browser = state.cookieImportModal.browser;
  state.cookieBrowser = browser; // remember the choice for next time
  const label = browser[0].toUpperCase() + browser.slice(1);
  const id = crypto.randomUUID();
  state.cookieImportModal.id = id;
  state.cookieImportStopRequested = false;
  // True as long as nothing else (STOP, or a fresh attempt) has taken over
  // since this one started — an already-stopped run's late result shouldn't
  // overwrite whatever's on screen now.
  const stillActive = () => state.cookieImportModal.id === id;

  state.cookieImport = { state: "busy", message: `Checking ${label}…` };
  renderSettingsModal();

  try {
    const running = await invoke<boolean>("browser_is_running", { browser });
    if (!stillActive()) return;
    state.cookieImport = {
      state: "busy",
      message: running ? `Closing ${label}, reading cookies, then reopening it…` : `Reading cookies from ${label}…`,
    };
    renderSettingsModal();

    const result = await invoke<CookieImportResult>("import_cookies_from_browser", {
      id,
      browser,
      probeUrl: state.analysis?.webpageUrl || state.url || null,
    });
    if (!stillActive()) return;

    state.cookieFile = result.path;
    persistSettings();
    state.cookieImport = {
      state: "done",
      message: result.warning ?? `Imported ${result.cookieCount} cookies from ${label}.`,
    };
  } catch (err) {
    if (!stillActive()) return;
    state.cookieImport = { state: "error", message: String(err) };
  }
  state.cookieImportModal.id = null;
  renderSettingsModal();
}

function historyStrip(): string {
  if (state.history.length === 0) {
    return `<div class="hist-strip">
      <span class="section-label">HISTORY</span>
      <span class="text-muted mono">nothing downloaded yet</span>
    </div>`;
  }
  return historyTable();
}

function historyTable(): string {
  const rows = state.history
    .slice(0, 6)
    .map(
      (h, i) => `<tr>
        <td style="width:52px"><span class="tag ${i === 0 ? "tag-accent" : "tag-neutral"}">${esc(h.ext.toUpperCase())}</span></td>
        <td style="max-width:300px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${esc(h.title)}</td>
        <td class="text-muted mono" style="white-space:nowrap;font-size:11px">${h.sizeLabel ? `${esc(h.sizeLabel)} · ` : ""}${relTime(h.at)}</td>
        <td style="width:70px;text-align:right">${h.path ? `<button class="btn btn-ghost open-hist" data-path="${esc(h.path)}" style="font-size:12px">OPEN</button>` : ""
        }</td>
      </tr>`
    )
    .join("");
  return `<div class="hist-box">
    <div class="hist-head">
      <span class="section-label">HISTORY</span>
      <span class="text-muted mono" style="font-size:11px">${Math.min(state.history.length, 6)} most recent</span>
    </div>
    <table class="table" style="font-size:13px"><tbody>${rows}</tbody></table>
  </div>`;
}

// input row (live) — used in empty & success
// Always the live, editable URL box — never swapped for a read-only chip or
// disabled once something's been analyzed. Submitting again (Enter or the
// button) just cancels whatever analyze is in flight and starts a new one;
// see doAnalyze(). Only the trailing status tag changes per screen.
function linkInputRow(placeholder: string): string {
  const a = state.analysis;
  let statusTag = "";
  if (a && state.screen === "playlist") {
    const count = a.playlistCount ?? a.entries.length;
    statusTag = `<span class="tag tag-accent">PLAYLIST · ${count} VIDEOS</span>`;
  } else if (state.screen === "mix") {
    statusTag = `<span class="tag tag-neutral" style="font-weight:700">MIX · ENDLESS</span>`;
  } else if (state.screen === "analyzing") {
    statusTag = `<span class="tag tag-neutral">ANALYZING…</span>`;
  } else if (a && state.screen === "analyzed") {
    const src = detectSource(a.webpageUrl);
    statusTag = `<span class="tag tag-accent">${esc(a.extractor.toUpperCase() || src)}</span>`;
  }
  return `<div class="link-row">
    <div class="link-input-wrap">
      <input class="input" id="urlInput" placeholder="${esc(placeholder)}" value="${esc(state.url)}" spellcheck="false" autocomplete="off">
      ${state.url ? `<span class="icon-x link-clear" id="clearLink" title="Clear">${svg(IC_X, 13)}</span>` : ""}
    </div>
    ${statusTag}
  </div>`;
}

// ── screens ──────────────────────────────────────────────────────────────
function viewEmpty(): string {
  const analyzing = state.screen === "analyzing";
  const src = state.url ? detectSource(state.url) : "";
  const subtitle = analyzing
    ? "analyzing link…"
    : state.url
      ? "press Enter to try again"
      : "no link yet — paste one to start";
  const p = state.preview;
  const leftCard =
    analyzing && p
      ? `<figure class="blueprint preview">${corners}
      <div class="thumb">
        ${p.thumbnail
        ? `<img src="${esc(p.thumbnail)}" alt="" onerror="this.style.display='none'">`
        : `<span class="ph">[ no thumbnail ]</span>`
      }
        ${p.durationSeconds ? `<span class="dur">${fmtDuration(p.durationSeconds)}</span>` : ""}
      </div>
      <div class="meta">
        <span class="ttl">${esc(p.title)}</span>
        <div class="sub">
          ${p.uploader ? `<span>${esc(p.uploader)}</span><span>·</span>` : ""}
          <span class="tag tag-neutral" style="margin-left:auto">LOADING QUALITIES…</span>
        </div>
      </div>
    </figure>`
      : `<div class="blueprint dropzone" style="cursor:pointer">${corners}
      <div class="inner">
        <span style="font-size:24px;display:flex;align-items:center;justify-content:center">${analyzing ? svg(IC_LOADER, 28) : svg(IC_DOWNLOAD, 28)}</span>
        <span class="mono" style="font-size:11px">${subtitle}</span>
        ${state.url && !analyzing ? `<span class="tag tag-accent" style="margin-top:6px">${src} detected</span>` : ""}
      </div>
    </div>`;
  return `
  ${missingBinariesWarn()}
  ${linkInputRow("Paste a video, music or playlist link…")}
  <div class="grid-analyze">
    ${leftCard}
    <div class="card locked">
      <div class="seg" style="align-self:stretch">
        <label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt" checked>Video</label>
        <label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt">Audio</label>
      </div>
      <div class="qgrid">
        ${["BEST", "1080P", "720P", "480P"]
      .map(
        (q) =>
          `<div class="qcell"><span class="q">${q}</span></div>`
      )
      .join("")}
      </div>
      <button class="btn btn-primary btn-block" disabled>DOWNLOAD</button>
    </div>
  </div>
  ${historyStrip()}`;
}

function viewAnalyzed(): string {
  const a = state.analysis!;
  if (state.tab === "video" && a.videoOptions.length === 0 && a.audioOptions.length > 0) {
    state.tab = "audio";
  }
  const opts = state.tab === "video" ? a.videoOptions : a.audioOptions;
  if (!state.selectedQualityId && opts.length) {
    state.selectedQualityId = opts.find((o) => o.id === "1080p")?.id ?? opts[0].id;
  }
  const selected = opts.find((o) => o.id === state.selectedQualityId) ?? opts[0];
  if (!selected) return "";
  return `
  ${linkInputRow("Paste a video, music or playlist link…")}
  <div class="grid-analyze">
    <figure class="blueprint preview">${corners}
      <div class="thumb">
        ${a.thumbnail
      ? `<img src="${esc(a.thumbnail)}" alt="" onerror="this.style.display='none';this.nextElementSibling.style.display='block'"><span class="ph" style="display:none">[ thumbnail unavailable ]</span>`
      : `<span class="ph">[ no thumbnail ]</span>`
    }
        ${a.durationSeconds ? `<span class="dur">${fmtDuration(a.durationSeconds)}</span>` : ""}
      </div>
      <div class="meta">
        <span class="ttl">${esc(a.title)}</span>
        <div class="sub">
          ${a.uploader ? `<span>${esc(a.uploader)}</span><span>·</span>` : ""}
          ${a.viewCount != null ? `<span>${fmtViews(a.viewCount)}</span>` : ""}
          <span class="tag tag-accent" style="margin-left:auto">${state.tab === "video" ? "VIDEO · MP4" : "AUDIO"}</span>
        </div>
      </div>
    </figure>
    <div class="card">
      ${fmtSegHtml()}
      <div class="qgrid">
        ${opts
      .map(
        (o) =>
          `<div class="qcell" data-q="${o.id}" role="button" aria-selected="${o.id === selected.id}">
                 <span class="q">${esc(o.label)}</span>
               </div>`
      )
      .join("")}
        ${state.tab === "audio" && a.videoOptions.length > opts.length
      ? Array(a.videoOptions.length - opts.length)
        .fill(`<div class="qcell" style="visibility:hidden;pointer-events:none"></div>`)
        .join("")
      : ""
    }
      </div>
      ${optsRow()}
      <button class="btn btn-primary btn-block" id="downloadBtn" style="display:inline-flex;align-items:center;gap:6px">${svg(IC_DOWNLOAD, 15)} DOWNLOAD · ${esc(selected.label)}</button>
    </div>
  </div>
  ${dirRow()}
  ${historyStrip()}`;
}

function viewPlaylist(): string {
  const a = state.analysis!;
  if (state.tab === "video" && a.videoOptions.length === 0 && a.audioOptions.length > 0) {
    state.tab = "audio";
  }
  const opts = state.tab === "video" ? a.videoOptions : a.audioOptions;
  if (!state.selectedQualityId && opts.length) {
    state.selectedQualityId = opts.find((o) => o.id === "1080p")?.id ?? opts[0].id;
  }
  const selected = opts.find((o) => o.id === state.selectedQualityId) ?? opts[0];
  if (!selected) return "";
  const count = a.playlistCount ?? a.entries.length;
  const selCount = state.entrySelected.filter(Boolean).length;
  const rows = a.entries
    .map((e, i) => {
      const sel = state.entrySelected[i];
      return `<label class="queue-row" style="cursor:pointer;${sel ? "background:var(--color-accent-100)" : ""}" data-entry="${i}">
        <span style="width:14px;height:14px;flex:none;display:grid;place-items:center;font-size:9px;${sel
          ? "background:var(--color-accent);color:var(--color-bg)"
          : "border:1px solid var(--color-neutral-400)"
        }">${sel ? "✓" : ""}</span>
        <span class="idx mono">${String(e.index).padStart(2, "0")}</span>
        <span class="nm" style="${sel ? "" : "color:var(--color-neutral-700)"}">${esc(e.title)}</span>
        <span class="text-muted mono" style="font-size:11px">${fmtDuration(e.durationSeconds)}</span>
      </label>`;
    })
    .join("");
  return `
  ${linkInputRow("Paste a video, music or playlist link…")}
  <div class="grid-analyze">
    <div class="blueprint preview playlist-box">${corners}
      <div class="playlist-head">
        <span class="ttl" title="${esc(a.title)}">${esc(a.title)}</span>
        <span class="text-muted mono" style="font-size:11px">${count} videos${a.entries.length < count ? ` · showing first ${a.entries.length}` : ""}</span>
        <a href="#" id="selectAll">${selCount === a.entries.length ? "Clear all" : "Select all"}</a>
      </div>
      <div class="playlist-rows">${rows}</div>
      <div class="playlist-foot">
        <span class="mono" style="font-size:12px;color:var(--color-accent-700)">${selCount}/${a.entries.length} selected</span>
      </div>
    </div>
    <div class="card">
      ${fmtSegHtml()}
      <div class="qgrid">
        ${opts
      .map(
        (o) =>
          `<div class="qcell" data-q="${o.id}" role="button" aria-selected="${o.id === selected.id}">
                 <span class="q">${esc(o.label)}</span>
               </div>`
      )
      .join("")}
        ${state.tab === "audio" && a.videoOptions.length > opts.length
      ? Array(a.videoOptions.length - opts.length)
        .fill(`<div class="qcell" style="visibility:hidden;pointer-events:none"></div>`)
        .join("")
      : ""
    }
      </div>
      ${optsRow()}
      <button class="btn btn-primary btn-block" id="downloadPlaylist" ${selCount ? "" : "disabled"} style="display:inline-flex;align-items:center;gap:6px">${svg(IC_DOWNLOAD, 15)} DOWNLOAD ${selCount} SELECTED</button>
    </div>
  </div>
  ${dirRow()}`;
}

function viewMix(): string {
  return `
  ${linkInputRow("Paste a video, music or playlist link…")}
  <div class="card" style="gap:var(--space-3);padding:var(--space-4)">
    <div>
      <h4 style="margin-bottom:var(--space-1);font-size:17px">THIS IS A MIX — AN AUTO-GENERATED LIST WITH NO END</h4>
      <p class="text-muted" style="margin:0;font-size:13px">"Download all" isn't possible. Grab just this video, or cap it at the first N entries of the mix.</p>
    </div>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:var(--space-3)">
      <div class="mix-opt" data-mix="single" style="border:1px solid ${state.mixMode === "single" ? "var(--color-accent)" : "var(--color-divider)"};${state.mixMode === "single" ? "background:var(--color-accent-100)" : ""};padding:var(--space-3);display:flex;flex-direction:column;gap:var(--space-2);cursor:pointer">
        <label class="radio"><input type="radio" name="mix" ${state.mixMode === "single" ? "checked" : ""}><span class="dot"></span><span style="font:600 14px var(--font-heading);${state.mixMode === "single" ? "color:var(--color-accent-800)" : ""}">THIS VIDEO ONLY</span></label>
        <span style="font-size:12px;color:var(--color-neutral-700)">Skip the mix, download the current video (--no-playlist)</span>
      </div>
      <div class="mix-opt" data-mix="capped" style="border:1px solid ${state.mixMode === "capped" ? "var(--color-accent)" : "var(--color-divider)"};${state.mixMode === "capped" ? "background:var(--color-accent-100)" : ""};padding:var(--space-3);display:flex;flex-direction:column;gap:var(--space-2);cursor:pointer">
        <label class="radio"><input type="radio" name="mix" ${state.mixMode === "capped" ? "checked" : ""}><span class="dot"></span><span style="font:600 14px var(--font-heading)">FIRST N OF THE MIX</span></label>
        <div class="seg">
          ${[5, 10, 15, 25]
      .map(
        (n) =>
          `<label class="seg-opt mixn" data-n="${n}"><input type="radio" name="mixn" ${state.mixN === n ? "checked" : ""}>${n}</label>`
      )
      .join("")}
        </div>
      </div>
    </div>
    <button class="btn btn-primary btn-block" id="mixContinue">CONTINUE → PICK QUALITY</button>
  </div>
  ${dirRow()}`;
}

function formatStageText(stage: string): string {
  switch (stage) {
    case "merging":
      return "merging streams (FFmpeg)…";
    case "extracting":
      return "extracting audio (FFmpeg)…";
    case "installing ffmpeg…":
      return "installing ffmpeg (one-time)…";
    case "updating yt-dlp…":
      return "updating yt-dlp…";
    case "starting":
      return "starting…";
    default:
      return "downloading";
  }
}

function viewDownloading(): string {
  const j = state.job!;
  const indeterminate = j.percent <= 0 || j.stage !== "downloading";
  const stageText = formatStageText(j.stage);
  return `
  <div class="link-row" style="opacity:.6">
    <input class="input mono" style="flex:1;min-height:42px;font-size:13px" placeholder="Paste another link when this finishes" disabled>
  </div>
  <div class="blueprint job">${corners}
    <div class="job-top">
      <div class="job-thumb"></div>
      <div class="job-info">
        <span class="job-name">${esc(j.name)}</span>
        <span class="job-stage text-muted">${stageText}</span>
      </div>
      <button class="btn btn-secondary btn-icon" id="cancelBtn" title="Cancel">${svg(IC_X, 14)}</button>
    </div>
    <div style="display:flex;flex-direction:column;gap:var(--space-2)">
      <div class="bar ${indeterminate ? "indeterminate" : ""}" id="bar"><span id="barFill" style="width:${Math.max(0, Math.min(100, j.percent))}%"></span></div>
      <div class="bar-meta">
        <span class="pct" id="jobPct">${indeterminate ? stageText : `${j.percent.toFixed(0)}%`}</span>
        <span class="text-muted" id="jobEta">${j.speed}${j.eta ? ` · ${j.eta} left` : ""}</span>
      </div>
    </div>
  </div>`;
}

// ── event wiring (re-run after every render) ─────────────────────────────
function wireEvents() {
  $("#urlInput")?.addEventListener("input", (e) => {
    const prevSpotify = detectSource(state.url) === "SPOTIFY";
    state.url = (e.target as HTMLInputElement).value;
    const nextSpotify = detectSource(state.url) === "SPOTIFY";
    if (prevSpotify !== nextSpotify && state.screen === "empty") render();
    scheduleAutoAnalyze();
  });
  $("#urlInput")?.addEventListener("paste", (e) => {
    const text = e.clipboardData?.getData("text")?.trim();
    if (text && isLikelyUrl(text)) {
      state.url = text;
      setTimeout(() => {
        if (isLikelyUrl(state.url)) doAnalyze({ auto: true });
      }, 0);
    }
  });
  $("#urlInput")?.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter" && isLikelyUrl(state.url)) doAnalyze({ force: true });
  });
  body().querySelector<HTMLElement>(".dropzone")?.addEventListener("click", () => {
    if (isLikelyUrl(state.url)) {
      doAnalyze({ force: true });
    } else {
      $("#urlInput")?.focus();
    }
  });
  $("#clearLink")?.addEventListener("click", resetToEmpty);
  $("#pickDir")?.addEventListener("click", pickDir);
  $("#downloadBtn")?.addEventListener("click", startDownload);
  $("#cancelBtn")?.addEventListener("click", cancelDownload);
  $("#installBtn")?.addEventListener("click", startInstall);
  $("#installCancel")?.addEventListener("click", cancelInstall);
  $("#installBack")?.addEventListener("click", () => (state.installIsUpdate ? finishToolUpdate() : finishInstall()));
  $("#installRestart")?.addEventListener("click", restartApp);
  $("#setupHomebrewBtn")?.addEventListener("click", () => openHelp("homebrew"));
  $("#installLearnHow")?.addEventListener("click", () => openHelp("homebrew"));

  // format tabs (video/audio)
  body()
    .querySelectorAll<HTMLInputElement>('input[name="fmt"]')
    .forEach((r) =>
      r.addEventListener("change", () => {
        if (state.screen !== "analyzed" && state.screen !== "playlist") return;
        state.tab = (r.value as "video" | "audio") ?? "video";
        state.selectedQualityId = "";
        render();
      })
    );

  // quality cells — targeted DOM patch so thumbnails don't flicker
  body()
    .querySelectorAll<HTMLElement>(".qcell[data-q]")
    .forEach((c) =>
      c.addEventListener("click", () => {
        state.selectedQualityId = c.dataset.q!;
        patchQualitySelection();
      })
    );

  $("#cbThumb")?.addEventListener("change", (e) => {
    state.writeThumbnail = (e.target as HTMLInputElement).checked;
  });
  $("#cbDesc")?.addEventListener("change", (e) => {
    state.writeDescription = (e.target as HTMLInputElement).checked;
  });
  $("#cbSrt")?.addEventListener("change", (e) => {
    state.writeSubs = (e.target as HTMLInputElement).checked;
  });

  // open folder buttons (history + success)
  body()
    .querySelectorAll<HTMLElement>(".open-hist")
    .forEach((b) =>
      b.addEventListener("click", () => invoke("reveal_in_folder", { path: b.dataset.path }))
    );

  // playlist — targeted DOM patch instead of wiping the list on every click
  body()
    .querySelectorAll<HTMLElement>("[data-entry]")
    .forEach((row) =>
      row.addEventListener("click", (e) => {
        e.preventDefault();
        const i = Number(row.dataset.entry);
        state.entrySelected[i] = !state.entrySelected[i];
        patchPlaylistSelection();
      })
    );
  $("#selectAll")?.addEventListener("click", (e) => {
    e.preventDefault();
    const all = state.entrySelected.every(Boolean);
    state.entrySelected = state.entrySelected.map(() => !all);
    patchPlaylistSelection();
  });
  $("#downloadPlaylist")?.addEventListener("click", startPlaylistDownload);

  // mix
  body()
    .querySelectorAll<HTMLElement>(".mix-opt")
    .forEach((o) =>
      o.addEventListener("click", () => {
        state.mixMode = (o.dataset.mix as "single" | "capped") ?? "single";
        render();
      })
    );
  body()
    .querySelectorAll<HTMLElement>(".mixn")
    .forEach((o) =>
      o.addEventListener("click", () => {
        state.mixN = Number(o.dataset.n);
        state.mixMode = "capped";
        render();
      })
    );
  $("#mixContinue")?.addEventListener("click", mixContinue);
}

// ── actions ──────────────────────────────────────────────────────────────
// Kills whatever analyze is currently running on the backend, if any — used
// whenever the link changes or the user backs out, so a stale request can
// never land and clobber what's on screen.
let lastRequestedUrl = "";
let autoAnalyzeTimer: number | undefined;

// Typing/pasting a link analyzes it automatically once the input settles —
// there is no separate ANALYZE button.
function scheduleAutoAnalyze() {
  window.clearTimeout(autoAnalyzeTimer);
  if (!isLikelyUrl(state.url) || !canAnalyze()) return;
  if (state.screen === "downloading" || state.screen === "installing") return;
  autoAnalyzeTimer = window.setTimeout(() => doAnalyze({ auto: true }), 450);
}

function cancelInFlightAnalyze() {
  if (state.analyzingId) {
    invoke("cancel_analyze", { id: state.analyzingId }).catch(() => { });
    state.analyzingId = null;
  }
}

function resetToEmpty() {
  cancelInFlightAnalyze();
  lastRequestedUrl = "";
  state.preview = null;
  state.screen = "empty";
  state.url = "";
  state.analysis = null;
  state.selectedQualityId = "";
  state.tab = "video";
  state.mixResolving = null;
  render();
}

// Same as resetToEmpty() but keeps the URL in the input — used after a
// failed analyze/download so the user can just hit ANALYZE again instead of
// retyping the link they already pasted.
function backToEmptyKeepingUrl() {
  cancelInFlightAnalyze();
  lastRequestedUrl = "";
  state.preview = null;
  state.screen = "empty";
  state.analysis = null;
  state.selectedQualityId = "";
  state.tab = "video";
  state.mixResolving = null;
  render();
}

// ── toasts — float above whatever screen is current; opening/closing only
// ever touches #toastWrap, never #body, so they never disrupt the user's
// next action underneath. Slide up from the bottom edge.
// "done"/"error" toasts auto-dismiss after 5s (paused while the pointer is
// over them). "update" toasts don't — they're only ever closed by the user
// or by the update actually being applied.
const TOAST_DURATION = 5000;
const TOAST_TRANSITION = 220; // must match .toast-wrap .toast's transition duration in styles.css
let toastAutoTimer: number | undefined;
let toastCleanupTimer: number | undefined;
// Tracked so the shared #toastWrap knows which behavior applies to what's
// on screen right now (mouse-leave re-arming, and what to do once
// dismissToast runs).
let activeToastKind: "done" | "update" | "error" | null = null;
// Set only by the user clicking the update toast's own ✕. Session-scoped —
// resets on relaunch, so the next startup check surfaces it again either way.
let updateToastDismissedManually = false;

function armToastTimer() {
  window.clearTimeout(toastAutoTimer);
  toastAutoTimer = window.setTimeout(dismissToast, TOAST_DURATION);
}

function showDoneToast(item: HistoryItem) {
  const wrap = document.getElementById("toastWrap");
  if (!wrap) return;
  window.clearTimeout(toastCleanupTimer);
  activeToastKind = "done";
  wrap.classList.remove("hidden");
  wrap.innerHTML = `
    <div class="banner toast">
      <span class="ok">${svg(IC_CHECK, 18, 2.5)}</span>
      <div class="b-txt">
        <span class="b-ttl">DONE — SAVED TO DISK</span>
        <span class="b-sub" title="${esc(item.path ?? "")}">${esc(item.path ?? item.title)}${item.sizeLabel ? ` · ${item.sizeLabel}` : ""}</span>
      </div>
      ${item.path ? `<button class="btn btn-primary open-hist" data-path="${esc(item.path)}" style="flex:none">OPEN FOLDER</button>` : ""}
      <span class="icon-x" id="toastClose">${svg(IC_X, 14)}</span>
    </div>`;
  wrap.querySelector<HTMLElement>(".open-hist")?.addEventListener("click", (e) =>
    invoke("reveal_in_folder", { path: (e.currentTarget as HTMLElement).dataset.path })
  );
  document.getElementById("toastClose")?.addEventListener("click", dismissToast);
  requestAnimationFrame(() => wrap.classList.add("open"));
  armToastTimer();
}

function showErrorToast(message: string) {
  const wrap = document.getElementById("toastWrap");
  if (!wrap) return;
  window.clearTimeout(toastCleanupTimer);
  activeToastKind = "error";
  wrap.classList.remove("hidden");
  const homebrewHint = isHomebrewMissingError(message);
  const formatHint = isFormatUnavailableError(message);
  const cookieHint = isCookieError(message);
  wrap.innerHTML = `
    <div class="banner toast error">
      <span class="ok">${svg(IC_ALERT, 18, 2)}</span>
      <div class="b-txt">
        <span class="b-ttl">COULDN'T DO THAT</span>
        <span class="b-sub" style="white-space:normal">${esc(message)}</span>
      </div>
      ${formatHint && state.installers?.available ? `<button class="btn btn-primary" id="toastUpdateYtdlp" style="flex:none">UPDATE YT-DLP</button>` : ""}
      <button class="btn btn-secondary" id="toastLearnMore" style="flex:none">${homebrewHint ? "LEARN HOW" : "LEARN MORE"}</button>
      <span class="icon-x" id="toastClose">${svg(IC_X, 14)}</span>
    </div>`;
  document.getElementById("toastClose")?.addEventListener("click", dismissToast);
  document.getElementById("toastUpdateYtdlp")?.addEventListener("click", () => {
    dismissToast();
    startToolUpdate("yt-dlp");
  });
  document.getElementById("toastLearnMore")?.addEventListener("click", () => {
    dismissToast();
    openHelp(homebrewHint ? "homebrew" : "download", cookieHint);
  });
  requestAnimationFrame(() => wrap.classList.add("open"));
  armToastTimer();
}

function dismissToast() {
  window.clearTimeout(toastAutoTimer);
  const wrap = document.getElementById("toastWrap");
  if (!wrap) return;
  const wasUpdate = activeToastKind === "update";
  wrap.classList.remove("open");
  toastCleanupTimer = window.setTimeout(() => {
    wrap.classList.add("hidden");
    wrap.innerHTML = "";
    activeToastKind = null;
    if (wasUpdate) {
      // Only reachable via the toast's own ✕ (it never auto-dismisses).
      updateToastDismissedManually = true;
    } else {
      // A "done" toast just finished its course — if an update notice was
      // waiting behind it, bring it back instead of losing it silently.
      maybeResurfaceUpdateToast();
    }
  }, TOAST_TRANSITION);
}

// ── update checking ─────────────────────────────────────────────────────
// yt-dlp ships new releases every 1-2 weeks to keep up with YouTube's
// changes, so a stale copy silently breaks — this pings once at startup
// (and on demand from Settings) and surfaces a toast that stays up until
// the user closes it or applies the update, never a blocking screen.
function pendingUpdateItems(): { tool: string; label: string; from?: string; to?: string }[] {
  const result = state.updateCheck;
  if (!result) return [];
  const items: { tool: string; label: string; from?: string; to?: string }[] = [];
  if (result.ytDlp.updateAvailable) {
    items.push({ tool: "yt-dlp", label: "yt-dlp", from: result.ytDlp.current, to: result.ytDlp.latest });
  }
  if (result.spotdl?.updateAvailable) {
    items.push({ tool: "spotdl", label: "spotDL", from: result.spotdl.current, to: result.spotdl.latest });
  }
  return items;
}

function maybeResurfaceUpdateToast() {
  if (updateToastDismissedManually) return;
  const items = pendingUpdateItems();
  if (items.length) showUpdateToast(items);
}

async function checkForUpdates(opts: { toast: boolean } = { toast: true }) {
  try {
    state.updateCheck = await invoke<UpdateCheck>("check_for_updates");
    if (!opts.toast) return;
    const items = pendingUpdateItems();
    // Don't steal the slot from an in-progress download-completion toast —
    // it'll resurface on its own once that one clears.
    if (items.length && activeToastKind !== "done") showUpdateToast(items);
  } catch { }
}

// Local-only (no network) version lookup — resolves near-instantly, unlike
// checkForUpdates() which also has to reach GitHub/gyan.dev for the latest
// version and can take a while (or hang) on a slow/blocked connection.
// Called whenever Settings opens so "current version" never sits blank
// behind that network round-trip.
async function loadInstalledVersions() {
  try {
    const local = await invoke<UpdateCheck>("installed_versions");
    state.updateCheck = state.updateCheck
      ? {
        ytDlp: { ...state.updateCheck.ytDlp, current: local.ytDlp.current, source: local.ytDlp.source },
        ffmpeg: { ...state.updateCheck.ffmpeg, current: local.ffmpeg.current, source: local.ffmpeg.source },
        spotdl: { ...state.updateCheck.spotdl, current: local.spotdl.current, source: local.spotdl.source },
      }
      : local;
    if (state.settingsOpen) renderSettingsModal();
  } catch { }
}

function showUpdateToast(items: { tool: string; label: string; from?: string; to?: string }[]) {
  const wrap = document.getElementById("toastWrap");
  if (!wrap || !items.length) return;
  window.clearTimeout(toastCleanupTimer);
  window.clearTimeout(toastAutoTimer); // stays up until dismissed or updated — no auto-hide
  activeToastKind = "update";
  wrap.classList.remove("hidden");
  const rows = items
    .map(
      (it) => `<div class="update-row" data-row-tool="${esc(it.tool)}" style="display:contents">
        <div class="b-txt">
          <span class="b-ttl">${esc(it.label.toUpperCase())} UPDATE AVAILABLE</span>
          <span class="b-sub">${esc(it.from || "?")} → ${esc(it.to || "?")}</span>
        </div>
        <button class="btn btn-primary update-tool-btn" data-tool="${esc(it.tool)}" style="flex:none">UPDATE</button>
      </div>`
    )
    .join("");
  wrap.innerHTML = `
    <div class="banner toast" id="updateToast">
      <span class="ok">${svg(IC_ARROW_UP, 18, 2.5)}</span>
      ${rows}
      <span class="icon-x" id="updateToastClose">${svg(IC_X, 14)}</span>
    </div>`;
  wrap.querySelectorAll<HTMLButtonElement>(".update-tool-btn").forEach((btn) =>
    btn.addEventListener("click", () => {
      const tool = btn.dataset.tool!;
      dismissToast();
      startToolUpdate(tool);
    })
  );
  document.getElementById("updateToastClose")?.addEventListener("click", dismissToast);
  requestAnimationFrame(() => wrap.classList.add("open"));
}

let installCancelRequested = false;

function startInstall() {
  const need = missingTools();
  if (!need.length) return;
  installCancelRequested = false;
  state.installTargets = need;
  state.installIsUpdate = false;
  state.installReturnScreen = null;
  state.installLog = [];
  state.installDone = false;
  state.installError = "";
  state.screen = "installing";
  render();
  invoke("install_tools", { tools: need }).catch((err) => {
    if (installCancelRequested) return;
    // fallback if the event didn't fire (e.g. failed to launch)
    if (!state.installError && !state.installDone) {
      state.installError = String(err);
      if (state.screen === "installing") render();
    }
  });
}

function cancelInstall() {
  installCancelRequested = true;
  invoke("cancel_install").catch(() => { });
  if (state.installIsUpdate) {
    finishToolUpdate();
  } else {
    finishInstall();
  }
}

async function finishInstall() {
  state.installTargets = [];
  state.installIsUpdate = false;
  state.installDone = false;
  state.installError = "";
  state.installLog = [];
  resetToEmpty();
  try {
    state.binaries = await invoke<Binaries>("check_binaries");
  } catch { }
  render();
}

// Same screen/log/progress UI as a first-time install, but for updating a
// tool that's already working — triggered from the Settings modal or the
// update toast. Unlike startInstall() this can happen mid-session, so it
// remembers the current screen and returns to it instead of resetting.
function startToolUpdate(tool: string) {
  installCancelRequested = false;
  state.installTargets = [tool];
  state.installIsUpdate = true;
  state.installReturnScreen = state.screen === "installing" ? "empty" : state.screen;
  state.installLog = [];
  state.installDone = false;
  state.installError = "";
  state.screen = "installing";
  render();
  invoke("update_tool", { tool }).catch((err) => {
    if (installCancelRequested) return;
    if (!state.installError && !state.installDone) {
      state.installError = String(err);
      if (state.screen === "installing") render();
    }
  });
}

async function finishToolUpdate() {
  const targetScreen = (state.installReturnScreen && state.installReturnScreen !== "installing")
    ? state.installReturnScreen
    : "empty";
  state.screen = targetScreen;
  state.installReturnScreen = null;
  state.installTargets = [];
  state.installIsUpdate = false;
  state.installDone = false;
  state.installError = "";
  state.installLog = [];
  render();
  if (state.settingsOpen) renderSettingsModal();

  try {
    state.binaries = await invoke<Binaries>("check_binaries");
  } catch { }
  await checkForUpdates({ toast: false });
  if (state.settingsOpen) renderSettingsModal();
}

function restartApp() {
  invoke("restart_app").catch(() => { });
}

function appendInstallLog() {
  const pre = $("#installLog");
  if (!pre) {
    render();
    return;
  }
  pre.textContent = state.installLog.slice(-200).join("\n") || "starting…";
  pre.scrollTop = pre.scrollHeight;
}

async function pickDir() {
  const picked = await openDialog({ directory: true, defaultPath: state.outputDir });
  if (typeof picked === "string") {
    state.outputDir = picked;
    dirPinned = true;
    persistSettings();
    render();
  }
}

async function doAnalyze(opts: { force?: boolean; auto?: boolean } = {}) {
  window.clearTimeout(autoAnalyzeTimer);
  if (!isLikelyUrl(state.url)) return;
  const requested = state.url.trim();
  // The same link is already analyzed / being analyzed — nothing to redo
  // (Enter forces a retry).
  if (
    !opts.force &&
    requested === lastRequestedUrl &&
    (state.screen === "analyzing" ||
      state.screen === "analyzed" ||
      state.screen === "playlist" ||
      state.screen === "mix")
  ) {
    return;
  }
  // Missing the tool this link needs (e.g. a Spotify link with spotDL not yet
  // installed): an explicit Enter kicks off the install, auto-analyze doesn't.
  if (!canAnalyze()) {
    if (!opts.auto) startInstall();
    return;
  }
  lastRequestedUrl = requested;
  // A Mix/Radio link is recognizable from the URL alone (see isLikelyMixUrl) —
  // jump straight to the chooser instead of burning a whole analyze
  // round-trip just to learn what the pasted link already told us. The real
  // analyze only happens once the user picks THIS VIDEO ONLY (on the
  // stripped, non-mix URL) or FIRST N (mixResolving, below).
  if (!state.mixResolving && isLikelyMixUrl(requested)) {
    cancelInFlightAnalyze();
    state.mixMode = "single";
    state.preview = null;
    state.screen = "mix";
    render();
    return;
  }
  cancelInFlightAnalyze(); // editing + resubmitting replaces whatever was running, not queues alongside it
  const id = crypto.randomUUID();
  state.analyzingId = id;
  const urlAtRequest = requested; // the link as submitted — further edits don't retarget this request
  const resolvingCapped = state.mixResolving === "capped";
  state.mixResolving = null;
  state.preview = null;
  state.screen = "analyzing";
  render();

  // Instant title/thumbnail preview while yt-dlp works out the real qualities.
  invoke<QuickMeta | null>("quick_meta", { url: urlAtRequest })
    .then((meta) => {
      if (meta && state.analyzingId === id && state.screen === "analyzing") {
        state.preview = meta;
        render();
      }
    })
    .catch(() => { });

  try {
    const res = await invoke<AnalyzeResult>("analyze", { id, url: urlAtRequest, ...cookieParams() });
    if (state.analyzingId !== id) return; // superseded by a newer request meanwhile — drop it
    state.analyzingId = null;
    state.preview = null;
    state.analysis = res;
    state.selectedQualityId = "";
    // Audio-only sources (Spotify) have no video tiers — start on the audio tab.
    state.tab = res.videoOptions.length ? "video" : "audio";
    if (res.kind === "playlist") {
      state.entrySelected = res.entries.map(() => true);
      state.screen = "playlist";
    } else if (res.kind === "mix") {
      if (resolvingCapped) {
        // This analyze was explicitly asked for (FIRST N OF THE MIX) — go
        // straight to the quality picker with the first N pre-selected
        // instead of re-showing the chooser the user already answered.
        state.entrySelected = res.entries.map((_, i) => i < state.mixN);
        state.screen = "playlist";
      } else {
        state.mixMode = "single";
        state.screen = "mix";
      }
    } else {
      state.screen = "analyzed";
    }
  } catch (err) {
    if (state.analyzingId !== id) return; // superseded (or this is just its own cancellation) — ignore
    state.analyzingId = null;
    backToEmptyKeepingUrl();
    showErrorToast(String(err));
    return;
  }
  render();
}

function patchQualitySelection() {
  const sel = currentSelection();
  if (!sel) return;
  body()
    .querySelectorAll<HTMLElement>(".qcell[data-q]")
    .forEach((cell) => {
      cell.setAttribute("aria-selected", String(cell.dataset.q === sel.id));
    });
  const dlBtn = document.getElementById("downloadBtn");
  if (dlBtn) {
    dlBtn.innerHTML = `${svg(IC_DOWNLOAD, 15)} DOWNLOAD · ${esc(sel.label)}`;
  }
}

function patchPlaylistSelection() {
  const a = state.analysis;
  if (!a) return;
  const total = a.entries.length;
  const selCount = state.entrySelected.filter(Boolean).length;
  body()
    .querySelectorAll<HTMLElement>(".queue-row[data-entry]")
    .forEach((row) => {
      const i = Number(row.dataset.entry);
      const sel = !!state.entrySelected[i];
      row.style.background = sel ? "var(--color-accent-100)" : "";
      const box = row.firstElementChild as HTMLElement | null;
      if (box) {
        box.style.background = sel ? "var(--color-accent)" : "";
        box.style.color = sel ? "var(--color-bg)" : "";
        box.style.border = sel ? "" : "1px solid var(--color-neutral-400)";
        box.textContent = sel ? "✓" : "";
      }
      const nm = row.querySelector<HTMLElement>(".nm");
      if (nm) {
        nm.style.color = sel ? "" : "var(--color-neutral-700)";
      }
    });
  const selectAll = document.getElementById("selectAll");
  if (selectAll) {
    selectAll.textContent = selCount === total ? "Clear all" : "Select all";
  }
  const footCount = body().querySelector<HTMLElement>(".playlist-foot span");
  if (footCount) {
    footCount.textContent = `${selCount}/${total} selected`;
  }
  const dlBtn = document.getElementById("downloadPlaylist") as HTMLButtonElement | null;
  if (dlBtn) {
    dlBtn.disabled = selCount === 0;
    dlBtn.innerHTML = `${svg(IC_DOWNLOAD, 15)} DOWNLOAD ${selCount} SELECTED`;
  }
}

async function startDownload() {
  cancelInFlightAnalyze();
  const a = state.analysis!;
  const sel = currentSelection();
  if (!sel) return;
  const id = crypto.randomUUID();
  state.job = {
    id,
    percent: 0,
    speed: "",
    eta: "",
    stage: "starting",
    name: a.title + (state.tab === "audio" ? `.${audioExtLabel(sel.id)}` : ".mp4"),
    sizeLabel: "",
  };
  state.screen = "downloading";
  render();
  runDownload({
    id,
    url: a.webpageUrl,
    formatSelector: sel.formatSelector,
    qualityId: sel.id,
    kind: state.tab,
    audioFormat: state.tab === "audio" ? sel.id : null,
    outputDir: state.outputDir,
    writeThumbnail: state.writeThumbnail,
    writeDescription: state.writeDescription,
    writeSubs: state.writeSubs,
    playlistItems: null,
  });
}

function startPlaylistDownload() {
  const a = state.analysis!;
  const isSpotify = a.extractor === "Spotify";
  // Spotify has no index-range selection — pass the exact track URLs the user
  // ticked (newline-joined); yt-dlp takes a comma-separated index list.
  const items = isSpotify
    ? a.entries
      .filter((_, i) => state.entrySelected[i])
      .map((e) => e.url)
      .filter(Boolean)
      .join("\n")
    : a.entries
      .filter((_, i) => state.entrySelected[i])
      .map((e) => e.index)
      .join(",");
  if (!items) return;
  const sel = currentSelection();
  if (!sel) return;
  const id = crypto.randomUUID();
  const selCount = state.entrySelected.filter(Boolean).length;
  state.job = {
    id,
    percent: 0,
    speed: "",
    eta: "",
    stage: "starting",
    name: `${a.title} — ${selCount} videos`,
    sizeLabel: "",
  };
  state.screen = "downloading";
  render();
  runDownload({
    id,
    url: a.webpageUrl,
    formatSelector: sel.formatSelector,
    qualityId: sel.id,
    kind: state.tab,
    audioFormat: state.tab === "audio" ? sel.id : null,
    outputDir: state.outputDir,
    writeThumbnail: state.writeThumbnail,
    writeDescription: state.writeDescription,
    writeSubs: state.writeSubs,
    playlistItems: items,
  });
}

function mixContinue() {
  // Turn the mix into a normal single-video analyze/download path.
  if (state.mixMode === "single") {
    // Strip the radio/list/index params off the pasted link and run the
    // (only) analyze so we get the real clip's title, thumbnail and
    // per-format sizes — no separate mix analyze ever happened.
    try {
      const u = new URL(state.url);
      u.searchParams.delete("list");
      u.searchParams.delete("index");
      u.searchParams.delete("start_radio");
      state.url = u.toString();
    } catch {
      state.url = state.url
        .replace(/[?&]list=[^&]+/g, "")
        .replace(/[?&]index=[^&]+/g, "")
        .replace(/[?&]start_radio=[^&]+/g, "");
    }
    doAnalyze({ force: true });
  } else {
    // capped: this is the first time the mix actually needs to be resolved
    // (to get real entries to pre-select from) — analyze the original,
    // unstripped URL and land on the quality picker once it resolves.
    state.mixResolving = "capped";
    state.selectedQualityId = "";
    doAnalyze({ force: true });
  }
}

function runDownload(req: Record<string, unknown>) {
  invoke("download", { req: { ...req, ...cookieParams() } }).catch((err) => {
    // the dl-error event already handles UI; keep this as a fallback
    if (state.screen === "downloading") {
      backToEmptyKeepingUrl();
      showErrorToast(String(err));
    }
  });
}

async function cancelDownload() {
  if (state.job) await invoke("cancel_download", { id: state.job.id });
  resetToEmpty();
}

// ── live progress events ─────────────────────────────────────────────────
async function wireBackendEvents() {
  await listen<{ id: string; percent: number; speed: string; eta: string; stage: string }>(
    "dl-progress",
    (e) => {
      const p = e.payload;
      if (!state.job || state.job.id !== p.id) return;
      state.job.percent = p.percent;
      state.job.speed = p.speed;
      state.job.eta = p.eta;
      state.job.stage = p.stage;
      patchProgress();
    }
  );

  await listen<{ id: string; filepath?: string; filesize?: number }>("dl-done", (e) => {
    if (!state.job || state.job.id !== e.payload.id) return;
    const item: HistoryItem = {
      title: (state.analysis?.title ?? state.job.name).replace(/\.(mp4|mp3|m4a|webm|opus|flac|ogg|wav)$/i, ""),
      ext: e.payload.filepath?.split(".").pop() ?? (state.tab === "audio" ? audioExtLabel(state.selectedQualityId) : "mp4"),
      // Prefer the real on-disk size the backend reports; fall back to the
      // pre-download estimate only if it's somehow missing.
      sizeLabel: e.payload.filesize ? fmtBytes(e.payload.filesize) : state.job.sizeLabel,
      path: e.payload.filepath,
      at: Date.now(),
    };
    state.history.unshift(item);
    persistHistory();
    state.job = null;
    resetToEmpty();
    showDoneToast(item);
  });

  await listen<{ id: string; message: string }>("dl-error", (e) => {
    if (!state.job || state.job.id !== e.payload.id) return;
    state.job = null;
    backToEmptyKeepingUrl();
    showErrorToast(e.payload.message);
  });

  // installer events
  await listen<string>("install-log", (e) => {
    if (installCancelRequested) return;
    state.installLog.push(e.payload);
    if (state.screen === "installing") appendInstallLog();
  });
  await listen("install-done", async () => {
    if (installCancelRequested) return;
    state.installDone = true;
    // Antivirus real-time scanning can briefly lock a just-downloaded .exe,
    // so the very first check right after "done" can spuriously say it's
    // still missing. Retry a few times before accepting that result.
    for (let attempt = 0; attempt < 6; attempt++) {
      try {
        state.binaries = await invoke<Binaries>("check_binaries");
      } catch { }
      if (missingTools().length === 0) break;
      await new Promise((r) => setTimeout(r, 400));
    }
    if (state.screen === "installing") render();
  });
  await listen<string>("install-error", (e) => {
    if (installCancelRequested) return;
    state.installError = e.payload || "Install failed.";
    if (state.screen === "installing") render();
  });
}

// patch just the progress bits without a full re-render (smooth updates)
function patchProgress() {
  const j = state.job;
  if (!j || state.screen !== "downloading") return;
  const bar = $("#bar");
  const fill = $("#barFill");
  const pct = $("#jobPct");
  const eta = $("#jobEta");
  const stageEl = body().querySelector<HTMLElement>(".job-stage");
  const indeterminate = j.percent <= 0 || j.stage !== "downloading";
  if (bar) bar.classList.toggle("indeterminate", indeterminate);
  if (fill) fill.style.width = `${Math.max(0, Math.min(100, j.percent))}%`;
  const stageText = formatStageText(j.stage);
  if (pct) pct.textContent = indeterminate ? stageText : `${j.percent.toFixed(0)}%`;
  if (eta) eta.textContent = `${j.speed}${j.eta ? ` · ${j.eta} left` : ""}`;
  if (stageEl) stageEl.textContent = stageText;
}

// ── theme toggle ─────────────────────────────────────────────────────────
function wireChrome() {
  $("#settingsToggle")?.addEventListener("click", openSettings);
  $("#themeToggle")?.addEventListener("click", () => {
    const app = $("#app")!;
    const dark = app.getAttribute("data-theme") === "dark";
    app.setAttribute("data-theme", dark ? "light" : "dark");
    localStorage.setItem("fetch.theme", dark ? "light" : "dark");
  });
  const saved = localStorage.getItem("fetch.theme");
  if (saved) $("#app")!.setAttribute("data-theme", saved);

  const win = getCurrentWindow();
  $("#winMinimize")?.addEventListener("click", () => win.minimize());
  $("#winClose")?.addEventListener("click", () => win.close());

  // pause the toast's auto-dismiss while the pointer is over it — only
  // applies to "done"/"error" toasts; "update" toasts have no timer to re-arm
  const toastWrap = $("#toastWrap")!;
  toastWrap.addEventListener("mouseenter", () => window.clearTimeout(toastAutoTimer));
  toastWrap.addEventListener("mouseleave", () => {
    if (toastWrap.classList.contains("open") && activeToastKind !== "update") armToastTimer();
  });

  // Global paste handler: pasting a URL anywhere in the app automatically loads and analyzes it
  window.addEventListener("paste", (e) => {
    const active = document.activeElement as HTMLElement | null;
    if (active && (active.tagName === "INPUT" || active.tagName === "TEXTAREA")) return;
    if (state.settingsOpen || state.helpOpen || state.cookieImportModal.open) return;
    if (state.screen === "downloading" || state.screen === "installing") return;

    const text = e.clipboardData?.getData("text")?.trim();
    if (text && isLikelyUrl(text)) {
      e.preventDefault();
      state.url = text;
      doAnalyze({ auto: true });
    }
  });

  window.addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    if (state.helpOpen) {
      closeHelp();
    } else if (state.cookieImportModal.open) {
      if (state.cookieImport.state !== "busy") closeCookieImportModal();
    } else if (state.settingsOpen) {
      closeSettings();
    }
  });
}

// ── boot ─────────────────────────────────────────────────────────────────
window.addEventListener("DOMContentLoaded", async () => {
  loadHistory();
  loadSettings();
  wireChrome();
  if (!state.hasSavedOutputDir) state.outputDir = "~/Downloads/Fetch";
  render(); // paint immediately; backend calls below only enrich the view

  try {
    await wireBackendEvents();
  } catch (e) {
    console.warn("Tauri events unavailable (running outside the app shell):", e);
  }
  if (!state.hasSavedOutputDir) {
    try {
      state.outputDir = await invoke<string>("default_download_dir");
    } catch { }
  }
  try {
    state.binaries = await invoke<Binaries>("check_binaries");
  } catch {
    state.binaries = { ytDlp: false, ffmpeg: false, spotdl: false };
  }
  try {
    state.installers = await invoke<Installers>("detect_installers");
  } catch {
    state.installers = { available: false, manager: "", platform: "", brewAvailable: false };
  }
  render();
  if (state.binaries?.ytDlp && state.binaries?.ffmpeg) {
    checkForUpdates(); // fire-and-forget; toasts only if something's outdated
  }
  // Check for FETCH app updates (auto-update via Tauri updater plugin)
  checkForAppUpdate();

  // Dev-only hook: lets the UI states be driven for visual review without a
  // live yt-dlp backend. Stripped from production builds.
  if ((import.meta as any).env?.DEV) {
    (window as any).__fetch = { state, render };
  }
});
