/**
 * util.ts — shared helpers for parsers and the CLI: robust JSONL/JSON reading,
 * timestamp normalization to epoch seconds, content flattening, secret
 * redaction + display budget, and the EventBuilder every parser emits through.
 */

// --- sanitization budget (display only; stored text is full) ---
export const MAX_TEXT_CHARS = 4000;

const REDACTION_PATTERNS: RegExp[] = [
  /\bsk-ant-[A-Za-z0-9_-]{16,}\b/g,
  /\bsk-[A-Za-z0-9_-]{16,}\b/g,
  /\bgh[pousr]_[A-Za-z0-9]{20,}\b/g,
  /\bBearer\s+[A-Za-z0-9._-]{20,}\b/gi,
  /\bxox[baprs]-[A-Za-z0-9-]{20,}\b/g,
];

export function redact(text: string): string {
  let out = text;
  for (const p of REDACTION_PATTERNS) out = out.replace(p, "[REDACTED]");
  return out;
}

/** Redact secrets, then cap length. Used when emitting event text to callers. */
export function truncate(text: string): string {
  const s = redact(text);
  return s.length <= MAX_TEXT_CHARS ? s : `${s.slice(0, MAX_TEXT_CHARS)}\n…(truncated)…`;
}

// --- timestamps ---

/** ISO-8601 (with or without trailing Z) → epoch seconds. null on failure. */
export function isoToEpochSec(value: unknown): number | null {
  if (typeof value !== "string" || !value) return null;
  const ms = Date.parse(value);
  return Number.isNaN(ms) ? null : Math.floor(ms / 1000);
}

/** Epoch milliseconds (number or numeric string) → epoch seconds. */
export function millisToEpochSec(value: unknown): number | null {
  const n = typeof value === "string" ? Number(value) : value;
  if (typeof n !== "number" || !Number.isFinite(n)) return null;
  return Math.floor(n / 1000);
}

// --- content flattening ---

/** Stringify any value compactly for FTS text (objects → JSON, never throws). */
export function compact(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

/**
 * Flatten a content field that may be a string, an array of blocks, or a single
 * block object, into plain text. Handles the common {text} / {content} block
 * shapes across agents; unknown blocks fall back to compact JSON.
 */
export function flattenContent(value: unknown): string {
  if (value == null) return "";
  if (typeof value === "string") return value;
  if (Array.isArray(value)) {
    const parts: string[] = [];
    for (const block of value) {
      if (typeof block === "string") {
        if (block) parts.push(block);
        continue;
      }
      if (block && typeof block === "object") {
        const b = block as Record<string, unknown>;
        if (typeof b.text === "string" && b.text) parts.push(b.text);
        else if (typeof b.content === "string" && b.content) parts.push(b.content);
        else parts.push(compact(block));
      }
    }
    return parts.join("\n");
  }
  if (typeof value === "object") {
    const b = value as Record<string, unknown>;
    if (typeof b.text === "string") return b.text;
    if (typeof b.content === "string") return b.content;
    return compact(value);
  }
  return String(value);
}

// --- JSONL / JSON reading ---

/** Parse one JSON line; returns null on blank/invalid. */
export function parseJsonLine(line: string): Record<string, unknown> | null {
  const t = line.trim();
  if (!t) return null;
  try {
    const v = JSON.parse(t);
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

/** Read a JSONL file into parsed records (skips blanks/invalid). Empty on error. */
export function readJsonl(path: string): Record<string, unknown>[] {
  let text: string;
  try {
    text = require("fs").readFileSync(path, "utf8");
  } catch {
    return [];
  }
  const out: Record<string, unknown>[] = [];
  for (const line of text.split("\n")) {
    const r = parseJsonLine(line);
    if (r) out.push(r);
  }
  return out;
}

/** Read + parse a JSON file. null on any error. */
export function readJson(path: string): unknown {
  try {
    return JSON.parse(require("fs").readFileSync(path, "utf8"));
  } catch {
    return null;
  }
}

export function asStr(v: unknown): string | null {
  return typeof v === "string" ? v : null;
}
export function asNum(v: unknown): number | null {
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string" && v.trim() !== "" && Number.isFinite(Number(v))) return Number(v);
  return null;
}
export function asDict(v: unknown): Record<string, unknown> {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : {};
}

/** RawSource for a plain file (jsonl/json) with size+mtime for drift detection. */
export function fileSource(path: string, kind: "jsonl" | "json"): import("./types.js").RawSource {
  let bytes: number | null = null;
  let mtime: number | null = null;
  try {
    const st = require("fs").statSync(path);
    bytes = st.size;
    mtime = Math.floor(st.mtimeMs / 1000);
  } catch {
    /* ignore */
  }
  return { path, kind, bytes, mtime };
}

// --- event building ---

import type { Event, EventKind } from "./types.js";

/** Optional per-event fields a parser can attach beyond kind/text/ts. */
export interface EventAttrs {
  subtype?: string | null;
  name?: string | null;
  callId?: string | null;
  isError?: boolean | null;
  nativeId?: string | null;
  parentId?: string | null;
  tokens?: number | null;
}

/**
 * Accumulates ordered events with a running idx, dropping empty-text pieces
 * (except `usage`, which is legitimately text-light — we keep it for its token
 * count). Every parser builds through one of these so idx assignment and
 * empty handling stay identical across agents.
 */
export class EventBuilder {
  private evs: Event[] = [];
  private i = 0;

  push(kind: EventKind, text: string, ts: number | null, attrs: EventAttrs = {}): void {
    const t = typeof text === "string" ? text : compact(text);
    if (!t && kind !== "usage") return;
    this.evs.push({
      idx: this.i++,
      kind,
      subtype: attrs.subtype ?? null,
      name: attrs.name ?? null,
      callId: attrs.callId ?? null,
      isError: attrs.isError ?? null,
      nativeId: attrs.nativeId ?? null,
      parentId: attrs.parentId ?? null,
      tokens: attrs.tokens ?? null,
      text: t,
      ts,
    });
  }

  get events(): Event[] {
    return this.evs;
  }
}
