/**
 * dpe-tool-filter — filter / map NDJSON stream via JS expressions.
 *
 * Settings (argv[1] JSON):
 *   {
 *     "ops": [
 *       {"filter": "<expr>"} | {"map": "<expr>"}
 *     ],
 *     "target":   "row",                  // which field of v to operate on (default "row")
 *     "apply_to": "data" | "meta" | "both"  // which envelope types reach ops (default "data")
 *   }
 *
 * Per envelope:
 *   - if apply_to excludes this envelope's type → drop (do NOT emit)
 *   - pick item = v[target] ?? v
 *   - run ops top-to-bottom:
 *     - filter: if falsy → drop envelope
 *     - map:    if returns null/undefined → drop; else replaces item
 *   - survivors are emitted with item written back to v[target] (or v if no target)
 *
 * Expression form (filter OR map):
 *   - default: wrapped as  function(item, env, hash, now) { return (EXPR); }
 *   - if EXPR text contains a `return` keyword, it's used as the function body verbatim
 *
 * Exposed bindings in the expression:
 *   - item   — the target of operations
 *   - env    — the full envelope (escape hatch: env.t, env.id, env.src, env.v)
 *   - hash(str)  — blake2b64 hex (matches framework)
 *   - now()  — Date.now() milliseconds
 *   - plus all standard JS globals (JSON, Math, Date, Object, Array, String, Number, RegExp, parseInt, parseFloat)
 */

import {
    run,
    hashString,
    parseEnvelope,
    writeData,
    writeMeta,
    writeError,
    writeLog,
} from "@combycode/dpe-framework-ts";
import type { JSONValue, Envelope, DataEnvelope } from "@combycode/dpe-framework-ts";

type ApplyTo = "data" | "meta" | "both";
type OpKind = "filter" | "map";
type Op = { kind: OpKind; expr: string; fn: CompiledFn };
type CompiledFn = (item: unknown, env: unknown) => unknown;

interface ResolvedSettings {
    ops: Op[];
    target: string;
    applyTo: ApplyTo;
}

let resolved: ResolvedSettings | null = null;
let stats = { data_in: 0, meta_in: 0, dropped_filter: 0, dropped_map: 0, emitted: 0 };

function resolveSettings(raw: JSONValue): ResolvedSettings {
    const s = raw && typeof raw === "object" && !Array.isArray(raw) ? (raw as Record<string, JSONValue>) : {};

    const opsRaw = Array.isArray(s.ops) ? s.ops : [];
    const ops: Op[] = opsRaw.map((entry, idx) => {
        if (!entry || typeof entry !== "object" || Array.isArray(entry)) {
            throw new Error(`ops[${idx}] must be an object like {filter:"..."} or {map:"..."}`);
        }
        const e = entry as Record<string, JSONValue>;
        const keys = Object.keys(e);
        if (keys.length !== 1 || (keys[0] !== "filter" && keys[0] !== "map")) {
            throw new Error(`ops[${idx}] must have exactly one of {filter} or {map}`);
        }
        const kind = keys[0] as OpKind;
        const expr = e[kind];
        if (typeof expr !== "string") {
            throw new Error(`ops[${idx}].${kind} must be a string expression`);
        }
        const fn = compileExpression(expr);
        return { kind, expr, fn };
    });

    const target = typeof s.target === "string" && s.target.length > 0 ? s.target : "row";
    const applyToRaw = typeof s.apply_to === "string" ? s.apply_to : "data";
    if (applyToRaw !== "data" && applyToRaw !== "meta" && applyToRaw !== "both") {
        throw new Error(`apply_to must be "data" | "meta" | "both" (got "${applyToRaw}")`);
    }

    return { ops, target, applyTo: applyToRaw };
}

/** Compile user expression to a reusable Function. */
function compileExpression(expr: string): CompiledFn {
    const body = /\breturn\b/.test(expr) ? expr : `return (${expr});`;
    // Exposed identifiers: item, env, plus helpers. Standard JS globals are
    // automatically available (no need to pass them).
    // eslint-disable-next-line no-new-func
    const compiled = new Function("item", "env", "hash", "now", body) as (
        item: unknown,
        env: unknown,
        hash: (s: string) => string,
        now: () => number,
    ) => unknown;

    return (item, env) => compiled(item, env, hashString, Date.now);
}

