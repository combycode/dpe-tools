/**
 * dpe-tool-llm entry point.
 *
 * Per envelope:
 *   1. Resolve input from settings.input_path.
 *   2. Compose system prompt: base + overlays (settings.overlays_from, defaults).
 *   3. Call provider.messageSingle().
 *   4. Strip fences; emit `text` or `json` output.
 */

import { dirname, basename } from "node:path";

import { run, type Context, type JSONValue } from "@combycode/dpe-framework-ts";

import type { ModelCfg, ProviderClient, Settings } from "./types";
import { resolveOverlayValues, resolveInputPath, stringifyInput, loadPrompt } from "./prompt-loader";
import { AnthropicProvider } from "./providers/anthropic";
import { GoogleProvider } from "./providers/google";

/**
 * Default namespace under $DPE_STORAGE for cached LLM responses. Used
 * when settings.cache_dir is unset (the default — shared cross-batch
 * cache). Different stage configs hashing to the same key share entries.
 */
export const CACHE_NAMESPACE = "llm";

/**
 * Cache the produce() result honouring an optional `settings.cache_dir`
 * override. When unset, delegates straight to `ctx.cached(CACHE_NAMESPACE, …)`
 * which writes to `$DPE_STORAGE/llm/<hash>.json`. When set, temporarily
 * points `DPE_STORAGE` at the parent of `cache_dir` and uses the basename
 * as the namespace — net effect: cache file lands at `<cache_dir>/<hash>.json`
 * using the same hash + same JSON-payload format as the default path.
 *
 * Exported for tests; callers in this tool should use it instead of
 * calling ctx.cached() directly.
 */
export async function llmCached<T extends JSONValue>(
    ctx: Context,
    cacheDir: string | undefined,
    key: unknown,
    produce: () => Promise<T>,
): Promise<T> {
    if (!cacheDir || cacheDir.length === 0) {
        return ctx.cached(CACHE_NAMESPACE, key, produce) as Promise<T>;
    }
    const parent = dirname(cacheDir);
    const ns = basename(cacheDir);
    const prev = process.env.DPE_STORAGE;
    process.env.DPE_STORAGE = parent;
    try {
        return await (ctx.cached(ns, key, produce) as Promise<T>);
    } finally {
        if (prev === undefined) delete process.env.DPE_STORAGE;
        else process.env.DPE_STORAGE = prev;
    }
}

// ─── public entry used both by framework run() and by unit tests ─────

export interface HandlerDeps {
    provider: ProviderClient;
    settings: Settings;
}

export function buildProvider(settings: Settings): ProviderClient {
    if (settings.provider === "anthropic") {
        const key = process.env.ANTHROPIC_API_KEY;
        if (!key) throw new Error("ANTHROPIC_API_KEY not set");
        return new AnthropicProvider({ apiKey: key, retry: settings.retry });
    }
    if (settings.provider === "google") {
        const key = process.env.GEMINI_API_KEY ?? process.env.GOOGLE_API_KEY;
        if (!key) throw new Error("GEMINI_API_KEY (or GOOGLE_API_KEY) not set");
        return new GoogleProvider({ apiKey: key, retry: settings.retry });
    }
    throw new Error(`unknown provider: ${(settings as any).provider}`);
}

function modelCfg(settings: Settings): ModelCfg {
    return {
        model: settings.model,
        temperature: settings.temperature ?? 0,
        max_tokens: settings.max_tokens ?? 64000,
        thinking_budget: settings.thinking_budget ?? null,
    };
}

/** Remove ```markdown ... ``` or ```json ... ``` fences if present. */
export function stripFences(text: string): string {
    let s = text.trim();
    const fenceOpen = /^```[a-zA-Z]*\s*\n/;
    const fenceClose = /\n```\s*$/;
    if (fenceOpen.test(s)) s = s.replace(fenceOpen, "");
    if (fenceClose.test(s)) s = s.replace(fenceClose, "");
    return s.trim();
}

/**
 * Build the system prompt for this envelope. Exported so tests can drive it
 * without real fetch.
 */
export function buildSystemPrompt(envelopeV: any, settings: Settings): string {
    const overlays = resolveOverlayValues(settings.prompts.overlays, envelopeV, settings.overlay_defaults);
    return loadPrompt({
        dir: settings.prompts.dir,
        name: settings.prompts.name,
        version: settings.prompts.version,
        overlays,
    });
}

/**
 * Cache key for one provider call. Hashed by the framework's
 * canonical-JSON hasher; any field change flips the on-disk hash and
 * forces a fresh produce. Pre-hash the large strings (system + user)
 * so the in-memory key payload stays small even for big prompts.
 *
 * Exported so tests can pin its composition without invoking the
 * provider.
 */
