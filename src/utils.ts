// DOM helpers, formatting utilities, and URL/error classifiers.

export const COOKIE_BROWSERS = [
  "chrome",
  "edge",
  "firefox",
  "brave",
  "opera",
  "vivaldi",
  "safari",
];

export const BREW_INSTALL_CMD =
  '/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"';

export const $ = <T extends HTMLElement = HTMLElement>(sel: string) =>
  document.querySelector<T>(sel);

export const body = () => $("#body")!;

export const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) =>
    ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!)
  );

export function fmtBytes(b?: number): string {
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

export function fmtDuration(s?: number): string {
  if (s == null) return "";
  const t = Math.round(s);
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const sec = t % 60;
  const pad = (x: number) => String(x).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`;
}

export function fmtViews(v?: number): string {
  if (v == null) return "";
  if (v >= 1e6) return `${(v / 1e6).toFixed(1)}M views`;
  if (v >= 1e3) return `${(v / 1e3).toFixed(1)}K views`;
  return `${v} views`;
}

export function relTime(at: number): string {
  const d = (Date.now() - at) / 1000;
  if (d < 60) return "just now";
  if (d < 3600) return `${Math.floor(d / 60)} min ago`;
  if (d < 86400) return `${Math.floor(d / 3600)} h ago`;
  return `${Math.floor(d / 86400)} d ago`;
}

export function detectSource(url: string): string {
  const trimmed = url.trim();
  if (/^spotify:/i.test(trimmed)) return "SPOTIFY";
  try {
    const h = new URL(trimmed).hostname.replace(/^www\./, "");
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

export const isLikelyUrl = (s: string) => {
  const t = s.trim();
  return (
    /^https?:\/\/\S+\.\S+/.test(t) ||
    /^spotify:(track|album|playlist|artist|episode|show):[A-Za-z0-9]+/i.test(t)
  );
};

export const isLikelyMixUrl = (s: string) => /[?&]list=RD/.test(s);

export function audioExtLabel(id: string): string {
  if (id.startsWith("mp3")) return "mp3";
  if (id === "m4a") return "m4a";
  return "audio";
}

export function isCookieError(msg: string): boolean {
  return /cookie|sign[- ]?in|log[- ]?in|login|private|age[- ]?restrict|members?-only|account|token/i.test(
    msg
  );
}

export function isHomebrewMissingError(msg: string): boolean {
  return /homebrew isn'?t installed/i.test(msg);
}

export function isFormatUnavailableError(msg: string): boolean {
  return /requested format is not available/i.test(msg);
}
