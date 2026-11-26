// Shared TypeScript types mirroring the Rust backend structs and UI state.

export interface Binaries {
  ytDlp: boolean;
  ffmpeg: boolean;
  spotdl: boolean;
  ytDlpPath?: string;
  ffmpegPath?: string;
  spotdlPath?: string;
}

export interface Installers {
  available: boolean;
  manager: string; // "mac" | "fetch" | ""
  platform: string;
  brewAvailable: boolean;
}

export interface ToolUpdate {
  current?: string;
  latest?: string;
  updateAvailable: boolean;
  source: string; // "portable" | "brew" | "other" | ""
}

export interface UpdateCheck {
  ytDlp: ToolUpdate;
  ffmpeg: ToolUpdate;
  spotdl: ToolUpdate;
}

export interface CookieImportResult {
  path: string;
  cookieCount: number;
  warning?: string;
}

export interface QualityOption {
  id: string;
  label: string;
  height?: number;
  approxBytes?: number;
  formatSelector: string;
  kind: "video" | "audio";
}

export interface PlaylistEntry {
  index: number;
  title: string;
  durationSeconds?: number;
  thumbnail?: string;
  url?: string; // Spotify only: the individual track URL
}

export interface AnalyzeResult {
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

export interface HistoryItem {
  title: string;
  ext: string;
  sizeLabel: string;
  path?: string;
  at: number;
}

export type Screen =
  | "empty"
  | "analyzing"
  | "analyzed"
  | "playlist"
  | "mix"
  | "downloading"
  | "installing";

export type CookieImportState = {
  state: "idle" | "busy" | "done" | "error";
  message: string;
};

export type HelpTopic = "download" | "homebrew";

export interface QuickMeta {
  title: string;
  uploader?: string;
  thumbnail?: string;
  durationSeconds?: number;
  extractor: string;
}