function getTarget(v: JSONValue, target: string): unknown {
    if (v && typeof v === "object" && !Array.isArray(v) && target in (v as object)) {
        return (v as Record<string, JSONValue>)[target];
    }
    return v;
}

function setTarget(v: JSONValue, target: string, item: unknown): JSONValue {
    if (v && typeof v === "object" && !Array.isArray(v) && target in (v as object)) {
        return { ...(v as Record<string, JSONValue>), [target]: item as JSONValue };
    }
    return item as JSONValue;
}

/**
 * Apply ops in order. Returns:
 *   { kept: true, item }  — passes; emit (v[target] replaced by item)
 *   { kept: false, reason }   — dropped
 */
function applyOps(
    item: unknown,
    env: unknown,
    ops: Op[],
): {
    kept: boolean;
    item?: unknown;
    reason?: "filter" | "map";
} {
    for (const op of ops) {
        const result = op.fn(item, env);
        if (op.kind === "filter") {
            if (!result) return { kept: false, reason: "filter" };
        } else {
            // map
            if (result === null || result === undefined) {
                return { kept: false, reason: "map" };
            }
            item = result;
        }
    }
    return { kept: true, item };
}

// ─── custom runtime ─────────────────────────────────────────────────
// We DON'T use framework's run() directly because that skips meta envelopes.
// Filter needs to see meta too (subject to apply_to).

async function customRun(): Promise<void> {
    const rawSettings = parseArgvSettings();
    try {
        resolved = resolveSettings(rawSettings);
    } catch (e) {
        writeLog(`settings error: ${(e as Error).message}`, "error");
        process.exit(1);
    }

    process.on("SIGTERM", onShutdown);
    process.on("SIGINT", onShutdown);

    for await (const line of readStdinLines()) {
        const env = parseEnvelope(line);
        if (!env) continue;
        handleEnvelope(env);
    }

    onShutdown();
}

function handleEnvelope(env: Envelope): void {
    const cfg = resolved!;
    const envType = env.t;

    // Apply_to gate — envelopes of wrong type are DROPPED (not passed through)
    const shouldProcess =
        cfg.applyTo === "both" ||
        (cfg.applyTo === "data" && envType === "d") ||
        (cfg.applyTo === "meta" && envType === "m");

    if (!shouldProcess) return; // strict drop

    if (envType === "d") stats.data_in++;
    else stats.meta_in++;

    // Pick target. For meta, target lookup still goes via v[target].
    const v: JSONValue = env.v;
    const item = getTarget(v, cfg.target);

    let result: ReturnType<typeof applyOps>;
    try {
        result = applyOps(item, env, cfg.ops);
    } catch (e) {
        const id = (env as DataEnvelope).id ?? "";
        const src = (env as DataEnvelope).src ?? "";
        writeError(v, e, id, src);
        return;
    }

    if (!result.kept) {
        if (result.reason === "filter") stats.dropped_filter++;
        else stats.dropped_map++;
        return;
    }

    const newV = setTarget(v, cfg.target, result.item);

    if (envType === "d") {
        const d = env as DataEnvelope;
        writeData(newV, d.id ?? "", d.src ?? "");
    } else {
        writeMeta(newV);
    }
    stats.emitted++;
}

function onShutdown(): void {
    // Emit final meta summarising drops/passes (only if we processed data).
    if (stats.data_in + stats.meta_in > 0) {
        writeMeta({
            tool: "filter",
            ...stats,
        } as unknown as JSONValue);
    }
}

function parseArgvSettings(): JSONValue {
    if (process.argv.length < 3) return {};
    try {
        return JSON.parse(process.argv[2]!) as JSONValue;
    } catch {
        writeLog("settings: argv[1] is not valid JSON, using {}", "warn");
        return {};
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

// keep `run` import so removing it later is intentional (framework convention hook);
// unused for now because filter needs raw envelope access (meta pass-through).
void run;

await customRun();