export function providerCacheKey(
    system: string,
    userText: string,
    settings: Settings,
    hashFn: (s: string) => string,
): JSONValue {
    return {
        system_hash: hashFn(system),
        user_hash: hashFn(userText),
        model: settings.model,
        provider: settings.provider,
        temperature: settings.temperature ?? 0,
        max_tokens: settings.max_tokens ?? 64000,
        thinking_budget: settings.thinking_budget ?? null,
    };
}

/**
 * Heuristic classification of a failure message into a canonical class
 * for routing. Mirrors doc-converter's classify_error — substring match
 * against the four retry-trigger classes; fallback otherwise.
 */
export function classifyError(msg: string, fallback: string): string {
    const m = msg.toLowerCase();
    if (m.includes(" 429 ") || m.includes("rate limit") || m.includes("rate-limit") || m.includes("rate_limit")) {
        return "rate_limit";
    }
    if (m.includes(" 500") || m.includes(" 502") || m.includes(" 503") || m.includes(" 504") || m.includes(" 5xx")) {
        return "server_error";
    }
    if (m.includes("timeout") || m.includes("timed out")) {
        return "timeout";
    }
    if (
        m.includes("connection") ||
        m.includes("dns") ||
        m.includes(" tls ") ||
        m.includes("connect refused") ||
        m.includes("tcp reset")
    ) {
        return "connection_error";
    }
    return fallback;
}

/**
 * Surface a terminal failure per `settings.on_failure`. Always called
 * instead of bare `ctx.error(v, msg)` for any path that drops the
 * envelope from the data stream.
 */
export function emitFailure(
    ctx: Context,
    v: JSONValue,
    vObj: Record<string, JSONValue>,
    fallbackClass: string,
    errorMsg: string,
    onFailure: "stderr" | "envelope" | "both",
    prevAttempt: number,
): void {
    const errorClass = classifyError(errorMsg, fallbackClass);
    const emitEnv = onFailure === "envelope" || onFailure === "both";
    const logErr = onFailure === "stderr" || onFailure === "both";

    if (emitEnv) {
        const out: Record<string, JSONValue> = {
            failed: true,
            error_class: errorClass,
            error_msg: errorMsg,
            error_attempt: prevAttempt + 1,
        };
        // Source-identity preservation — same field set as success path.
        for (const k of ["source_path", "source_hash", "source_filename", "source_ext", "source_size", "source_file"]) {
            if (vObj[k] !== undefined && out[k] === undefined) {
                out[k] = vObj[k];
            }
        }
        // For raw scan-fs inputs that haven't been promoted, also map
        // path/hash/size/filename/ext → source_*.
        const rawMap: Record<string, string> = {
            path: "source_path",
            hash: "source_hash",
            size: "source_size",
            filename: "source_filename",
            ext: "source_ext",
        };
        for (const [inK, outK] of Object.entries(rawMap)) {
            if (vObj[inK] !== undefined && out[outK] === undefined) {
                out[outK] = vObj[inK];
            }
        }
        ctx.output(out);
    }

    if (logErr) {
        ctx.error(v, errorMsg);
    }
}

