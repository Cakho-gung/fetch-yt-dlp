// Central application state, persistence (localStorage), and selectors.

import type {
  AnalyzeResult,
  Binaries,
  CookieImportState,
  HistoryItem,
  Installers,
  QualityOption,
  QuickMeta,
  Screen,
  UpdateCheck,
} from "./types";
import { detectSource } from "./utils";

export const state = {
  screen: "empty" as Screen,
  binaries: null as Binaries | null,
  installers: null as Installers | null,
  installLog: [] as string[],
  installDone: false,
  installError: "",
  installTargets: [] as string[],
  installIsUpdate: false,
  installReturnScreen: null as Screen | null,
  updateCheck: null as UpdateCheck | null,
  checkingUpdates: false,
  outputDir: "",
  hasSavedOutputDir: false,
  dirPinned: false,
  url: "",
  analyzingId: null as string | null,
  analysis: null as AnalyzeResult | null,
  preview: null as QuickMeta | null,
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
  mixResolving: null as "capped" | null,
  // settings & modals
  settingsOpen: false,
  helpOpen: false,
  cookieMode: "none" as "none" | "file",
  cookieBrowser: "chrome",
  cookieFile: "",
  cookieImport: { state: "idle", message: "" } as CookieImportState,
  cookieImportModal: { open: false, browser: "chrome", id: null as string | null },
  cookieImportStopRequested: false,
};

export function requiredTools(url: string): ("yt-dlp" | "ffmpeg" | "spotdl")[] {
  return detectSource(url) === "SPOTIFY" ? ["spotdl", "ffmpeg"] : ["yt-dlp", "ffmpeg"];
}

export function missingTools(): string[] {
  const b = state.binaries;
  if (!b) return [];
  const have: Record<string, boolean> = {
    "yt-dlp": b.ytDlp,
    ffmpeg: b.ffmpeg,
    spotdl: !!b.spotdl,
  };
  return requiredTools(state.url).filter((t) => !have[t]);
}

export function canAnalyze(): boolean {
  return !!state.binaries && missingTools().length === 0;
}

export function currentSelection(): QualityOption | undefined {
  const a = state.analysis;
  if (!a) return undefined;
  const opts = state.tab === "video" ? a.videoOptions : a.audioOptions;
  return opts.find((o) => o.id === state.selectedQualityId) ?? opts[0];
}

export function cookieParams() {
  return {
    cookieMode: state.cookieMode,
    cookieFile: state.cookieMode === "file" ? state.cookieFile : null,
    cookieBrowser: state.cookieMode === "file" ? state.cookieBrowser : null,
  };
}

export function persistHistory() {
  try {
    localStorage.setItem("fetch.history", JSON.stringify(state.history.slice(0, 30)));
  } catch {}
}

export function loadHistory() {
  try {
    const raw = localStorage.getItem("fetch.history");
    if (raw) state.history = JSON.parse(raw);
  } catch {}
}

export function persistSettings() {
  try {
    localStorage.setItem(
      "fetch.settings",
      JSON.stringify({
        outputDir: state.outputDir,
        dirPinned: state.dirPinned,
        cookieMode: state.cookieMode,
        cookieBrowser: state.cookieBrowser,
        cookieFile: state.cookieFile,
      })
    );
  } catch {}
}

export function loadSettings() {
  try {
    const raw = localStorage.getItem("fetch.settings");
    if (!raw) return;
    const s = JSON.parse(raw);
    if (s.dirPinned === true && typeof s.outputDir === "string" && s.outputDir) {
      state.outputDir = s.outputDir;
      state.hasSavedOutputDir = true;
      state.dirPinned = true;
    }
    if (s.cookieMode === "file") {
      state.cookieMode = "file";
    }
    if (typeof s.cookieBrowser === "string" && s.cookieBrowser) {
      state.cookieBrowser = s.cookieBrowser;
    }
    if (typeof s.cookieFile === "string") {
      state.cookieFile = s.cookieFile;
    }
  } catch {}
}
