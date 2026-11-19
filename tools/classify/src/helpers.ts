/**
 * Helper library exposed to rule expressions.
 * Every helper is pure (no state beyond input args).
 *
 * Dict helpers operate on both flat (TSV/text) and obj (JSON/JSONL) dicts via
 * the tagged-union Dict type. Uniqueness + array-flatten is applied where
 * semantically useful (values, pairs).
 */

import { hashString } from "@combycode/dpe-framework-ts";
import type { JSONValue } from "@combycode/dpe-framework-ts";
import type { Dict, FlatDict, ObjDict } from "./dict";

export type { Dict, FlatDict, ObjDict } from "./dict";

export interface HelperBindings {
    classes: string[];
    classesSet: Set<string>; // O(1) FOUND/NEW check
    dicts: Record<string, Dict>;
    _dictCol0Sorted: Record<string, string[]>; // for smart words()
}

export function makeBindings(classes: string[], dicts: Record<string, Dict | string[][]>): HelperBindings {
    const normalised: Record<string, Dict> = {};
    for (const [name, d] of Object.entries(dicts)) {
        normalised[name] = Array.isArray(d) ? { kind: "flat", rows: d } : d;
    }
    const col0Sorted: Record<string, string[]> = {};
    for (const [name, d] of Object.entries(normalised)) {
        col0Sorted[name] = primaryKeyList(d)
            .filter((s) => s.length > 0)
            .sort((a, b) => b.length - a.length);
    }
    return { classes, classesSet: new Set(classes), dicts: normalised, _dictCol0Sorted: col0Sorted };
}

// ═══════════════════════════════════════════════════════════════════════
// Dict primary-key / column access
// ═══════════════════════════════════════════════════════════════════════

/** Primary-key list for a dict: col-0 values (flat) or object keys (obj). */
function primaryKeyList(d: Dict): string[] {
    if (d.kind === "flat") return d.rows.map((r) => r[0] ?? "");
    return d.keys;
}

/** All primary keys of a dict, deduped, in natural order. */
export function keys(dictName: string, bindings: HelperBindings): string[] {
    const d = bindings.dicts[dictName];
    if (!d) return [];
    return uniq(primaryKeyList(d).filter((k) => k.length > 0));
}

/**
 * values() — retrieve unique field values from dict records, with array flatten.
 *
 *   values(name, field)
 *       → all records' `field` values, flattened + unique
 *   values(name, keyValue, field)
 *       → records where primary key == keyValue, field values, flattened + unique
 *   values(name, filterField, filterValue, collectField)
 *       → records where filterField == filterValue, collectField values, flattened + unique
 *
 * `field` is number (column index for flat dicts) or string (field name for obj dicts).
 * For flat dicts, numeric field is the column; string field errors unless ... (future: header support).
 */
export function values(dictName: string, ...args: unknown[]): JSONValue[] {
    throw new Error("values() called without bindings — use compiled rule binding");
}

export function valuesImpl(dictName: string, args: unknown[], bindings: HelperBindings): JSONValue[] {
    const d = bindings.dicts[dictName];
    if (!d) return [];

    if (args.length === 1) {
        return collectField(d, d.kind === "flat" ? primaryKeyList(d) : d.keys, fieldSelector(d, "all"), args[0]);
    }
    if (args.length === 2) {
        const [keyVal, field] = args;
        const matchKeys = matchByPrimaryKey(d, keyVal as JSONValue);
        return collectField(d, matchKeys, fieldSelector(d, "byPkey", matchKeys), field);
    }
    if (args.length === 3) {
        const [fField, fVal, cField] = args;
        const matchKeys = matchByField(d, fField, fVal as JSONValue);
        return collectField(d, matchKeys, fieldSelector(d, "byPkey", matchKeys), cField);
    }
    return [];
}

