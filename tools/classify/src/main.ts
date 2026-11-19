/**
 * dpe-tool-classify — rule-based universal classifier.
 *
 * Settings (argv[1]):
 *   {
 *     "classes":  "classes.tsv",         // optional; exposed as `classes` binding + dicts.classes
 *     "dicts":    { "aliases":"a.tsv", "mids":"m.tsv", "stopWords":"sw.txt" },
 *     "rules":    ["rules/a.js", {"name":"inline","confidence":90,"expr":"..."}],
 *     "source":   "row.path",            // dot-path into v — input to rules
 *     "target":   "class"                // dot-path on v — where to write result
 *   }
 *
 * Per envelope:
 *   - read item = get(v, source) — or v itself if source missing
 *   - run rules; obtain ClassificationResult
 *   - write result at v[target]
 *   - emit envelope
 */

import { parseEnvelope, writeData, writeMeta, writeLog, writeError } from "@combycode/dpe-framework-ts";
import type { JSONValue, DataEnvelope } from "@combycode/dpe-framework-ts";
import { readFileSync } from "node:fs";
import { resolve, dirname, extname } from "node:path";
import { loadDict, type Dict } from "./dict";
import { makeBindings, get as hGet } from "./helpers";
import {
    compileRule,
    buildBindings,
    runRules,
    loadRulesFile,
    loadRulesJson,
    type CompiledRule,
    type RuleSpec,
    type ClassificationResult,
} from "./rules";

interface ResolvedConfig {
    classes: string[];
    dicts: Record<string, Dict>;
    rules: CompiledRule[];
    source?: string;
    target: string;
}

let cfg: ResolvedConfig | null = null;
let stats = { in: 0, emitted: 0, found: 0, new: 0, unknown: 0 };

async function loadConfig(settings: JSONValue, settingsPath?: string): Promise<ResolvedConfig> {
    const s =
        settings && typeof settings === "object" && !Array.isArray(settings)
            ? (settings as Record<string, JSONValue>)
            : {};
    const baseDir = settingsPath ? dirname(settingsPath) : process.cwd();
    const rel = (p: string) => resolve(baseDir, p);

    // Load classes
    let classes: string[] = [];
    const dicts: Record<string, Dict> = {};
    if (typeof s.classes === "string") {
        const d = loadDict(rel(s.classes));
        classes = extractPrimaryKeys(d);
        dicts.classes = d;
    }

    // Load other dicts
    const dictsSetting = s.dicts;
    if (dictsSetting && typeof dictsSetting === "object" && !Array.isArray(dictsSetting)) {
        for (const [name, entry] of Object.entries(dictsSetting)) {
            if (typeof entry === "string") {
                dicts[name] = loadDict(rel(entry));
            } else if (entry && typeof entry === "object" && !Array.isArray(entry)) {
                const rec = entry as Record<string, JSONValue>;
                const path = typeof rec.path === "string" ? rel(rec.path) : null;
                const key = typeof rec.key === "string" ? rec.key : undefined;
                if (path) dicts[name] = loadDict(path, { key });
            }
        }
    }

    // Load rules (mixed: file paths + inline)
    const rulesSetting = Array.isArray(s.rules) ? s.rules : [];
    const allSpecs: RuleSpec[] = [];
    for (const r of rulesSetting) {
        if (typeof r === "string") {
            const abs = rel(r);
            const ext = extname(abs).toLowerCase();
            let specs: RuleSpec[];
            if (ext === ".json") specs = loadRulesJson(abs);
            else specs = await loadRulesFile(abs);
            allSpecs.push(...specs);
        } else if (r && typeof r === "object") {
            allSpecs.push(r as unknown as RuleSpec);
        }
    }
    const rules = allSpecs.map(compileRule);

    return {
        classes,
        dicts,
        rules,
        source: typeof s.source === "string" ? s.source : undefined,
        target: typeof s.target === "string" ? s.target : "class",
    };
}

function extractPrimaryKeys(d: Dict): string[] {
    if (d.kind === "flat") return d.rows.map((r) => r[0] ?? "").filter((s) => s.length > 0);
    return d.keys;
}

