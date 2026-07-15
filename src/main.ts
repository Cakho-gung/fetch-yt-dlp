import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";

// ── types (mirror the Rust structs) ───────────────────────────────────────
interface Binaries {
  ytDlp: boolean;
  ffmpeg: boolean;
  ytDlpPath?: string;
  ffmpegPath?: string;
}
interface Installers {
  available: boolean;
  manager: string; // "mac" | "fetch" | ""
  platform: string;
}
interface ToolUpdate {
  current?: string;
  latest?: string;
  updateAvailable: boolean;
  source: string; // "portable" | "brew" | "other" | ""
}
interface UpdateCheck {
  ytDlp: ToolUpdate;
  ffmpeg: ToolUpdate;
}
interface QualityOption {
  id: string;
  label: string;
  height?: number;
  approxBytes?: number;
  formatSelector: string;
  kind: "video" | "audio";
}
interface PlaylistEntry {
  index: number;
  title: string;
  durationSeconds?: number;
  thumbnail?: string;
}
interface AnalyzeResult {
  kind: "video" | "playlist" | "mix";
  webpageUrl: string;
  title: string;
  uploader?: string;
  durationSeconds?: number;
  viewCount?: number;
  thumbnail?: string;
  extractor: string;
  videoOptions: QualityOption[];
  audioOptions: QualityOption[];
  entries: PlaylistEntry[];
  playlistCount?: number;
}
interface HistoryItem {
  title: string;
  ext: string;
  sizeLabel: string;
  path?: string;
  at: number;
}

type Screen =
  | "empty"
  | "analyzing"
  | "analyzed"
  | "playlist"
  | "mix"
  | "downloading"
  | "installing";

// ── app state ──────────────────────────────────────────────────────────────
const state = {
  screen: "empty" as Screen,
  binaries: null as Binaries | null,
  installers: null as Installers | null,
  installLog: [] as string[],
  installDone: false,
  installError: "",
  // What the "installing" screen is currently showing: the label(s) to
  // display, whether it's a version update (vs. first-time install of a
  // missing tool — different title/copy, and doesn't force a restart), and
  // which screen to return to once it's done (updates happen mid-session,
  // so unlike a first-time install we can't just dump the user back to
  // "empty" — they may have a link/analysis in progress).
  installTargets: [] as string[],
  installIsUpdate: false,
  installReturnScreen: null as Screen | null,
  updateCheck: null as UpdateCheck | null,
  checkingUpdates: false,
  outputDir: "",
  url: "",
  analyzingId: null as string | null,
  analysis: null as AnalyzeResult | null,
  tab: "video" as "video" | "audio",
  selectedQualityId: "",
  writeThumbnail: false,
  writeDescription: false,
  writeSubs: false,
  job: null as null | {
    id: string;
    percent: number;
    speed: string;
    eta: string;
    stage: string;
    name: string;
    sizeLabel: string;
  },
  history: [] as HistoryItem[],
  // playlist selection
  entrySelected: [] as boolean[],
  mixMode: "single" as "single" | "capped",
  mixN: 10,
  // settings
  settingsOpen: false,
  cookieMode: "none" as "none" | "browser" | "file",
  cookieBrowser: "chrome",
  cookieFile: "",
};

const COOKIE_BROWSERS = ["chrome", "edge", "firefox", "brave", "opera", "vivaldi", "safari"];

// ── helpers ──────────────────────────────────────────────────────────────
const $ = <T extends HTMLElement = HTMLElement>(sel: string) =>
  document.querySelector<T>(sel);
const body = () => $("#body")!;
const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!)
  );