/** Process one envelope using the given provider. Public for unit tests. */
export async function processEnvelope(v: JSONValue, ctx: Context, deps: HandlerDeps): Promise<void> {
    const { provider, settings } = deps;

    const vObj = v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, JSONValue>) : {};

    // Two-layer retry: read `v.error_attempt` (set by the prior failure
    // emission) so the next emission can increment it. Pipeline gates
    // retries by attempt count (e.g. `route: failed = "v.failed &&
    // v.error_attempt < 5"`).
    const prevAttempt =
        typeof vObj.error_attempt === "number" && vObj.error_attempt > 0 ? (vObj.error_attempt as number) : 0;
    const onFailure: "stderr" | "envelope" | "both" = settings.on_failure ?? "stderr";

    const input = resolveInputPath(vObj, settings.input_path);
    const userText = stringifyInput(input);
    if (!userText) {
        emitFailure(
            ctx,
            v,
            vObj,
            "input",
            `input_path ${settings.input_path} resolved to empty`,
            onFailure,
            prevAttempt,
        );
        return;
    }

    let system: string;
    try {
        system = buildSystemPrompt(vObj, settings);
    } catch (e) {
        emitFailure(ctx, v, vObj, "input", `prompt compose failed: ${(e as Error).message}`, onFailure, prevAttempt);
        return;
    }

    let responseText: string;
    try {
        const cacheKey = providerCacheKey(system, userText, settings, (s) => ctx.hash(s));
        responseText = await llmCached(ctx, settings.cache_dir, cacheKey, () =>
            provider.messageSingle(system, userText, modelCfg(settings)),
        );
    } catch (e) {
        emitFailure(ctx, v, vObj, "convert", `provider call failed: ${(e as Error).message}`, onFailure, prevAttempt);
        return;
    }

    const clean = stripFences(responseText);

    // Closing-the-loop log: a successful run after one or more external
    // retries deserves a journal entry so operators can see the loop
    // resolved. The output envelope itself is built fresh below and never
    // copies failure markers — `route: failed == true` no longer matches.
    if (prevAttempt > 0) {
        ctx.log(`llm: succeeded after ${prevAttempt} external retr${prevAttempt === 1 ? "y" : "ies"}`, {
            level: "info",
        });
    }

    if (settings.output === "text") {
        // Preserve other envelope fields except the one we consumed AND
        // any failure markers from a retry input. Success closes the loop:
        // downstream `route: failed == true` no longer matches.
        const consumed = settings.input_path.replace(/^v\./, "").split(".")[0];
        const skip = new Set([consumed, "failed", "error_class", "error_msg", "error_attempt"]);
        const out: Record<string, JSONValue> = { response: clean };
        for (const [k, val] of Object.entries(vObj)) {
            if (!skip.has(k)) out[k] = val;
        }
        // When the operator opted into envelope-style failure surfacing,
        // every output envelope (success AND failure) carries `failed:
        // bool` so downstream `route: failed == true` evaluates without
        // throwing on missing-field access. Mirrors doc-converter's
        // contract.
        if (onFailure === "envelope" || onFailure === "both") {
            out.failed = false;
        }
        ctx.output(out);
        return;
    }

    // output: "json"
    let parsed: JSONValue;
    try {
        parsed = JSON.parse(clean);
    } catch (e) {
        emitFailure(
            ctx,
            v,
            vObj,
            "parse",
            `json parse failed: ${(e as Error).message}; response head: ${clean.slice(0, 200)}`,
            onFailure,
            prevAttempt,
        );
        return;
    }
    // Layer source-identity fields from input onto the parsed JSON when
    // the LLM didn't produce them. Additive only — LLM-produced keys
    // always win. Lets downstream stages trace an LLM output back to its
    // source file without inferring from filenames.
    //
    // NB: failure markers (failed, error_class, error_msg, error_attempt)
    // from a retry-input envelope are NEVER carried into the JSON output —
    // the LLM's parsed JSON IS the new envelope shape, so the loop closes.
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        const out = parsed as Record<string, JSONValue>;
        for (const k of ["source_path", "source_hash", "source_filename", "source_ext", "source_size", "source_file"]) {
            if (vObj[k] !== undefined && out[k] === undefined) {
                out[k] = vObj[k];
            }
        }
        // Symmetric with text mode: stamp `failed: false` on success
        // envelopes when the operator opted into envelope-style failure
        // surfacing, so downstream `route: failed == true` doesn't throw
        // on missing-field access. Only applies when parsed is an object —
        // scalar / array JSON outputs can't carry a status field.
        if (onFailure === "envelope" || onFailure === "both") {
            if (out.failed === undefined) out.failed = false;
        }
    }
    ctx.output(parsed);
}

// ─── framework bootstrap ────────────────────────────────────────────

// Settings are parsed from argv[1] inside the framework and exposed via run().
// We read them off argv here to build the provider + bind deps.

async function main(): Promise<void> {
    const arg = process.argv[2] ?? "{}";
    let settings: Settings;
    try {
        settings = JSON.parse(arg) as Settings;
    } catch (e) {
        process.stderr.write(`{"type":"error","error":"bad settings: ${(e as Error).message}"}\n`);
        process.exit(2);
    }

    const provider = buildProvider(settings);
    const deps: HandlerDeps = { provider, settings };

    // Await the framework's run() Promise. Without `await`, Bun (on
    // Windows in particular) exits before the stdin async-iter pulls
    // any chunks — the Promise is GC'd unfulfilled, the tool exits 0
    // with zero envelopes processed, and `dpe test` reports a vacuous
    // PASS because expected/actual are both empty.
    await run({
        input: async (v, _settings, ctx) => {
            await processEnvelope(v, ctx, deps);
        },
    });
}

// Only run when invoked directly (not when imported by tests).
if (import.meta.main) {
    main().catch((e) => {
        // Unhandled rejection -> route through the framework's stderr
        // error channel instead of vanishing silently.
        process.stderr.write(JSON.stringify({ type: "error", error: String(e) }) + "\n");
        process.exit(1);
    });
}