// Helper internal: get list of matching primary keys for a filter by primary key.
function matchByPrimaryKey(d: Dict, keyVal: JSONValue): string[] {
    if (d.kind === "flat") {
        return d.rows.filter((r) => r[0] === keyVal).map((r) => r[0] ?? "");
    }
    const sKey = typeof keyVal === "string" ? keyVal : String(keyVal);
    return sKey in d.entries ? [sKey] : [];
}

// Helper internal: get matching keys where field == value.
function matchByField(d: Dict, field: unknown, value: JSONValue): string[] {
    if (d.kind === "flat") {
        const idx = typeof field === "number" ? field : -1;
        if (idx < 0) return [];
        const out: string[] = [];
        for (const r of d.rows) {
            if (r[idx] === value) out.push(r[0] ?? "");
        }
        return out;
    }
    const fname = typeof field === "string" ? field : String(field);
    const out: string[] = [];
    for (const k of d.keys) {
        const rec = d.entries[k];
        if (
            rec &&
            typeof rec === "object" &&
            !Array.isArray(rec) &&
            (rec as Record<string, JSONValue>)[fname] === value
        ) {
            out.push(k);
        }
    }
    return out;
}

// Opaque selector used by collectField to iterate records.
type Selector = { all: true } | { keys: string[] };
function fieldSelector(_d: Dict, mode: "all" | "byPkey", keysArr?: string[]): Selector {
    if (mode === "all") return { all: true };
    return { keys: keysArr ?? [] };
}

function collectField(d: Dict, _primaryKeys: string[], selector: Selector, field: unknown): JSONValue[] {
    const out: JSONValue[] = [];
    const seen = new Set<string>();
    const pushOne = (v: JSONValue) => {
        if (v == null) return;
        const k = valueKey(v);
        if (seen.has(k)) return;
        seen.add(k);
        out.push(v);
    };
    const pushMany = (v: JSONValue) => {
        if (Array.isArray(v)) {
            for (const x of v) pushOne(x as JSONValue);
        } else pushOne(v);
    };

    if (d.kind === "flat") {
        const idx = typeof field === "number" ? field : -1;
        const rows = selectRows(d, selector);
        for (const r of rows) {
            if (idx >= 0 && idx < r.length) {
                const cell = r[idx];
                if (cell !== undefined && cell !== "") pushMany(cell as JSONValue);
            }
        }
    } else {
        const fname = typeof field === "string" ? field : "";
        const keysToScan = "all" in selector ? d.keys : selector.keys;
        for (const k of keysToScan) {
            const rec = d.entries[k];
            if (fname === "") continue;
            if (rec && typeof rec === "object" && !Array.isArray(rec)) {
                const v = (rec as Record<string, JSONValue>)[fname];
                if (v !== undefined) pushMany(v);
            }
        }
    }
    return out;
}

function selectRows(d: FlatDict, sel: Selector): string[][] {
    if ("all" in sel) return d.rows;
    const set = new Set(sel.keys);
    return d.rows.filter((r) => set.has(r[0] ?? ""));
}

function valueKey(v: JSONValue): string {
    if (v === null) return "__null";
    if (typeof v === "object") return JSON.stringify(v);
    return String(v);
}

/** Return the whole record for `key`. Row array (flat) / object (obj). `null` if missing. */
export function entry(dictName: string, key: string, bindings: HelperBindings): JSONValue | null {
    const d = bindings.dicts[dictName];
    if (!d) return null;
    if (d.kind === "flat") {
        const r = d.rows.find((row) => row[0] === key);
        return r ? (r as unknown as JSONValue) : null;
    }
    return key in d.entries ? (d.entries[key] ?? null) : null;
}

/** Return ALL records where `field` == `value`. */
export function members(
    dictName: string,
    field: number | string,
    value: JSONValue,
    bindings: HelperBindings,
): JSONValue[] {
    const d = bindings.dicts[dictName];
    if (!d) return [];
    if (d.kind === "flat") {
        const idx = typeof field === "number" ? field : -1;
        if (idx < 0) return [];
        return d.rows.filter((r) => r[idx] === value) as unknown as JSONValue[];
    }
    const fname = typeof field === "string" ? field : String(field);
    const out: JSONValue[] = [];
    for (const k of d.keys) {
        const rec = d.entries[k];
        if (
            rec &&
            typeof rec === "object" &&
            !Array.isArray(rec) &&
            (rec as Record<string, JSONValue>)[fname] === value
        ) {
            out.push(rec);
        }
    }
    return out;
}