function fmtBytes(b?: number): string {
  if (!b || b <= 0) return "— MB";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let n = b;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n < 10 && i > 1 ? n.toFixed(1) : Math.round(n)} ${u[i]}`;
}
function fmtDuration(s?: number): string {
  if (s == null) return "";
  const t = Math.round(s);
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const sec = t % 60;
  const pad = (x: number) => String(x).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`;
}
function fmtViews(v?: number): string {
  if (v == null) return "";
  if (v >= 1e6) return `${(v / 1e6).toFixed(1)}M views`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(1)}K views`;
  return `${v} views`;
}
function detectSource(url: string): string {
  try {
    const h = new URL(url).hostname.replace(/^www\./, "");
    if (h.includes("youtu")) return "YOUTUBE";
    if (h.includes("spotify")) return "SPOTIFY";
    if (h.includes("soundcloud")) return "SOUNDCLOUD";
    if (h.includes("tiktok")) return "TIKTOK";
    if (h.includes("vimeo")) return "VIMEO";
    if (h.includes("twitter") || h.includes("x.com")) return "X";
    return h.split(".")[0].toUpperCase();
  } catch {
    return "LINK";
  }
}
const isLikelyUrl = (s: string) => /^https?:\/\/\S+\.\S+/.test(s.trim());

// registration-mark corners for a .blueprint element
const corners = `<i class="corner tl"></i><i class="corner tr"></i><i class="corner bl"></i><i class="corner br"></i>`;

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
  } else if (state.binaries && !state.binaries.ytDlp) {
    tag.innerHTML = `<span class="tag tag-neutral">yt-dlp MISSING</span>`;
  } else {
    tag.innerHTML = "";
  }
}

function missingTools(): string[] {
  const b = state.binaries;
  if (!b) return [];
  const need: string[] = [];
  if (!b.ytDlp) need.push("yt-dlp");
  if (!b.ffmpeg) need.push("ffmpeg");
  return need;
}

function missingBinariesWarn(): string {
  const need = missingTools();
  if (need.length === 0) return "";
  const canAuto = !!state.installers?.available;
  const mgr = state.installers?.manager || "brew";
  const needsYtDlp = need.includes("yt-dlp");
  const needsFfmpeg = need.includes("ffmpeg");

  let label = "⇣ INSTALL WITH BREW";
  let desc = `FETCH will run <code>brew</code> for you — ffmpeg can take a few minutes.`;
  if (mgr === "fetch") {
    label = "⇣ DOWNLOAD YT-DLP + FFMPEG";
    desc = `FETCH downloads portable copies into its own folder — no system install, no PATH changes. ffmpeg can take a few minutes.`;
  } else if (mgr === "mac") {
    // yt-dlp downloads straight into FETCH's own folder; ffmpeg still goes
    // through brew (no reliable portable static build for macOS).
    if (needsYtDlp && needsFfmpeg) {
      label = "⇣ DOWNLOAD YT-DLP + INSTALL FFMPEG";
      desc = `yt-dlp downloads directly, no install needed; ffmpeg installs via <code>brew</code> — can take a few minutes.`;
    } else if (needsYtDlp) {
      label = "⇣ DOWNLOAD YT-DLP";
      desc = `FETCH downloads a portable copy into its own folder — no system install, no PATH changes.`;
    } else {
      label = "⇣ INSTALL WITH BREW";
      desc = `FETCH will run <code>brew</code> for you to install ffmpeg — can take a few minutes.`;
    }
  }

  const actions = canAuto
    ? `<div style="display:flex;align-items:center;gap:var(--space-2);margin-top:var(--space-2)">
         <button class="btn btn-primary" id="installBtn" style="font-size:12.5px">${label}</button>
         <span class="text-muted" style="font-size:11.5px">${desc}</span>
       </div>`
    : `<br>macOS: <code>brew install ${need.join(" ")}</code>, then reopen FETCH. On other platforms, install ${need.join(
        " and "
      )} manually and make sure they're on PATH.`;
  return `<div class="warn">
    <span>⚠</span>
    <div>Missing <b>${need.join(" + ")}</b> — analysis and downloads won't run until installed.${actions}</div>
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
        <span class="job-name">${
          errored ? failLabel : done ? doneLabel : `${verb} ${targets.join(" + ").toUpperCase()}…`
        }</span>
        <span class="job-stage text-muted">${
          errored
            ? esc(state.installError)
            : needsRestart
            ? "FETCH needs to restart to pick this up"
            : done
            ? (state.installIsUpdate ? "up to date" : "tools are ready")
            : state.installers?.manager !== "fetch" && targets.includes("ffmpeg")
            ? "running brew — please wait"
            : "downloading — please wait"
        }</span>
      </div>
      ${
        needsRestart
          ? `<button class="btn btn-primary" id="installRestart">⟳ RESTART APP</button>`
          : done || errored
          ? `<button class="btn ${done ? "btn-primary" : "btn-secondary"}" id="installBack">${done ? "CONTINUE" : "BACK"}</button>`
          : ""
      }
    </div>
    ${
      done || errored
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
  const thumb = `<label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbThumb" ${state.writeThumbnail ? "checked" : ""}><span class="dot"></span>Thumbnail file</label>`;
  const desc = `<label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbDesc" ${state.writeDescription ? "checked" : ""}><span class="dot"></span>Description file</label>`;
  const subs = `<label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbSrt" ${state.writeSubs ? "checked" : ""}><span class="dot"></span>Subtitle file</label>`;
  return `<div class="opts-row">${thumb}${desc}${state.tab === "video" ? subs : ""}</div>`;
}

