/**
 * Rule compilation and execution.
 *
 * Output ClassificationResult has SEPARATE category field:
 *   className:  string | null          // literal token; null only when UNKNOWN
 *   category:   FOUND | NEW | UNKNOWN  // derived from rule return + classes dict membership
 *   confidence: number 0-100
 *   reason:     rule name
 *   data:       optional payload
 *
 * Category auto-derivation:
 *   - rule returns CLASS(x) AND x ∈ classes  → FOUND
 *   - rule returns CLASS(x) AND x ∉ classes  → NEW  (auto-upgrade; rules don't need to know)
 *   - rule returns NEW(x)                    → NEW  (explicit)
 *   - rule returns UNKNOWN(r)                → UNKNOWN
 *   - no rule matched                        → UNKNOWN
 */

import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import type { JSONValue } from "@combycode/dpe-framework-ts";
import type { HelperBindings, Dict, RuleResult, IncludeResult } from "./helpers";
import * as H from "./helpers";

export interface RuleSpec {
    name: string;
    confidence: number;
    expr: string;
}

export interface CompiledRule {
    name: string;
    confidence: number;
    fn: (bindings: RuleBindings) => RuleResult;
}

export interface RuleBindings {
    item: unknown;
    env: unknown;
    classes: string[];
    // string / path
    split: (v: string, seps: string) => string[];
    segment: (v: string, idx: number, sep?: string) => string;
    fileName: (p: string) => string;
    fileStem: (p: string) => string;
    fileExt: (p: string) => string;
    get: (obj: unknown, path: string) => unknown;
    words: (v: string, dictName?: string) => string[];
    // normalise
    lower: (v: string) => string;
    normalize: (v: string) => string;
    strip: (v: string, tokens: (string | RegExp)[]) => string;
    stripParens: (v: string) => string;
    stripWords: (v: string, dictName: string) => string;
    replace: (v: string, from: string | RegExp, to: string) => string;
    // match
    include: (v: string, list: string[], method?: string) => IncludeResult;
    match: (v: string, patterns: (string | RegExp)[], fuzzy?: number) => string | null;
    contains: (v: string, words: string[]) => boolean;
    hasPrefix: (v: string, list: string[]) => boolean;
    hasSuffix: (v: string, list: string[]) => boolean;
    closest: (v: string, list: string[], maxDist?: number) => string | null;
    editDistance: (a: string, b: string) => number;
    // numeric
    inRange: (v: unknown, from: number, to: number) => boolean;
    parseNumber: (v: unknown) => number | null;
    // dict access (new API)
    keys: (dictName: string) => string[];
    values: (dictName: string, ...args: unknown[]) => JSONValue[];
    entry: (dictName: string, key: string) => JSONValue | null;
    members: (dictName: string, field: number | string, value: JSONValue) => JSONValue[];
    pairs: (dictName: string, key: string, col?: number | string) => JSONValue[];
    // legacy flat-only helpers (still useful)
    alias: (dictName: string, v: string) => string | null;
    aliasReverse: (dictName: string, v: string) => string | null;
    lookup: (dictName: string, v: string, opts?: { key?: number; value?: number }) => string | null;
    dict: (dictName: string) => Dict | null;
    // predicates
    empty: (v: unknown) => boolean;
    has: (obj: unknown, path: string) => boolean;
    uniq: <T>(l: T[]) => T[];
    count: <T>(l: T[], pred?: (v: T) => boolean) => number;
    // result shorthand
    CLASS: typeof H.CLASS;
    NEW: typeof H.NEW;
    UNKNOWN: typeof H.UNKNOWN;
    // misc
    hash: (s: string) => string;
    now: () => number;
}

