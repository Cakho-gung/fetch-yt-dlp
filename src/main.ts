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
  manager: string; // "brew" | "winget" | ""
  platform: string;
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
  | "success"
  | "error"
  | "installing";

// ── app state ──────────────────────────────────────────────────────────────
const state = {
  screen: "empty" as Screen,
  binaries: null as Binaries | null,
  installers: null as Installers | null,
  installLog: [] as string[],
  installDone: false,
  installError: "",
  outputDir: "",
  url: "",
  analysis: null as AnalyzeResult | null,
  tab: "video" as "video" | "audio",
  selectedQualityId: "",
  embedSubs: true,
  embedMeta: true,
  errorMsg: "",
  job: null as null | {
    id: string;
    percent: number;
    speed: string;
    eta: string;
    stage: string;
    name: string;
    sizeLabel: string;
  },
  lastResult: null as null | HistoryItem,
  history: [] as HistoryItem[],
  // playlist selection
  entrySelected: [] as boolean[],
  mixMode: "single" as "single" | "capped",
  mixN: 10,
};

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
function render() {
  updateStatusTag();
  const el = body();
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
    case "success":
      el.innerHTML = viewSuccess();
      break;
    case "error":
      el.innerHTML = viewError();
      break;
    case "installing":
      el.innerHTML = viewInstalling();
      break;
  }
  wireEvents();
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
  const actions = canAuto
    ? `<div style="display:flex;align-items:center;gap:var(--space-2);margin-top:var(--space-2)">
         <button class="btn btn-primary" id="installBtn" style="font-size:12.5px">⇣ INSTALL WITH ${esc(mgr.toUpperCase())}</button>
         <span class="text-muted" style="font-size:11.5px">FETCH will run <code>${mgr}</code> for you — ffmpeg can take a few minutes.</span>
       </div>`
    : `<br>macOS: <code>brew install ${need.join(" ")}</code> &nbsp;·&nbsp; Windows: <code>winget install ${need
        .map((n) => (n === "yt-dlp" ? "yt-dlp.yt-dlp" : "Gyan.FFmpeg"))
        .join(" ")}</code>, then reopen FETCH.`;
  return `<div class="warn">
    <span>⚠</span>
    <div>Missing <b>${need.join(" + ")}</b> — analysis and downloads won't run until installed.${actions}</div>
  </div>`;
}