/**
 * pairs() — key-lookup helper. Find rows containing `key` anywhere (flat) or
 * records where any field equals `key` (obj). Return either the cell at `col`
 * or all non-key cells. Row-major, deduplicated.
 */
export function pairs(
    dictName: string,
    key: string,
    col: number | string | undefined,
    bindings: HelperBindings,
): JSONValue[] {
    const d = bindings.dicts[dictName];
    if (!d) return [];
    const out: JSONValue[] = [];
    const seen = new Set<string>();
    const push = (v: unknown) => {
        if (v === undefined || v === null || v === "") return;
        const k = valueKey(v as JSONValue);
        if (seen.has(k)) return;
        seen.add(k);
        out.push(v as JSONValue);
    };

    if (d.kind === "flat") {
        const colIdx = typeof col === "number" ? col : -1;
        for (const row of d.rows) {
            if (!row.some((c) => c === key)) continue;
            if (colIdx >= 0) {
                push(row[colIdx]);
            } else {
                for (const cell of row) {
                    if (cell !== key) push(cell);
                }
            }
        }
    } else {
        const colField = typeof col === "string" ? col : "";
        for (const k of d.keys) {
            const rec = d.entries[k];
            if (k === key) {
                if (colField) push(getField(rec, colField));
                else if (rec && typeof rec === "object" && !Array.isArray(rec)) {
                    for (const [fname, v] of Object.entries(rec as Record<string, JSONValue>)) {
                        if (v !== key) push(v);
                    }
                }
                continue;
            }
            // key may appear in field values
            if (rec && typeof rec === "object" && !Array.isArray(rec)) {
                const fields = Object.entries(rec as Record<string, JSONValue>);
                if (fields.some(([, v]) => v === key)) {
                    if (colField) push(getField(rec, colField));
                    else {
                        push(k);
                        for (const [, v] of fields) {
                            if (v !== key) push(v);
                        }
                    }
                }
            }
        }
    }
    return out;
}

function getField(rec: JSONValue, field: string): JSONValue | undefined {
    if (rec && typeof rec === "object" && !Array.isArray(rec)) {
        return (rec as Record<string, JSONValue>)[field];
    }
    return undefined;
}

/** Legacy alias() — col 0 → col 1 lookup for flat dicts. Returns null if missing or if dict is obj. */
export function alias(dictName: string, v: string, bindings: HelperBindings): string | null {
    if (!v) return null;
    const d = bindings.dicts[dictName];
    if (!d || d.kind !== "flat") return null;
    const vl = v.toLowerCase();
    for (const row of d.rows) {
        if ((row[0] ?? "").toLowerCase() === vl) return row[1] ?? null;
    }
    return null;
}

export function aliasReverse(dictName: string, v: string, bindings: HelperBindings): string | null {
    if (!v) return null;
    const d = bindings.dicts[dictName];
    if (!d || d.kind !== "flat") return null;
    const vl = v.toLowerCase();
    for (const row of d.rows) {
        if ((row[1] ?? "").toLowerCase() === vl) return row[0] ?? null;
    }
    return null;
}

export function lookup(
    dictName: string,
    v: string,
    opts: { key?: number; value?: number } = {},
    bindings?: HelperBindings,
): string | null {
    if (!bindings || !v) return null;
    const d = bindings.dicts[dictName];
    if (!d || d.kind !== "flat") return null;
    const key = opts.key ?? 0;
    const value = opts.value ?? 1;
    const vl = v.toLowerCase();
    for (const row of d.rows) {
        if ((row[key] ?? "").toLowerCase() === vl) return row[value] ?? null;
    }
    return null;
}