export function buildBindings(item: unknown, env: unknown, helpers: HelperBindings): RuleBindings {
    return {
        item,
        env,
        classes: helpers.classes,
        split: H.split,
        segment: H.segment,
        fileName: H.fileName,
        fileStem: H.fileStem,
        fileExt: H.fileExt,
        get: H.get,
        words: (v, dictName) => H.words(v, dictName, helpers),
        lower: H.lower,
        normalize: H.normalize,
        strip: H.strip,
        stripParens: H.stripParens,
        stripWords: (v, dictName) => H.stripWords(v, dictName, helpers),
        replace: H.replace,
        include: H.include,
        match: H.match,
        contains: H.contains,
        hasPrefix: H.hasPrefix,
        hasSuffix: H.hasSuffix,
        closest: H.closest,
        editDistance: H.editDistance,
        inRange: H.inRange,
        parseNumber: H.parseNumber,
        keys: (name) => H.keys(name, helpers),
        values: (name, ...args) => H.valuesImpl(name, args, helpers),
        entry: (name, key) => H.entry(name, key, helpers),
        members: (name, field, val) => H.members(name, field, val, helpers),
        pairs: (name, key, col) => H.pairs(name, key, col, helpers),
        alias: (name, v) => H.alias(name, v, helpers),
        aliasReverse: (name, v) => H.aliasReverse(name, v, helpers),
        lookup: (name, v, opts = {}) => H.lookup(name, v, opts, helpers),
        dict: (name) => H.dict(name, helpers),
        empty: H.empty,
        has: H.has,
        uniq: H.uniq,
        count: H.count,
        CLASS: H.CLASS,
        NEW: H.NEW,
        UNKNOWN: H.UNKNOWN,
        hash: H.hash,
        now: H.now,
    };
}

export function compileExpression(expr: string): (b: RuleBindings) => RuleResult {
    const body = /\breturn\b/.test(expr) ? expr : `return (${expr});`;
    const argNames = [
        "item",
        "env",
        "classes",
        "split",
        "segment",
        "fileName",
        "fileStem",
        "fileExt",
        "get",
        "words",
        "lower",
        "normalize",
        "strip",
        "stripParens",
        "stripWords",
        "replace",
        "include",
        "match",
        "contains",
        "hasPrefix",
        "hasSuffix",
        "closest",
        "editDistance",
        "inRange",
        "parseNumber",
        "keys",
        "values",
        "entry",
        "members",
        "pairs",
        "alias",
        "aliasReverse",
        "lookup",
        "dict",
        "empty",
        "has",
        "uniq",
        "count",
        "CLASS",
        "NEW",
        "UNKNOWN",
        "hash",
        "now",
    ] as const;
    // eslint-disable-next-line no-new-func
    const compiled = new Function(...argNames, body) as (...args: unknown[]) => RuleResult;
    return (b) =>
        compiled(
            b.item,
            b.env,
            b.classes,
            b.split,
            b.segment,
            b.fileName,
            b.fileStem,
            b.fileExt,
            b.get,
            b.words,
            b.lower,
            b.normalize,
            b.strip,
            b.stripParens,
            b.stripWords,
            b.replace,
            b.include,
            b.match,
            b.contains,
            b.hasPrefix,
            b.hasSuffix,
            b.closest,
            b.editDistance,
            b.inRange,
            b.parseNumber,
            b.keys,
            b.values,
            b.entry,
            b.members,
            b.pairs,
            b.alias,
            b.aliasReverse,
            b.lookup,
            b.dict,
            b.empty,
            b.has,
            b.uniq,
            b.count,
            b.CLASS,
            b.NEW,
            b.UNKNOWN,
            b.hash,
            b.now,
        );
}

export function compileRule(spec: RuleSpec): CompiledRule {
    return {
        name: spec.name,
        confidence: spec.confidence,
        fn: compileExpression(spec.expr),
    };
}