function viewInstalling(): string {
  const need = missingTools();
  const logLines = state.installLog.slice(-200).map(esc).join("\n");
  const done = state.installDone;
  const errored = !!state.installError;
  return `
  <div class="blueprint job" style="border-color:var(--color-accent)">${corners}
    <div class="job-top">
      <div class="job-info">
        <span class="job-name">${
          errored ? "INSTALL FAILED" : done ? "INSTALLED ✓" : `INSTALLING ${need.join(" + ").toUpperCase()}…`
        }</span>
        <span class="job-stage text-muted">${
          errored
            ? esc(state.installError)
            : done
            ? "tools are ready"
            : `running ${esc(state.installers?.manager ?? "installer")} — please wait`
        }</span>
      </div>
      ${
        done || errored
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

function dirRow(): string {
  return `<div class="dir-row">
    <span class="text-muted mono">↧</span>
    <span class="path" title="${esc(state.outputDir)}">${esc(state.outputDir)}</span>
    <button class="btn btn-ghost" id="pickDir">CHANGE…</button>
  </div>`;
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
      (h, i) => `<tr${i === 0 && state.screen === "success" ? ` style="background:var(--color-accent-100)"` : ""}>
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
function linkInputRow(placeholder: string): string {
  const canAnalyze = !!(state.binaries?.ytDlp && state.binaries?.ffmpeg);
  return `<div class="link-row">
    <input class="input" id="urlInput" placeholder="${esc(placeholder)}" value="${esc(state.url)}" spellcheck="false" autocomplete="off" ${canAnalyze ? "" : "disabled"}>
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
  const src = detectSource(a.webpageUrl);
  return `
  <div class="link-row">
    <div class="link-chip">
      <span style="color:var(--color-accent-700);font-size:12px">✓</span>
      <span class="url">${esc(a.webpageUrl)}</span>
      <span class="tag tag-accent">${esc(a.extractor.toUpperCase() || src)}</span>
      <span class="icon-x" id="clearLink">✕</span>
    </div>
    <button class="btn btn-secondary" style="min-height:42px;padding-inline:var(--space-4);color:var(--color-accent-700)">✓ ANALYZED</button>
  </div>
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
      </div>
      ${
        state.tab === "video"
          ? `<div class="opts-row">
        <label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbSubs" ${state.embedSubs ? "checked" : ""}><span class="dot"></span>Subtitles (embed)</label>
        <label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbMeta" ${state.embedMeta ? "checked" : ""}><span class="dot"></span>Thumbnail + metadata</label>
      </div>`
          : `<div class="opts-row">
        <label class="radio" style="font-size:12.5px"><input type="checkbox" id="cbMeta" ${state.embedMeta ? "checked" : ""}><span class="dot"></span>Cover art + metadata</label>
      </div>`
      }
      <button class="btn btn-primary btn-block" id="downloadBtn">⇣ DOWNLOAD · ${fmtBytes(selected.approxBytes).replace("— MB", "best")}</button>
    </div>
  </div>
  ${dirRow()}
  ${historyStrip()}`;
}

function viewPlaylist(): string {
  const a = state.analysis!;
  const count = a.playlistCount ?? a.entries.length;
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
  const selCount = state.entrySelected.filter(Boolean).length;
  return `
  <div class="link-chip" style="min-height:42px;flex:none">
    <span class="url">${esc(a.webpageUrl)}</span>
    <span class="tag tag-accent">PLAYLIST · ${count} VIDEOS</span>
  </div>
  <div style="border:1px solid var(--color-divider)">
    <div style="display:flex;align-items:center;gap:var(--space-3);padding:var(--space-3) var(--space-4);border-bottom:1px solid var(--color-divider)">
      <span style="font:600 15px var(--font-heading)">${esc(a.title.toUpperCase())}</span>
      <span class="text-muted mono" style="font-size:11px">${count} videos${a.entries.length < count ? ` · showing first ${a.entries.length}` : ""}</span>
      <a href="#" id="selectAll" style="margin-left:auto;font-size:12.5px">${selCount === a.entries.length ? "Clear all" : "Select all"}</a>
    </div>
    <div style="max-height:300px;overflow:auto">${rows}</div>
    <div style="display:flex;align-items:center;gap:var(--space-2);padding:var(--space-3) var(--space-4);border-top:1px solid var(--color-divider)">
      <span class="mono" style="font-size:12px;color:var(--color-accent-700)">${selCount}/${a.entries.length} selected</span>
      <button class="btn btn-secondary" id="clearLink" style="margin-left:auto">✕ CANCEL</button>
      <button class="btn btn-primary" id="downloadPlaylist" ${selCount ? "" : "disabled"}>⇣ DOWNLOAD ${selCount} SELECTED</button>
    </div>
  </div>
  ${dirRow()}`;
}

function viewMix(): string {
  const a = state.analysis!;
  return `
  <div class="link-chip" style="border-color:var(--color-neutral-700);flex:none">
    <span style="font-size:13px;color:var(--color-neutral-700)">∞</span>
    <span class="url">${esc(a.webpageUrl)}</span>
    <span class="tag tag-neutral" style="font-weight:700">MIX · ENDLESS</span>
  </div>
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

function viewSuccess(): string {
  const r = state.lastResult!;
  return `
  <div class="banner">
    <span class="ok">✓</span>
    <div class="b-txt">
      <span class="b-ttl">DONE — SAVED TO DISK</span>
      <span class="b-sub" title="${esc(r.path ?? "")}">${esc(r.path ?? r.title)}${r.sizeLabel ? ` · ${r.sizeLabel}` : ""}</span>
    </div>
    ${r.path ? `<button class="btn btn-primary open-hist" data-path="${esc(r.path)}" style="flex:none">OPEN FOLDER</button>` : ""}
    <span class="icon-x" id="dismissBanner">✕</span>
  </div>
  ${linkInputRow("Paste the next link…")}
  ${historyTable()}`;
}

function viewError(): string {
  return `
  <div class="banner error">
    <span class="ok">!</span>
    <div class="b-txt">
      <span class="b-ttl">COULDN'T DO THAT</span>
      <span class="b-sub" style="white-space:normal">${esc(state.errorMsg)}</span>
    </div>
    <button class="btn btn-secondary" id="backBtn" style="flex:none">BACK</button>
  </div>
  ${linkInputRow("Paste a link to try again…")}
  ${historyStrip()}`;
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
  $("#dismissBanner")?.addEventListener("click", resetToEmpty);
  $("#backBtn")?.addEventListener("click", resetToEmpty);
  $("#pickDir")?.addEventListener("click", pickDir);
  $("#downloadBtn")?.addEventListener("click", startDownload);
  $("#cancelBtn")?.addEventListener("click", cancelDownload);
  $("#installBtn")?.addEventListener("click", startInstall);
  $("#installBack")?.addEventListener("click", finishInstall);

  // format tabs (video/audio)
  body()
    .querySelectorAll<HTMLInputElement>('input[name="fmt"]')
    .forEach((r) =>
      r.addEventListener("change", () => {
        if (state.screen !== "analyzed") return;
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

  $("#cbSubs")?.addEventListener("change", (e) => {
    state.embedSubs = (e.target as HTMLInputElement).checked;
  });
  $("#cbMeta")?.addEventListener("change", (e) => {
    state.embedMeta = (e.target as HTMLInputElement).checked;
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
function resetToEmpty() {
  state.screen = "empty";
  state.url = "";
  state.analysis = null;
  state.selectedQualityId = "";
  state.tab = "video";
  render();
}

function startInstall() {
  const need = missingTools();
  if (!need.length) return;
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
    render();
  }
}

async function doAnalyze() {
  if (!isLikelyUrl(state.url)) return;
  state.screen = "analyzing";
  render();
  try {
    const res = await invoke<AnalyzeResult>("analyze", { url: state.url });
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
    state.errorMsg = String(err);
    state.screen = "error";
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
    embedSubs: state.embedSubs,
    embedMetadata: state.embedMeta,
    playlistItems: null,
    title: a.title,
  });
}

function startPlaylistDownload() {
  const a = state.analysis!;
  const items = a.entries
    .filter((_, i) => state.entrySelected[i])
    .map((e) => e.index)
    .join(",");
  if (!items) return;
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
    formatSelector: "bv*[height<=1080]+ba/b[height<=1080]",
    kind: "video",
    audioFormat: null,
    outputDir: state.outputDir,
    embedSubs: state.embedSubs,
    embedMetadata: state.embedMeta,
    playlistItems: items,
    title: a.title,
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
    // capped: download first N as a playlist selection
    const id = crypto.randomUUID();
    state.job = {
      id,
      percent: 0,
      speed: "",
      eta: "",
      stage: "starting",
      name: `${a.title} — first ${state.mixN}`,
      sizeLabel: "",
    };
    state.screen = "downloading";
    render();
    runDownload({
      id,
      url: a.webpageUrl,
      formatSelector: "bv*[height<=1080]+ba/b[height<=1080]",
      kind: "video",
      audioFormat: null,
      outputDir: state.outputDir,
      embedSubs: state.embedSubs,
      embedMetadata: state.embedMeta,
      playlistItems: `1:${state.mixN}`,
      title: a.title,
    });
  }
}

function runDownload(req: Record<string, unknown>) {
  invoke("download", { req }).catch((err) => {
    // the dl-error event already handles UI; keep this as a fallback
    if (state.screen === "downloading") {
      state.errorMsg = String(err);
      state.screen = "error";
      render();
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
    state.lastResult = item;
    state.job = null;
    state.screen = "success";
    render();
  });

  await listen<{ id: string; message: string }>("dl-error", (e) => {
    if (!state.job || state.job.id !== e.payload.id) return;
    state.job = null;
    state.errorMsg = e.payload.message;
    state.screen = "error";
    render();
  });

  // installer events
  await listen<string>("install-log", (e) => {
    state.installLog.push(e.payload);
    if (state.screen === "installing") appendInstallLog();
  });
  await listen("install-done", async () => {
    state.installDone = true;
    try {
      state.binaries = await invoke<Binaries>("check_binaries");
    } catch {}
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

// ── theme toggle ─────────────────────────────────────────────────────────
function wireChrome() {
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
}

// ── boot ─────────────────────────────────────────────────────────────────
window.addEventListener("DOMContentLoaded", async () => {
  loadHistory();
  wireChrome();
  state.outputDir = "~/Downloads/Fetch";
  render(); // paint immediately; backend calls below only enrich the view

  try {
    await wireBackendEvents();
  } catch (e) {
    console.warn("Tauri events unavailable (running outside the app shell):", e);
  }
  try {
    state.outputDir = await invoke<string>("default_download_dir");
  } catch {}
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

  // Dev-only hook: lets the UI states be driven for visual review without a
  // live yt-dlp backend. Stripped from production builds.
  if ((import.meta as any).env?.DEV) {
    (window as any).__fetch = { state, render };
  }
});