/** Raw dict escape hatch. */
export function dict(dictName: string, bindings: HelperBindings): Dict | null {
    return bindings.dicts[dictName] ?? null;
}

// ═══════════════════════════════════════════════════════════════════════
// Path / string extraction
// ═══════════════════════════════════════════════════════════════════════

export function split(v: string, seps: string): string[] {
    if (!v) return [];
    const chars = new Set(seps.split(""));
    const out: string[] = [];
    let cur = "";
    for (const c of v) {
        if (chars.has(c)) {
            if (cur.length) {
                out.push(cur);
                cur = "";
            }
        } else cur += c;
    }
    if (cur.length) out.push(cur);
    return out;
}

export function segment(v: string, idx: number, sep = "/"): string {
    if (!v) return "";
    const parts = v.split(sep).filter((p) => p.length > 0);
    const i = idx < 0 ? parts.length + idx : idx;
    return parts[i] ?? "";
}

export function fileName(path: string): string {
    if (!path) return "";
    const parts = path.split(/[\\/]/).filter((p) => p.length);
    return parts[parts.length - 1] ?? "";
}

export function fileStem(path: string): string {
    const name = fileName(path);
    const dot = name.lastIndexOf(".");
    return dot > 0 ? name.slice(0, dot) : name;
}

export function fileExt(path: string): string {
    const name = fileName(path);
    const dot = name.lastIndexOf(".");
    return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

export function get(obj: unknown, path: string): unknown {
    if (obj == null || !path) return obj;
    const parts = path.split(".");
    let cur: unknown = obj;
    for (const p of parts) {
        if (cur == null || typeof cur !== "object") return undefined;
        cur = (cur as Record<string, unknown>)[p];
    }
    return cur;
}

/** Smart tokenizer: greedy-longest-match against dict primary keys. */
export function words(v: string, dictName: string | undefined, bindings: HelperBindings): string[] {
    if (!v) return [];
    const defaultTokens = (s: string): string[] =>
        s.split(/[\s,;:!?()\[\]{}<>"`\\\/|+=*&^%$#@~_–—]+/).filter((t) => t.length > 0);

    if (!dictName) return defaultTokens(v);
    const entries = bindings._dictCol0Sorted[dictName];
    if (!entries || entries.length === 0) return defaultTokens(v);

    const lower = v.toLowerCase();
    const result: string[] = [];
    let i = 0;
    while (i < v.length) {
        while (i < v.length && /[\s,;:!?()\[\]{}<>"`\\\/|+=*&^%$#@~_–—]/.test(v[i]!)) i++;
        if (i >= v.length) break;
        let matched: string | null = null;
        for (const entry of entries) {
            if (lower.startsWith(entry.toLowerCase(), i)) {
                matched = entry;
                break;
            }
        }
        if (matched !== null) {
            result.push(v.slice(i, i + matched.length));
            i += matched.length;
            continue;
        }
        const rest = v.slice(i);
        const nextBreak = rest.search(/[\s,;:!?()\[\]{}<>"`\\\/|+=*&^%$#@~_–—]/);
        const tokLen = nextBreak < 0 ? rest.length : nextBreak;
        if (tokLen > 0) result.push(rest.slice(0, tokLen));
        i += Math.max(tokLen, 1);
    }
    return result;
}

// ═══════════════════════════════════════════════════════════════════════
// Normalisation
// ═══════════════════════════════════════════════════════════════════════

export function lower(v: string): string {
    return (v ?? "").toLowerCase();
}

export function normalize(v: string): string {
    if (!v) return "";
    let s = v.toLowerCase();
    s = s.replace(
        /\s*(ltd|limited|llc|corp|corporation|s\.?r\.?l\.?|s\.?r\.?o\.?|fzco|llp|ag|n\.?v\.?|a\.?s\.?|b\.?v\.?|gmbh|inc\.?)\s*\.?\s*$/iu,
        "",
    );
    s = s.replace(/[^\p{L}\p{N}\s']/gu, " ");
    s = s.replace(/\s+/g, " ").trim();
    return s;
}

export function strip(v: string, tokens: (string | RegExp)[]): string {
    if (!v) return "";
    let s = v;
    for (const t of tokens) {
        if (t instanceof RegExp) s = s.replace(t, " ");
        else if (t) s = s.replace(new RegExp(escapeRegex(t), "gi"), " ");
    }
    return s.replace(/\s+/g, " ").trim();
}

export function stripParens(v: string): string {
    if (!v) return "";
    return v
        .replace(/\s*\([^)]*\)\s*/g, " ")
        .replace(/\s+/g, " ")
        .trim();
}

export function stripWords(v: string, dictName: string, bindings: HelperBindings): string {
    if (!v) return "";
    const entries = bindings._dictCol0Sorted[dictName] ?? [];
    let s = v;
    for (const e of entries) {
        if (!e) continue;
        const re = new RegExp("\\b" + escapeRegex(e) + "\\b", "gi");
        s = s.replace(re, " ");
    }
    return s.replace(/\s+/g, " ").trim();
}

export function replace(v: string, from: string | RegExp, to: string): string {
    if (!v) return "";
    if (from instanceof RegExp) return v.replace(from, to);
    return v.split(from).join(to);
}

// ═══════════════════════════════════════════════════════════════════════
// Matching
// ═══════════════════════════════════════════════════════════════════════

export interface IncludeResult {
    found: boolean;
    items: number[];
    matched: string[];
}

export function include(v: string, list: string[], method = "strict"): IncludeResult {
    if (!v || !list || list.length === 0) return { found: false, items: [], matched: [] };
    const vl = v.toLowerCase();
    const items: number[] = [];
    const matched: string[] = [];
    if (method === "strict") {
        for (let i = 0; i < list.length; i++) {
            if (list[i]?.toLowerCase() === vl) {
                items.push(i);
                matched.push(list[i]!);
            }
        }
    } else if (method === "substring") {
        for (let i = 0; i < list.length; i++) {
            const el = list[i]?.toLowerCase() ?? "";
            if (el && (vl.includes(el) || el.includes(vl))) {
                items.push(i);
                matched.push(list[i]!);
            }
        }
    } else if (method.startsWith("fuzzy")) {
        const n = method.split(":")[1] ? parseInt(method.split(":")[1]!, 10) : 2;
        for (let i = 0; i < list.length; i++) {
            if (editDistance(vl, list[i]?.toLowerCase() ?? "") <= n) {
                items.push(i);
                matched.push(list[i]!);
            }
        }
    }
    return { found: items.length > 0, items, matched };
}

export function match(v: string, patterns: (string | RegExp)[], fuzzy = 0): string | null {
    if (!v) return null;
    for (const p of patterns) {
        if (p instanceof RegExp) {
            if (p.test(v)) return v.match(p)?.[0] ?? null;
        } else {
            if (fuzzy === 0) {
                if (v.toLowerCase().includes(p.toLowerCase())) return p;
            } else {
                if (editDistance(v.toLowerCase(), p.toLowerCase()) <= fuzzy) return p;
            }
        }
    }
    return null;
}

export function contains(v: string, words: string[]): boolean {
    if (!v || !words) return false;
    const lv = v.toLowerCase();
    return words.some((w) => w && lv.includes(w.toLowerCase()));
}

export function hasPrefix(v: string, list: string[]): boolean {
    if (!v || !list) return false;
    const lv = v.toLowerCase();
    return list.some((p) => p && lv.startsWith(p.toLowerCase()));
}

export function hasSuffix(v: string, list: string[]): boolean {
    if (!v || !list) return false;
    const lv = v.toLowerCase();
    return list.some((s) => s && lv.endsWith(s.toLowerCase()));
}

export function closest(v: string, list: string[], maxDist = 2): string | null {
    if (!v || !list || list.length === 0) return null;
    const lv = v.toLowerCase();
    let best: string | null = null;
    let bestD = maxDist + 1;
    for (const el of list) {
        if (!el) continue;
        const d = editDistance(lv, el.toLowerCase());
        if (d <= maxDist && d < bestD) {
            bestD = d;
            best = el;
            if (d === 0) return best;
        }
    }
    return best;
}

export function editDistance(a: string, b: string): number {
    if (a === b) return 0;
    if (!a) return b.length;
    if (!b) return a.length;
    if (Math.abs(a.length - b.length) > 32 && Math.max(a.length, b.length) > 128) {
        return Math.max(a.length, b.length);
    }
    const m = a.length,
        n = b.length;
    let prev = new Array(n + 1).fill(0);
    let curr = new Array(n + 1).fill(0);
    for (let j = 0; j <= n; j++) prev[j] = j;
    for (let i = 1; i <= m; i++) {
        curr[0] = i;
        for (let j = 1; j <= n; j++) {
            const cost = a[i - 1] === b[j - 1] ? 0 : 1;
            curr[j] = Math.min(curr[j - 1] + 1, prev[j] + 1, prev[j - 1] + cost);
        }
        [prev, curr] = [curr, prev];
    }
    return prev[n]!;
}

// ═══════════════════════════════════════════════════════════════════════
// Numeric
// ═══════════════════════════════════════════════════════════════════════

export function inRange(v: unknown, from: number, to: number): boolean {
    if (typeof v !== "number" || !Number.isFinite(v)) return false;
    return v >= from && v <= to;
}

export function parseNumber(v: unknown): number | null {
    if (typeof v === "number") return Number.isFinite(v) ? v : null;
    if (typeof v !== "string") return null;
    const cleaned = v.replace(/[, ]/g, "");
    const n = Number(cleaned);
    return Number.isFinite(n) ? n : null;
}

// ═══════════════════════════════════════════════════════════════════════
// Predicates / structural
// ═══════════════════════════════════════════════════════════════════════

export function empty(v: unknown): boolean {
    if (v == null) return true;
    if (typeof v === "string") return v.length === 0;
    if (Array.isArray(v)) return v.length === 0;
    if (typeof v === "object") return Object.keys(v as object).length === 0;
    return false;
}

export function has(obj: unknown, path: string): boolean {
    return get(obj, path) !== undefined;
}

export function uniq<T>(list: T[]): T[] {
    return Array.from(new Set(list));
}

export function count<T>(list: T[], pred?: (v: T) => boolean): number {
    if (!pred) return list.length;
    return list.filter(pred).length;
}

// ═══════════════════════════════════════════════════════════════════════
// Result shorthand
// ═══════════════════════════════════════════════════════════════════════

export interface ClassResult {
    class: string;
    confidence?: number;
    data?: unknown;
    reason?: string;
    category?: "FOUND" | "NEW";
}

export interface NewResult {
    new: string;
    reason?: string;
    data?: unknown;
}
export interface UnkResult {
    unknown: true;
    reason?: string;
    data?: unknown;
}

export type RuleResult = ClassResult | NewResult | UnkResult | string | null | undefined | false;

export function CLASS(name: string, confOrData?: number | object, maybeData?: object): ClassResult {
    if (typeof confOrData === "number") {
        return { class: name, confidence: confOrData, data: maybeData };
    }
    return { class: name, data: confOrData };
}

export function NEW(token: string, reason?: string, data?: unknown): NewResult {
    return { new: token, reason, data };
}

export function UNKNOWN(reason?: string, data?: unknown): UnkResult {
    return { unknown: true, reason, data };
}

// ═══════════════════════════════════════════════════════════════════════
// Misc
// ═══════════════════════════════════════════════════════════════════════

export function hash(s: string): string {
    return hashString(s);
}

export function now(): number {
    return Date.now();
}

function escapeRegex(s: string): string {
    return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