export async function loadRulesFile(path: string): Promise<RuleSpec[]> {
    // Dynamic import() on absolute filesystem paths needs a `file://`
    // URL on every runtime that targets the ESM loader (Node strict,
    // Bun in module resolution mode). pathToFileURL is portable and
    // works on Windows + Linux + macOS — apply it unconditionally for
    // absolute paths to avoid runtime-specific edge cases.
    //
    // Windows-only step: strip the `\\?\` extended-length prefix that
    // the runner's path canonicalization can produce (see runner's
    // PathResolver). pathToFileURL doesn't accept that prefix.
    let p = path;
    if (process.platform === "win32" && p.startsWith("\\\\?\\")) {
        p = p.slice(4);
    }
    const spec = pathToFileURL(p).href;
    const mod = await import(spec);
    const specs = mod.default ?? mod.rules;
    if (!Array.isArray(specs)) throw new Error(`${path}: default export must be RuleSpec[]`);
    return specs as RuleSpec[];
}

export function loadRulesJson(path: string): RuleSpec[] {
    const raw = readFileSync(path, "utf-8");
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) throw new Error(`${path}: not an array`);
    return parsed as RuleSpec[];
}

// ═══ Runner ════════════════════════════════════════════════════════════

export type Category = "FOUND" | "NEW" | "UNKNOWN";

export interface ClassificationResult {
    className: string | null;
    category: Category;
    confidence: number;
    reason: string;
    data?: unknown;
}

export function runRules(rules: CompiledRule[], bindings: RuleBindings, helpers: HelperBindings): ClassificationResult {
    let best: { result: ClassificationResult; isClass: boolean } | null = null;
    const newTokens: { token: string; reason: string; rule: string; data?: unknown }[] = [];

    const categorize = (name: string): Category => (helpers.classesSet.has(name) ? "FOUND" : "NEW");

    for (const rule of rules) {
        let result: RuleResult;
        try {
            result = rule.fn(bindings);
        } catch (e) {
            process.stderr.write(
                JSON.stringify({
                    type: "log",
                    level: "warn",
                    msg: `rule '${rule.name}' threw: ${(e as Error).message}`,
                }) + "\n",
            );
            continue;
        }

        if (result == null || result === false) continue;

        if (typeof result === "string") {
            if (result === "UNKNOWN") {
                return { className: null, category: "UNKNOWN", confidence: 0, reason: rule.name };
            }
            const cr: ClassificationResult = {
                className: result,
                category: categorize(result),
                confidence: rule.confidence,
                reason: rule.name,
            };
            if (cr.confidence === 100) return cr;
            if (!best || cr.confidence > best.result.confidence) best = { result: cr, isClass: true };
            continue;
        }

        if (typeof result === "object") {
            // Rule expressions are user code: inspect the returned object field by field.
            const r = result as unknown as Record<string, unknown>;
            if (r.unknown === true) {
                return {
                    className: null,
                    category: "UNKNOWN",
                    confidence: 0,
                    reason: rule.name,
                    data: r.data ?? (r.reason ? { reason: r.reason } : undefined),
                };
            }
            if (typeof r.new === "string") {
                newTokens.push({
                    token: r.new,
                    reason: typeof r.reason === "string" ? r.reason : "",
                    rule: rule.name,
                    data: r.data,
                });
                continue;
            }
            if (typeof r.class === "string") {
                const conf = typeof r.confidence === "number" ? r.confidence : rule.confidence;
                const explicitCategory =
                    r.category === "FOUND" || r.category === "NEW" ? (r.category as Category) : undefined;
                const cr: ClassificationResult = {
                    className: r.class,
                    category: explicitCategory ?? categorize(r.class),
                    confidence: conf,
                    reason: typeof r.reason === "string" ? r.reason : rule.name,
                    data: r.data,
                };
                if (conf === 100) return cr;
                if (!best || cr.confidence > best.result.confidence) best = { result: cr, isClass: true };
                continue;
            }
        }
    }

    if (best) return best.result;
    if (newTokens.length > 0) {
        const first = newTokens[0]!;
        return {
            className: first.token,
            category: "NEW",
            confidence: 0,
            reason: first.rule,
            data: first.data ?? (first.reason ? { reason: first.reason } : undefined),
        };
    }
    return { className: null, category: "UNKNOWN", confidence: 0, reason: "no-rule-matched" };
}