function dirRow(): string {
  return `<div class="dir-row">
    <span class="text-muted mono">↧</span>
    <span class="path" title="${esc(state.outputDir)}">${esc(state.outputDir)}</span>
    <button class="btn btn-ghost" id="pickDir">CHANGE…</button>
  </div>`;
}

// ── settings modal ──────────────────────────────────────────────────────
function renderSettingsModal() {
  const overlay = document.getElementById("settingsOverlay");
  if (!overlay || !state.settingsOpen) return;
  overlay.innerHTML = `
    <div class="modal">
      <div class="modal-head">
        <span class="section-label">SETTINGS</span>
        <span class="icon-x" id="settingsClose">✕</span>
      </div>
      <div class="modal-body">
        <div class="settings-group">
          <span class="section-label">DOWNLOAD LOCATION</span>
          <div class="dir-row">
            <span class="text-muted mono">↧</span>
            <span class="path" title="${esc(state.outputDir)}">${esc(state.outputDir)}</span>
            <button class="btn btn-ghost" id="settingsPickDir">CHANGE…</button>
          </div>
        </div>
        <div class="settings-group">
          <span class="section-label">COOKIES</span>
          <p class="text-muted" style="font-size:12px;margin:var(--space-1) 0 var(--space-2)">Needed for age-restricted, members-only or private videos.</p>
          <div class="opts-row">
            <label class="radio" style="font-size:13px"><input type="radio" name="cookieMode" value="none" ${state.cookieMode === "none" ? "checked" : ""}><span class="dot"></span>None</label>
            <label class="radio" style="font-size:13px"><input type="radio" name="cookieMode" value="browser" ${state.cookieMode === "browser" ? "checked" : ""}><span class="dot"></span>Use cookies from browser</label>
            ${
              state.cookieMode === "browser"
                ? `<select class="input" id="cookieBrowserSelect" style="margin-left:24px;width:auto;min-width:160px">
                    ${COOKIE_BROWSERS.map(
                      (b) => `<option value="${b}" ${state.cookieBrowser === b ? "selected" : ""}>${b[0].toUpperCase()}${b.slice(1)}</option>`
                    ).join("")}
                  </select>`
                : ""
            }
            <label class="radio" style="font-size:13px"><input type="radio" name="cookieMode" value="file" ${state.cookieMode === "file" ? "checked" : ""}><span class="dot"></span>Use a cookies.txt file</label>
            ${
              state.cookieMode === "file"
                ? `<div class="dir-row" style="margin-left:24px">
                    <span class="path" title="${esc(state.cookieFile)}">${state.cookieFile ? esc(state.cookieFile) : "no file selected"}</span>
                    <button class="btn btn-ghost" id="pickCookieFile">CHOOSE…</button>
                  </div>`
                : ""
            }
          </div>
        </div>
        <div class="settings-group">
          <span class="section-label">TOOLS</span>
          ${toolUpdateRow("yt-dlp", state.updateCheck?.ytDlp)}
          ${toolUpdateRow("ffmpeg", state.updateCheck?.ffmpeg)}
          <div class="dir-row" style="margin-top:var(--space-2)">
            <button class="btn btn-ghost" id="checkUpdatesBtn" ${state.checkingUpdates ? "disabled" : ""}>
              ${state.checkingUpdates ? "CHECKING…" : "⟳ CHECK FOR UPDATES"}
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

function toolUpdateRow(tool: "yt-dlp" | "ffmpeg", info: ToolUpdate | undefined): string {
  const label = tool === "yt-dlp" ? "yt-dlp" : "ffmpeg";
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
  const canUpdate = !!info?.updateAvailable && (info.source === "portable" || info.source === "brew");
  return `<div class="dir-row" style="justify-content:space-between">
    <span style="display:flex;align-items:center;gap:var(--space-2)">
      <span class="mono" style="font-size:12.5px">${label} ${current}</span>
      ${status}
    </span>
    ${
      canUpdate
        ? `<button class="btn btn-ghost settings-update-btn" data-tool="${esc(tool)}" style="font-size:11.5px">UPDATE</button>`
        : ""
    }
  </div>`;
}

function wireSettingsEvents() {
  document.getElementById("settingsClose")?.addEventListener("click", closeSettings);
  document.getElementById("settingsDone")?.addEventListener("click", closeSettings);
  document.getElementById("settingsOverlay")?.addEventListener("click", (e) => {
    if (e.target === document.getElementById("settingsOverlay")) closeSettings();
  });
  document.getElementById("settingsPickDir")?.addEventListener("click", pickDir);
  document.getElementById("pickCookieFile")?.addEventListener("click", pickCookieFile);
  document.getElementById("cookieBrowserSelect")?.addEventListener("change", (e) => {
    state.cookieBrowser = (e.target as HTMLSelectElement).value;
    persistSettings();
  });
  document
    .querySelectorAll<HTMLInputElement>('input[name="cookieMode"]')
    .forEach((r) =>
      r.addEventListener("change", () => {
        state.cookieMode = r.value as "none" | "browser" | "file";
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
  const overlay = document.getElementById("settingsOverlay");
  if (!overlay) return;
  overlay.classList.remove("open");
  settingsHideTimer = window.setTimeout(() => {
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
    persistSettings();
    renderSettingsModal();
  }
}

function cookieParams() {
  return {
    cookieMode: state.cookieMode,
    cookieBrowser: state.cookieMode === "browser" ? state.cookieBrowser : null,
    cookieFile: state.cookieMode === "file" ? state.cookieFile : null,
  };
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
        <td class="text-muted mono" style="white-space:nowrap;font-size:11px">${esc(h.sizeLabel)} · ${relTime(h.at)}</td>
        <td style="width:70px;text-align:right">${
          h.path ? `<button class="btn btn-ghost open-hist" data-path="${esc(h.path)}" style="font-size:12px">OPEN</button>` : ""
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

function relTime(at: number): string {
  const d = (Date.now() - at) / 1000;
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
  return `${Math.floor(d / 86400)} d ago`;
}

// input row (live) — used in empty & success
// Always the live, editable URL box — never swapped for a read-only chip or
// disabled once something's been analyzed. Submitting again (Enter or the
// button) just cancels whatever analyze is in flight and starts a new one;
// see doAnalyze(). Only the trailing status tag changes per screen.
function linkInputRow(placeholder: string): string {
  const canAnalyze = !!(state.binaries?.ytDlp && state.binaries?.ffmpeg);
  const a = state.analysis;
  let statusTag = "";
  if (a && state.screen === "playlist") {
    const count = a.playlistCount ?? a.entries.length;
    statusTag = `<span class="tag tag-accent">PLAYLIST · ${count} VIDEOS</span>`;
  } else if (a && state.screen === "mix") {
    statusTag = `<span class="tag tag-neutral" style="font-weight:700">MIX · ENDLESS</span>`;
  } else if (a && state.screen === "analyzed") {
    const src = detectSource(a.webpageUrl);
    statusTag = `<span class="tag tag-accent">${esc(a.extractor.toUpperCase() || src)}</span>`;
  }
  return `<div class="link-row">
    <div class="link-input-wrap">
      <input class="input" id="urlInput" placeholder="${esc(placeholder)}" value="${esc(state.url)}" spellcheck="false" autocomplete="off" ${canAnalyze ? "" : "disabled"}>
      ${state.url ? `<span class="icon-x link-clear" id="clearLink" title="Clear">✕</span>` : ""}
    </div>
    ${statusTag}
    <button class="btn btn-primary" id="analyzeBtn" ${isLikelyUrl(state.url) && canAnalyze ? "" : "disabled"}>ANALYZE</button>
  </div>`;
}

// ── screens ──────────────────────────────────────────────────────────────
function viewEmpty(): string {
  const analyzing = state.screen === "analyzing";
  const src = state.url ? detectSource(state.url) : "";
  return `
  ${missingBinariesWarn()}
  ${linkInputRow("Paste a video, music or playlist link…")}
  <div class="grid-analyze">
    <div class="blueprint dropzone">${corners}
      <div class="inner">
        <span style="font-size:24px">${analyzing ? "◐" : "⇣"}</span>
        <span class="mono" style="font-size:11px">${
          analyzing ? "analyzing link…" : "no link yet — paste one to start"
        }</span>
        ${state.url && !analyzing ? `<span class="tag tag-accent" style="margin-top:6px">${src} detected</span>` : ""}
      </div>
    </div>
    <div class="card locked">
      <div class="seg" style="align-self:stretch">
        <label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt" checked>Video</label>
        <label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt">Audio</label>
      </div>
      <div class="qgrid">
        ${["BEST", "1080P", "720P", "480P"]
          .map(
            (q) =>
              `<div class="qcell"><span class="q">${q}</span><span class="sz">— MB</span></div>`
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
  const opts = state.tab === "video" ? a.videoOptions : a.audioOptions;
  if (!state.selectedQualityId && opts.length) {
    state.selectedQualityId = opts.find((o) => o.id === "1080p")?.id ?? opts[0].id;
  }
  const selected = opts.find((o) => o.id === state.selectedQualityId) ?? opts[0];
  return `
  ${linkInputRow("Paste a video, music or playlist link…")}
  <div class="grid-analyze">
    <figure class="blueprint preview">${corners}
      <div class="thumb">
        ${
          a.thumbnail
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
      <div class="seg" style="align-self:stretch">
        <label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt" value="video" ${state.tab === "video" ? "checked" : ""}>Video</label>
        <label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt" value="audio" ${state.tab === "audio" ? "checked" : ""}>Audio</label>
      </div>
      <div class="qgrid">
        ${opts
          .map(
            (o) =>
              `<div class="qcell" data-q="${o.id}" role="button" aria-selected="${o.id === selected.id}">
                 <span class="q">${esc(o.label)}</span>
                 <span class="sz">≈ ${fmtBytes(o.approxBytes)}</span>
               </div>`
          )
          .join("")}
        ${
          state.tab === "audio" && a.videoOptions.length > opts.length
            ? Array(a.videoOptions.length - opts.length)
                .fill(`<div class="qcell" style="visibility:hidden;pointer-events:none"></div>`)
                .join("")
            : ""
        }
      </div>
      ${optsRow()}
      <button class="btn btn-primary btn-block" id="downloadBtn">⇣ DOWNLOAD · ${fmtBytes(selected.approxBytes).replace("— MB", "best")}</button>
    </div>
  </div>
  ${dirRow()}
  ${historyStrip()}`;
}

function viewPlaylist(): string {
  const a = state.analysis!;
  const opts = state.tab === "video" ? a.videoOptions : a.audioOptions;
  if (!state.selectedQualityId && opts.length) {
    state.selectedQualityId = opts.find((o) => o.id === "1080p")?.id ?? opts[0].id;
  }
  const selected = opts.find((o) => o.id === state.selectedQualityId) ?? opts[0];
  const count = a.playlistCount ?? a.entries.length;
  const selCount = state.entrySelected.filter(Boolean).length;
  const rows = a.entries
    .map((e, i) => {
      const sel = state.entrySelected[i];
      return `<label class="queue-row" style="cursor:pointer;${sel ? "background:var(--color-accent-100)" : ""}" data-entry="${i}">
        <span style="width:14px;height:14px;flex:none;display:grid;place-items:center;font-size:9px;${
          sel
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
      <div class="seg" style="align-self:stretch">
        <label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt" value="video" ${state.tab === "video" ? "checked" : ""}>Video</label>
        <label class="seg-opt" style="flex:1;justify-content:center"><input type="radio" name="fmt" value="audio" ${state.tab === "audio" ? "checked" : ""}>Audio</label>
      </div>
      <div class="qgrid">
        ${opts
          .map(
            (o) =>
              `<div class="qcell" data-q="${o.id}" role="button" aria-selected="${o.id === selected.id}">
                 <span class="q">${esc(o.label)}</span>
                 <span class="sz">≈ ${fmtBytes(o.approxBytes)}</span>
               </div>`
          )
          .join("")}
        ${
          state.tab === "audio" && a.videoOptions.length > opts.length
            ? Array(a.videoOptions.length - opts.length)
                .fill(`<div class="qcell" style="visibility:hidden;pointer-events:none"></div>`)
                .join("")
            : ""
        }
      </div>
      ${optsRow()}
      <button class="btn btn-primary btn-block" id="downloadPlaylist" ${selCount ? "" : "disabled"}>⇣ DOWNLOAD ${selCount} SELECTED</button>
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

function viewDownloading(): string {
  const j = state.job!;
  const indeterminate = j.percent <= 0 || j.stage !== "downloading";
  const stageText =
    j.stage === "merging"
      ? "merging streams (FFmpeg)…"
      : j.stage === "extracting"
      ? "extracting audio (FFmpeg)…"
      : j.stage === "starting"
      ? "starting…"
      : "downloading";
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
      <button class="btn btn-secondary btn-icon" id="cancelBtn" title="Cancel">✕</button>
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
    state.url = (e.target as HTMLInputElement).value;
    const btn = $("#analyzeBtn") as HTMLButtonElement | null;
    if (btn) btn.disabled = !isLikelyUrl(state.url);
  });
  $("#urlInput")?.addEventListener("keydown", (e) => {
    if ((e as KeyboardEvent).key === "Enter" && isLikelyUrl(state.url)) doAnalyze();
  });
  $("#analyzeBtn")?.addEventListener("click", doAnalyze);
  $("#clearLink")?.addEventListener("click", resetToEmpty);
  $("#pickDir")?.addEventListener("click", pickDir);
  $("#downloadBtn")?.addEventListener("click", startDownload);
  $("#cancelBtn")?.addEventListener("click", cancelDownload);
  $("#installBtn")?.addEventListener("click", startInstall);
  $("#installBack")?.addEventListener("click", () => (state.installIsUpdate ? finishToolUpdate() : finishInstall()));
  $("#installRestart")?.addEventListener("click", restartApp);

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

  // quality cells
  body()
    .querySelectorAll<HTMLElement>(".qcell[data-q]")
    .forEach((c) =>
      c.addEventListener("click", () => {
        state.selectedQualityId = c.dataset.q!;
        render();
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

  // playlist
  body()
    .querySelectorAll<HTMLElement>("[data-entry]")
    .forEach((row) =>
      row.addEventListener("click", (e) => {
        e.preventDefault();
        const i = Number(row.dataset.entry);
        state.entrySelected[i] = !state.entrySelected[i];
        render();
      })
    );
  $("#selectAll")?.addEventListener("click", (e) => {
    e.preventDefault();
    const all = state.entrySelected.every(Boolean);
    state.entrySelected = state.entrySelected.map(() => !all);
    render();
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
function cancelInFlightAnalyze() {
  if (state.analyzingId) {
    invoke("cancel_analyze", { id: state.analyzingId }).catch(() => {});
    state.analyzingId = null;
  }
}

function resetToEmpty() {
  cancelInFlightAnalyze();
  state.screen = "empty";
  state.url = "";
  state.analysis = null;
  state.selectedQualityId = "";
  state.tab = "video";
  render();
}

// Same as resetToEmpty() but keeps the URL in the input — used after a
// failed analyze/download so the user can just hit ANALYZE again instead of
// retyping the link they already pasted.
function backToEmptyKeepingUrl() {
  cancelInFlightAnalyze();
  state.screen = "empty";
  state.analysis = null;
  state.selectedQualityId = "";
  state.tab = "video";
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
      <span class="ok">✓</span>
      <div class="b-txt">
        <span class="b-ttl">DONE — SAVED TO DISK</span>
        <span class="b-sub" title="${esc(item.path ?? "")}">${esc(item.path ?? item.title)}${item.sizeLabel ? ` · ${item.sizeLabel}` : ""}</span>
      </div>
      ${item.path ? `<button class="btn btn-primary open-hist" data-path="${esc(item.path)}" style="flex:none">OPEN FOLDER</button>` : ""}
      <span class="icon-x" id="toastClose">✕</span>
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
  wrap.innerHTML = `
    <div class="banner toast error">
      <span class="ok">!</span>
      <div class="b-txt">
        <span class="b-ttl">COULDN'T DO THAT</span>
        <span class="b-sub" style="white-space:normal">${esc(message)}</span>
      </div>
      <span class="icon-x" id="toastClose">✕</span>
    </div>`;
  document.getElementById("toastClose")?.addEventListener("click", dismissToast);
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
  if (result.ffmpeg.updateAvailable) {
    items.push({ tool: "ffmpeg", label: "ffmpeg", from: result.ffmpeg.current, to: result.ffmpeg.latest });
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
  } catch {}
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
        }
      : local;
    if (state.settingsOpen) renderSettingsModal();
  } catch {}
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
      <span class="ok">⇪</span>
      ${rows}
      <span class="icon-x" id="updateToastClose">✕</span>
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

function startInstall() {
  const need = missingTools();
  if (!need.length) return;
  state.installTargets = need;
  state.installIsUpdate = false;
  state.installReturnScreen = null;
  state.installLog = [];
  state.installDone = false;
  state.installError = "";
  state.screen = "installing";
  render();
  invoke("install_tools", { tools: need }).catch((err) => {
    // fallback if the event didn't fire (e.g. failed to launch)
    if (!state.installError && !state.installDone) {
      state.installError = String(err);
      if (state.screen === "installing") render();
    }
  });
}

async function finishInstall() {
  try {
    state.binaries = await invoke<Binaries>("check_binaries");
  } catch {}
  resetToEmpty();
}

// Same screen/log/progress UI as a first-time install, but for updating a
// tool that's already working — triggered from the Settings modal or the
// update toast. Unlike startInstall() this can happen mid-session, so it
// remembers the current screen and returns to it instead of resetting.
function startToolUpdate(tool: string) {
  state.installTargets = [tool];
  state.installIsUpdate = true;
  state.installReturnScreen = state.screen === "installing" ? "empty" : state.screen;
  state.installLog = [];
  state.installDone = false;
  state.installError = "";
  state.screen = "installing";
  render();
  invoke("update_tool", { tool }).catch((err) => {
    if (!state.installError && !state.installDone) {
      state.installError = String(err);
      if (state.screen === "installing") render();
    }
  });
}

async function finishToolUpdate() {
  try {
    state.binaries = await invoke<Binaries>("check_binaries");
  } catch {}
  await checkForUpdates({ toast: false });
  state.screen = state.installReturnScreen ?? "empty";
  state.installReturnScreen = null;
  state.installTargets = [];
  state.installIsUpdate = false;
  render();
  if (state.settingsOpen) renderSettingsModal();
}

function restartApp() {
  invoke("restart_app").catch(() => {});
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
    persistSettings();
    render();
  }
}

async function doAnalyze() {
  if (!isLikelyUrl(state.url)) return;
  cancelInFlightAnalyze(); // editing + resubmitting replaces whatever was running, not queues alongside it
  const id = crypto.randomUUID();
  state.analyzingId = id;
  const urlAtRequest = state.url; // the link as submitted — further edits don't retarget this request
  state.screen = "analyzing";
  render();
  try {
    const res = await invoke<AnalyzeResult>("analyze", { id, url: urlAtRequest, ...cookieParams() });
    if (state.analyzingId !== id) return; // superseded by a newer request meanwhile — drop it
    state.analyzingId = null;
    state.analysis = res;
    state.selectedQualityId = "";
    state.tab = "video";
    if (res.kind === "playlist") {
      state.entrySelected = res.entries.map(() => true);
      state.screen = "playlist";
    } else if (res.kind === "mix") {
      state.mixMode = "single";
      state.screen = "mix";
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

function currentSelection(): QualityOption | undefined {
  const a = state.analysis!;
  const opts = state.tab === "video" ? a.videoOptions : a.audioOptions;
  return opts.find((o) => o.id === state.selectedQualityId) ?? opts[0];
}

async function startDownload() {
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
    name: a.title + (state.tab === "audio" ? `.${state.selectedQualityId}` : ".mp4"),
    sizeLabel: fmtBytes(sel.approxBytes),
  };
  state.screen = "downloading";
  render();
  runDownload({
    id,
    url: a.webpageUrl,
    formatSelector: sel.formatSelector,
    kind: state.tab,
    audioFormat: state.tab === "audio" ? state.selectedQualityId : null,
    outputDir: state.outputDir,
    writeThumbnail: state.writeThumbnail,
    writeDescription: state.writeDescription,
    writeSubs: state.writeSubs,
    playlistItems: null,
  });
}

function startPlaylistDownload() {
  const a = state.analysis!;
  const items = a.entries
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
    kind: state.tab,
    audioFormat: state.tab === "audio" ? state.selectedQualityId : null,
    outputDir: state.outputDir,
    writeThumbnail: state.writeThumbnail,
    writeDescription: state.writeDescription,
    writeSubs: state.writeSubs,
    playlistItems: items,
  });
}

function mixContinue() {
  const a = state.analysis!;
  // Turn the mix into a normal single-video analyze/download path.
  if (state.mixMode === "single") {
    // re-shape analysis as a plain video by re-analyzing without the list
    state.screen = "analyzed";
    // strip the &list= param so download uses the single video
    a.webpageUrl = a.webpageUrl.replace(/[?&]list=[^&]+/g, "").replace(/[?&]index=[^&]+/g, "");
    render();
  } else {
    // capped: hand off to the same quality-picker screen a real playlist
    // uses, pre-selecting just the first N entries — "CONTINUE → PICK
    // QUALITY" should actually let you pick quality, not silently lock in
    // 1080p and start downloading.
    state.entrySelected = a.entries.map((_, i) => i < state.mixN);
    state.selectedQualityId = "";
    state.screen = "playlist";
    render();
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

  await listen<{ id: string; filepath?: string }>("dl-done", (e) => {
    if (!state.job || state.job.id !== e.payload.id) return;
    const item: HistoryItem = {
      title: (state.analysis?.title ?? state.job.name).replace(/\.(mp4|mp3|m4a|webm)$/i, ""),
      ext: e.payload.filepath?.split(".").pop() ?? (state.tab === "audio" ? state.selectedQualityId : "mp4"),
      sizeLabel: state.job.sizeLabel,
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
    state.installLog.push(e.payload);
    if (state.screen === "installing") appendInstallLog();
  });
  await listen("install-done", async () => {
    state.installDone = true;
    // Antivirus real-time scanning can briefly lock a just-downloaded .exe,
    // so the very first check right after "done" can spuriously say it's
    // still missing. Retry a few times before accepting that result.
    for (let attempt = 0; attempt < 6; attempt++) {
      try {
        state.binaries = await invoke<Binaries>("check_binaries");
      } catch {}
      if (missingTools().length === 0) break;
      await new Promise((r) => setTimeout(r, 400));
    }
    if (state.screen === "installing") render();
  });
  await listen<string>("install-error", (e) => {
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
  const stageText =
    j.stage === "merging"
      ? "merging streams (FFmpeg)…"
      : j.stage === "extracting"
      ? "extracting audio (FFmpeg)…"
      : "downloading";
  if (pct) pct.textContent = indeterminate ? stageText : `${j.percent.toFixed(0)}%`;
  if (eta) eta.textContent = `${j.speed}${j.eta ? ` · ${j.eta} left` : ""}`;
  if (stageEl) stageEl.textContent = stageText;
}

// ── persistence ──────────────────────────────────────────────────────────
function persistHistory() {
  try {
    localStorage.setItem("fetch.history", JSON.stringify(state.history.slice(0, 30)));
  } catch {}
}
function loadHistory() {
  try {
    const raw = localStorage.getItem("fetch.history");
    if (raw) state.history = JSON.parse(raw);
  } catch {}
}

function persistSettings() {
  try {
    localStorage.setItem(
      "fetch.settings",
      JSON.stringify({
        outputDir: state.outputDir,
        cookieMode: state.cookieMode,
        cookieBrowser: state.cookieBrowser,
        cookieFile: state.cookieFile,
      })
    );
  } catch {}
}
let hasSavedOutputDir = false;
function loadSettings() {
  try {
    const raw = localStorage.getItem("fetch.settings");
    if (!raw) return;
    const s = JSON.parse(raw);
    if (typeof s.outputDir === "string" && s.outputDir) {
      state.outputDir = s.outputDir;
      hasSavedOutputDir = true;
    }
    if (s.cookieMode === "none" || s.cookieMode === "browser" || s.cookieMode === "file") {
      state.cookieMode = s.cookieMode;
    }
    if (typeof s.cookieBrowser === "string" && s.cookieBrowser) state.cookieBrowser = s.cookieBrowser;
    if (typeof s.cookieFile === "string") state.cookieFile = s.cookieFile;
  } catch {}
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
}

// ── boot ─────────────────────────────────────────────────────────────────
window.addEventListener("DOMContentLoaded", async () => {
  loadHistory();
  loadSettings();
  wireChrome();
  if (!hasSavedOutputDir) state.outputDir = "~/Downloads/Fetch";
  render(); // paint immediately; backend calls below only enrich the view

  try {
    await wireBackendEvents();
  } catch (e) {
    console.warn("Tauri events unavailable (running outside the app shell):", e);
  }
  if (!hasSavedOutputDir) {
    try {
      state.outputDir = await invoke<string>("default_download_dir");
    } catch {}
  }
  try {
    state.binaries = await invoke<Binaries>("check_binaries");
  } catch {
    state.binaries = { ytDlp: false, ffmpeg: false };
  }
  try {
    state.installers = await invoke<Installers>("detect_installers");
  } catch {
    state.installers = { available: false, manager: "", platform: "" };
  }
  render();
  if (state.binaries?.ytDlp && state.binaries?.ffmpeg) {
    checkForUpdates(); // fire-and-forget; toasts only if something's outdated
  }

  // Dev-only hook: lets the UI states be driven for visual review without a
  // live yt-dlp backend. Stripped from production builds.
  if ((import.meta as any).env?.DEV) {
    (window as any).__fetch = { state, render };
  }
});