function setAtPath(obj: Record<string, JSONValue>, path: string, value: JSONValue): void {
    const parts = path.split(".");
    let cur: Record<string, JSONValue> = obj;
    for (let i = 0; i < parts.length - 1; i++) {
        const k = parts[i]!;
        const next = cur[k];
        if (!next || typeof next !== "object" || Array.isArray(next)) {
            cur[k] = {};
        }
        cur = cur[k] as Record<string, JSONValue>;
    }
    cur[parts[parts.length - 1]!] = value;
}

function handleEnvelope(env: DataEnvelope): void {
    if (!cfg) return;
    stats.in++;

    const v = env.v;
    const item = cfg.source ? hGet(v, cfg.source) : v;

    const helpers = makeBindings(cfg.classes, cfg.dicts);
    const bindings = buildBindings(item, env, helpers);

    let result: ClassificationResult;
    try {
        result = runRules(cfg.rules, bindings, helpers);
    } catch (e) {
        writeError(v, e, env.id ?? "", env.src ?? "");
        return;
    }

    // Build new v with classification attached
    const classOutput: JSONValue = {
        className: result.className,
        category: result.category,
        confidence: result.confidence,
        reason: result.reason,
        data: (result.data ?? null) as JSONValue,
    };

    let outV: JSONValue;
    if (v && typeof v === "object" && !Array.isArray(v)) {
        const copy: Record<string, JSONValue> = { ...(v as Record<string, JSONValue>) };
        setAtPath(copy, cfg.target, classOutput);
        outV = copy;
    } else {
        outV = { value: v, [cfg.target]: classOutput } as unknown as JSONValue;
    }

    writeData(outV, env.id ?? "", env.src ?? "");
    stats.emitted++;
    if (result.category === "FOUND") stats.found++;
    else if (result.category === "NEW") stats.new++;
    else stats.unknown++;
}

function onShutdown(): void {
    if (stats.in > 0) {
        writeMeta({ tool: "classify", ...stats } as unknown as JSONValue);
    }
}

async function* readStdinLines(): AsyncGenerator<string> {
    let buffer = "";
    const decoder = new TextDecoder("utf-8");
    for await (const chunk of process.stdin as AsyncIterable<Buffer>) {
        buffer += decoder.decode(chunk, { stream: true });
        for (let idx = buffer.indexOf("\n"); idx >= 0; idx = buffer.indexOf("\n")) {
            let line = buffer.slice(0, idx);
            buffer = buffer.slice(idx + 1);
            if (line.endsWith("\r")) line = line.slice(0, -1);
            yield line;
        }
    }
    buffer += decoder.decode();
    if (buffer.length > 0) yield buffer;
}

async function main(): Promise<void> {
    // Parse settings
    let settings: JSONValue = {};
    let settingsArg = process.argv[2];
    let settingsPath: string | undefined;

    if (settingsArg) {
        // argv[1] is either raw JSON or @path/to/file.json
        if (settingsArg.startsWith("@")) {
            settingsPath = settingsArg.slice(1);
            try {
                settings = JSON.parse(readFileSync(settingsPath, "utf-8")) as JSONValue;
            } catch (e) {
                writeLog(`Failed to load settings from ${settingsPath}: ${(e as Error).message}`, "error");
                process.exit(1);
            }
        } else {
            try {
                settings = JSON.parse(settingsArg) as JSONValue;
            } catch {
                writeLog("settings: argv[1] is not valid JSON, using {}", "warn");
            }
        }
    }

    try {
        cfg = await loadConfig(settings, settingsPath);
    } catch (e) {
        writeLog(`config load failed: ${(e as Error).message}`, "error");
        process.exit(1);
    }

    process.on("SIGTERM", onShutdown);
    process.on("SIGINT", onShutdown);

    for await (const line of readStdinLines()) {
        const env = parseEnvelope(line);
        if (!env || env.t !== "d") continue;
        handleEnvelope(env as DataEnvelope);
    }

    onShutdown();
}

await main();
