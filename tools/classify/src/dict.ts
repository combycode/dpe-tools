/**
 * Dict loader. Auto-detects by file extension:
 *   .txt              → flat (single-column rows)
 *   .tsv / .csv       → flat (multi-column rows)
 *   .json             → obj (top-level key → arbitrary JSONValue)
 *   .jsonl / .ndjson  → obj (array of records; key field taken from settings.key)
 *
 * Flat row format auto-detected per line:
 *   - contains TAB        → split on \t
 *   - contains " : "      → split on " : "
 *   - else                → single-column
 *
 * Blank lines and `#`-comment lines ignored.
 */

import { readFileSync } from "node:fs";
import { extname } from "node:path";
import type { JSONValue } from "@combycode/dpe-framework-ts";

export type FlatDict = { kind: "flat"; rows: string[][] };
export type ObjDict = { kind: "obj"; entries: Record<string, JSONValue>; keys: string[] };
export type Dict = FlatDict | ObjDict;

export interface DictLoadOptions {
    /** For JSONL: the record field to use as the primary key. */
    key?: string;
}

export function loadDict(path: string, opts: DictLoadOptions = {}): Dict {
    const ext = extname(path).toLowerCase();
    if (ext === ".json") return loadJson(path);
    if (ext === ".jsonl" || ext === ".ndjson") return loadJsonl(path, opts.key ?? "id");
    return loadFlat(path);
}

// ─── flat (.txt / .tsv / .csv) ─────────────────────────────────────
function loadFlat(path: string): FlatDict {
    const raw = readFileSync(path, "utf-8");
    const rows: string[][] = [];
    for (const rawLine of raw.split(/\r?\n/)) {
        const line = rawLine.replace(/\uFEFF/, "").trimEnd();
        if (!line || line.startsWith("#")) continue;
        if (line.includes("\t")) {
            rows.push(line.split("\t").map((s) => s.trim()));
        } else if (line.includes(" : ")) {
            rows.push(line.split(" : ").map((s) => s.trim()));
        } else {
            rows.push([line.trim()]);
        }
    }
    return { kind: "flat", rows };
}

// ─── obj from single JSON object ───────────────────────────────────
function loadJson(path: string): ObjDict {
    const raw = readFileSync(path, "utf-8");
    const parsed = JSON.parse(raw);

    if (Array.isArray(parsed)) {
        throw new Error(
            `${path}: expected top-level object (key→value). ` + `For array-of-records, use .jsonl / .ndjson format.`,
        );
    }
    if (parsed === null || typeof parsed !== "object") {
        throw new Error(`${path}: JSON root must be an object`);
    }
    const entries = parsed as Record<string, JSONValue>;
    return { kind: "obj", entries, keys: Object.keys(entries) };
}

// ─── obj from JSONL (one record per line) ──────────────────────────
function loadJsonl(path: string, keyField: string): ObjDict {
    const raw = readFileSync(path, "utf-8");
    const entries: Record<string, JSONValue> = {};
    const keys: string[] = [];
    let lineNo = 0;
    for (const line of raw.split(/\r?\n/)) {
        lineNo++;
        const trimmed = line.replace(/\uFEFF/, "").trim();
        if (!trimmed || trimmed.startsWith("#")) continue;
        let obj: unknown;
        try {
            obj = JSON.parse(trimmed);
        } catch (e) {
            throw new Error(`${path}:${lineNo}: invalid JSON — ${(e as Error).message}`);
        }
        if (!obj || typeof obj !== "object" || Array.isArray(obj)) {
            throw new Error(`${path}:${lineNo}: expected JSON object`);
        }
        const rec = obj as Record<string, JSONValue>;
        const key = rec[keyField];
        if (typeof key !== "string" || !key) {
            throw new Error(`${path}:${lineNo}: missing or non-string key field '${keyField}'`);
        }
        if (!(key in entries)) keys.push(key);
        entries[key] = rec;
    }
    return { kind: "obj", entries, keys };
}

// ─── legacy helper kept for main.ts bootstrap (returns flat row form) ──
export function loadList(path: string): string[] {
    const d = loadFlat(path);
    return d.rows.map((r) => r[0] ?? "").filter((s) => s.length > 0);
}
