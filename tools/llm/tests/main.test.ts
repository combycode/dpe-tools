/**
 * End-to-end processEnvelope tests — mocked provider, real prompt loader,
 * real envelope shape. Verifies:
 *   - input_path resolution
 *   - system prompt composition (base + overlays + defaults)
 *   - output mode: text (wraps in {response}) vs json (parses)
 *   - fence stripping
 *   - error reporting (via ctx.error)
 */

import { test, expect, describe, beforeEach, afterEach } from "bun:test";
import { mkdirSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

import type { ProviderClient, Settings } from "../src/types";
import { processEnvelope, stripFences, buildSystemPrompt, providerCacheKey, CACHE_NAMESPACE } from "../src/main";

class MockProvider implements ProviderClient {
    public calls: { system: string; user: string; cfg: any }[] = [];
    public responses: string[];
    public errors: Error[];
    constructor(init?: { responses?: string[]; errors?: Error[] }) {
        this.responses = init?.responses ?? [];
        this.errors = init?.errors ?? [];
    }
    async messageSingle(system: string, userText: string, cfg: any): Promise<string> {
        this.calls.push({ system, user: userText, cfg });
        if (this.errors.length > 0) {
            const e = this.errors.shift()!;
            throw e;
        }
        if (this.responses.length === 0) throw new Error("mock: no more responses configured");
        return this.responses.shift()!;
    }
}

// A minimal Context lookalike for capture.
//
// `cached` is a passthrough by default: it just runs `produce()` so
// existing processEnvelope tests don't need DPE_STORAGE wiring. Tests
// that exercise the cache hit/miss flow build a real ctx via the
// framework instead (see "cached" describe block below).
function makeCtx() {
    const outputs: any[] = [];
    const errors: { v: any; err: string }[] = [];
    return {
        outputs,
        errors,
        id: "x",
        src: "seed",
        memory: {} as any,
        trace: () => {},
        stats: () => {},
        output: (v: any) => {
            outputs.push(v);
        },
        error: (v: any, err: unknown) => {
            errors.push({ v, err: err instanceof Error ? err.message : String(err) });
        },
        meta: () => {},
        log: () => {},
        emit: () => {},
        drain: async () => {},
        hash: (s: string) => `H(${s.length})`,
        hashFile: async () => undefined,
        cached: async (_ns: string, _key: unknown, produce: () => any) => produce(),
    } as any;
}

let dir: string;
beforeEach(() => {
    dir = join(tmpdir(), `main-test-${Date.now()}-${Math.random().toString(36).slice(2)}`);
    mkdirSync(dir, { recursive: true });
});
afterEach(() => rmSync(dir, { recursive: true, force: true }));

function writePrompts(files: Record<string, string>) {
    for (const [name, body] of Object.entries(files)) {
        writeFileSync(join(dir, name), body, "utf8");
    }
}

// ─── stripFences standalone ──────────────────────────────────────────

describe("stripFences", () => {
    test("no fence → unchanged", () => {
        expect(stripFences("hello")).toBe("hello");
    });
    test("strips ```markdown", () => {
        expect(stripFences("```markdown\nhi\n```")).toBe("hi");
    });
    test("strips ```json", () => {
        expect(stripFences('```json\n{"a":1}\n```')).toBe('{"a":1}');
    });
    test("strips bare ```", () => {
        expect(stripFences("```\nhi\n```")).toBe("hi");
    });
    test("trims outer whitespace", () => {
        expect(stripFences("\n  hi \n")).toBe("hi");
    });
});

// ─── buildSystemPrompt integration ───────────────────────────────────

test("buildSystemPrompt: merges base + category + kind", () => {
    writePrompts({
        "convert_v1.txt": "BASE",
        "convert_v1_alpha.txt": "OVERLAY_A",
        "convert_v1_bravo.txt": "OVERLAY_B",
    });
    const settings: Settings = {
        provider: "anthropic",
        model: "m",
        prompts: { dir, name: "convert", version: "v1", overlays: ["category", "kind"] },
        overlay_defaults: { category: "alpha" },
        input_path: "v.markdown",
        output: "text",
    };
    const prompt = buildSystemPrompt({ kind: "bravo" }, settings);
    expect(prompt).toBe("BASE\n\n---\n\nOVERLAY_A\n\n---\n\nOVERLAY_B");
});

test("buildSystemPrompt: envelope kind overrides missing default", () => {
    writePrompts({
        "convert_v1.txt": "BASE",
        "convert_v1_charlie.txt": "OVERLAY_C",
    });
    const settings: Settings = {
        provider: "anthropic",
        model: "m",
        prompts: { dir, name: "convert", version: "v1", overlays: ["kind"] },
        input_path: "v.text",
        output: "text",
    };
    expect(buildSystemPrompt({ kind: "charlie" }, settings)).toBe("BASE\n\n---\n\nOVERLAY_C");
});

// ─── processEnvelope — text output ───────────────────────────────────

describe("processEnvelope text output", () => {
    test("simple: composes prompt, emits {response}", async () => {
        writePrompts({ "extract_v1.txt": "BASE PROMPT" });
        const provider = new MockProvider({ responses: ["SUMMARY"] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "extract", version: "v1" },
            input_path: "v.markdown",
            output: "text",
        };
        const ctx = makeCtx();
        await processEnvelope({ markdown: "source content", kind: "bravo" }, ctx, { provider, settings });

        expect(ctx.outputs).toHaveLength(1);
        expect(ctx.outputs[0]).toEqual({ response: "SUMMARY", kind: "bravo" });
        expect(provider.calls).toHaveLength(1);
        expect(provider.calls[0].system).toBe("BASE PROMPT");
        expect(provider.calls[0].user).toBe("source content");
    });

    test("success after external retries logs at info level via the framework's opts object", async () => {
        writePrompts({ "extract_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ["OK"] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "extract", version: "v1" },
            input_path: "v.text",
            output: "text",
        };
        const ctx = makeCtx();
        const logs: unknown[][] = [];
        ctx.log = (...args: unknown[]) => {
            logs.push(args);
        };
        await processEnvelope({ text: "x", error_attempt: 2 }, ctx, { provider, settings });
        expect(logs).toEqual([["llm: succeeded after 2 external retries", { level: "info" }]]);
    });

    test("text: strips markdown fences from response", async () => {
        writePrompts({ "extract_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ["```markdown\nhello\n```"] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "extract", version: "v1" },
            input_path: "v.text",
            output: "text",
        };
        const ctx = makeCtx();
        await processEnvelope({ text: "x" }, ctx, { provider, settings });
        expect(ctx.outputs[0]).toEqual({ response: "hello" });
    });

    test("text: preserves other envelope fields, removes the consumed one", async () => {
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ["R"] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.markdown",
            output: "text",
        };
        const ctx = makeCtx();
        await processEnvelope(
            {
                markdown: "md body",
                kind: "bravo",
                category: "alpha",
                tags: ["a", "b"],
            },
            ctx,
            { provider, settings },
        );
        expect(ctx.outputs[0]).toEqual({
            response: "R",
            kind: "bravo",
            category: "alpha",
            tags: ["a", "b"],
        });
    });
});

// ─── processEnvelope — json output ───────────────────────────────────

describe("processEnvelope json output", () => {
    test("parses clean JSON response", async () => {
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ['{"rate": 1.5, "region": "EU"}'] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "json",
        };
        const ctx = makeCtx();
        await processEnvelope({ text: "input" }, ctx, { provider, settings });
        expect(ctx.outputs[0]).toEqual({ rate: 1.5, region: "EU" });
    });

    test("json: strips fenced JSON and parses", async () => {
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ['```json\n{"ok":true}\n```'] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "json",
        };
        const ctx = makeCtx();
        await processEnvelope({ text: "x" }, ctx, { provider, settings });
        expect(ctx.outputs[0]).toEqual({ ok: true });
    });

    test("json: invalid JSON goes to ctx.error, NOT output", async () => {
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ["not a json"] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "json",
        };
        const ctx = makeCtx();
        await processEnvelope({ text: "x" }, ctx, { provider, settings });
        expect(ctx.outputs).toHaveLength(0);
        expect(ctx.errors).toHaveLength(1);
        expect(ctx.errors[0].err).toMatch(/json parse failed/);
    });
});

// ─── failure paths ───────────────────────────────────────────────────

describe("processEnvelope errors", () => {
    test("provider throws → ctx.error, no output", async () => {
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ errors: [new Error("boom")] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "text",
        };
        const ctx = makeCtx();
        await processEnvelope({ text: "x" }, ctx, { provider, settings });
        expect(ctx.outputs).toHaveLength(0);
        expect(ctx.errors[0].err).toMatch(/provider call failed.*boom/);
    });

    test("empty input_path value → ctx.error", async () => {
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({});
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.missing",
            output: "text",
        };
        const ctx = makeCtx();
        await processEnvelope({ other: "x" }, ctx, { provider, settings });
        expect(ctx.outputs).toHaveLength(0);
        expect(ctx.errors[0].err).toMatch(/resolved to empty/);
        expect(provider.calls).toHaveLength(0);
    });

    test("missing base prompt → ctx.error", async () => {
        // no prompts written
        const provider = new MockProvider({});
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "nonexistent", version: "v1" },
            input_path: "v.text",
            output: "text",
        };
        const ctx = makeCtx();
        await processEnvelope({ text: "hi" }, ctx, { provider, settings });
        expect(ctx.errors[0].err).toMatch(/prompt compose failed.*not found/);
    });
});

// ─── cfg propagation ─────────────────────────────────────────────────

test("settings.thinking_budget propagates into model cfg", async () => {
    writePrompts({ "p_v1.txt": "BASE" });
    const provider = new MockProvider({ responses: ["r"] });
    const settings: Settings = {
        provider: "anthropic",
        model: "claude-opus-4-5",
        temperature: 0,
        max_tokens: 5000,
        thinking_budget: 2000,
        prompts: { dir, name: "p", version: "v1" },
        input_path: "v.text",
        output: "text",
    };
    const ctx = makeCtx();
    await processEnvelope({ text: "x" }, ctx, { provider, settings });
    expect(provider.calls[0].cfg).toEqual({
        model: "claude-opus-4-5",
        temperature: 0,
        max_tokens: 5000,
        thinking_budget: 2000,
    });
});

// ─── providerCacheKey composition ────────────────────────────────────
//
// Pin the cache-key shape: same inputs MUST produce the same key, and
// any field change MUST diverge the key (or stale outputs would be
// served when the user changes settings/prompt/etc).

describe("providerCacheKey", () => {
    const baseSettings: Settings = {
        provider: "anthropic",
        model: "claude-x",
        temperature: 0.0,
        max_tokens: 1024,
        thinking_budget: null,
        prompts: { dir: "/p", name: "n", version: "v1" },
        input_path: "v.text",
        output: "text",
    };
    // Content-sensitive mock hash: every char contributes, so "sys-A"
    // and "sys-B" produce different hashes. Real ctx.hash uses blake2b.
    const h = (s: string) => {
        let acc = 0;
        for (let i = 0; i < s.length; i++) acc = (acc * 31 + s.charCodeAt(i)) >>> 0;
        return `H:${s.length}:${acc.toString(16)}`;
    };

    test("stable for identical inputs", () => {
        const a = providerCacheKey("sys", "user", baseSettings, h);
        const b = providerCacheKey("sys", "user", baseSettings, h);
        expect(a).toEqual(b);
    });

    test("diverges on system prompt", () => {
        const a = providerCacheKey("sys-A", "user", baseSettings, h);
        const b = providerCacheKey("sys-B", "user", baseSettings, h);
        expect(a).not.toEqual(b);
    });

    test("diverges on user text", () => {
        const a = providerCacheKey("sys", "user-A", baseSettings, h);
        const b = providerCacheKey("sys", "user-B", baseSettings, h);
        expect(a).not.toEqual(b);
    });

    test("diverges on model", () => {
        const a = providerCacheKey("s", "u", baseSettings, h);
        const b = providerCacheKey("s", "u", { ...baseSettings, model: "claude-y" }, h);
        expect(a).not.toEqual(b);
    });

    test("diverges on provider", () => {
        const a = providerCacheKey("s", "u", baseSettings, h);
        const b = providerCacheKey("s", "u", { ...baseSettings, provider: "google" }, h);
        expect(a).not.toEqual(b);
    });

    test("diverges on temperature", () => {
        const a = providerCacheKey("s", "u", baseSettings, h);
        const b = providerCacheKey("s", "u", { ...baseSettings, temperature: 0.7 }, h);
        expect(a).not.toEqual(b);
    });

    test("diverges on max_tokens", () => {
        const a = providerCacheKey("s", "u", baseSettings, h);
        const b = providerCacheKey("s", "u", { ...baseSettings, max_tokens: 256 }, h);
        expect(a).not.toEqual(b);
    });

    test("diverges on thinking_budget", () => {
        const a = providerCacheKey("s", "u", baseSettings, h);
        const b = providerCacheKey("s", "u", { ...baseSettings, thinking_budget: 1000 }, h);
        expect(a).not.toEqual(b);
    });

    test("default temperature/max_tokens are normalized when omitted", () => {
        const noDefaults: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir: "/p", name: "n", version: "v1" },
            input_path: "v.text",
            output: "text",
        };
        const explicit: Settings = {
            ...noDefaults,
            temperature: 0,
            max_tokens: 64000,
            thinking_budget: null,
        };
        // The two settings should produce identical keys because the
        // defaults match — guards against accidentally caching a "no
        // explicit value" run separately from "matched explicit value".
        expect(providerCacheKey("s", "u", noDefaults, h)).toEqual(providerCacheKey("s", "u", explicit, h));
    });

    test("CACHE_NAMESPACE is 'llm'", () => {
        expect(CACHE_NAMESPACE).toBe("llm");
    });
});

// ─── cache hit/miss integration via real framework helper ────────────
//
// Build a real Context against a tempdir-backed DPE_STORAGE; verify
// the second processEnvelope call with identical inputs hits the
// cache (provider only invoked once).

describe("processEnvelope: ctx.cached flow", () => {
    let storage: string;
    let prevStorage: string | undefined;
    let prevMode: string | undefined;

    beforeEach(() => {
        storage = join(tmpdir(), `llm-cache-${Date.now()}-${Math.random().toString(36).slice(2)}`);
        mkdirSync(storage, { recursive: true });
        prevStorage = process.env.DPE_STORAGE;
        prevMode = process.env.DPE_CACHE_MODE;
        process.env.DPE_STORAGE = storage;
        delete process.env.DPE_CACHE_MODE;
    });
    afterEach(() => {
        rmSync(storage, { recursive: true, force: true });
        if (prevStorage === undefined) delete process.env.DPE_STORAGE;
        else process.env.DPE_STORAGE = prevStorage;
        if (prevMode === undefined) delete process.env.DPE_CACHE_MODE;
        else process.env.DPE_CACHE_MODE = prevMode;
    });

    async function buildRealCtx(): Promise<any> {
        // Use the real framework Context — it delegates ctx.cached to
        // cachedImpl, so this exercises the same code path a runtime
        // would. Output/error are captured via a wrapper, but we never
        // call them in cache tests.
        const fw = await import("@combycode/dpe-framework-ts");
        const outputs: any[] = [];
        const errors: { v: any; err: string }[] = [];
        const memory = new fw.Memory();
        const runtime = {
            enqueue: () => {},
            drainQueue: async () => {},
        };
        const ctx = new fw.Context("x", "seed", memory, runtime);
        // Decorate output/error capture (overrides go around the real
        // methods so cache tests can still assert on outputs).
        const origOutput = ctx.output.bind(ctx);
        const origError = ctx.error.bind(ctx);
        (ctx as any).output = (v: any, opts?: any) => {
            outputs.push(v);
            origOutput(v, opts);
        };
        (ctx as any).error = (v: any, err: unknown) => {
            errors.push({ v, err: err instanceof Error ? err.message : String(err) });
            origError(v, err);
        };
        (ctx as any).outputs = outputs;
        (ctx as any).errors = errors;
        return ctx;
    }

    test("second identical call hits cache; provider invoked once", async () => {
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ["RESP-1", "RESP-2"] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "text",
        };
        const ctx1 = await buildRealCtx();
        await processEnvelope({ text: "hello" }, ctx1, { provider, settings });
        expect(ctx1.outputs[0]).toEqual({ response: "RESP-1" });
        expect(provider.calls).toHaveLength(1);

        // Second call, same input → should hit cache, provider NOT
        // called again. We left "RESP-2" queued so a fresh produce
        // would consume it; if cache hits we keep "RESP-1".
        const ctx2 = await buildRealCtx();
        await processEnvelope({ text: "hello" }, ctx2, { provider, settings });
        expect(ctx2.outputs[0]).toEqual({ response: "RESP-1" });
        expect(provider.calls).toHaveLength(1); // still 1 — cache hit
    });

    test("DPE_CACHE_MODE=bypass forces fresh produce", async () => {
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ["FIRST", "SECOND"] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "text",
        };
        const ctx1 = await buildRealCtx();
        await processEnvelope({ text: "hello" }, ctx1, { provider, settings });
        expect(ctx1.outputs[0]).toEqual({ response: "FIRST" });

        process.env.DPE_CACHE_MODE = "bypass";
        const ctx2 = await buildRealCtx();
        await processEnvelope({ text: "hello" }, ctx2, { provider, settings });
        expect(ctx2.outputs[0]).toEqual({ response: "SECOND" });
        expect(provider.calls).toHaveLength(2);
    });

    test("different user text → different cache entry, both produce", async () => {
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ["A-RESP", "B-RESP"] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "text",
        };
        const ctxA = await buildRealCtx();
        await processEnvelope({ text: "alpha" }, ctxA, { provider, settings });
        const ctxB = await buildRealCtx();
        await processEnvelope({ text: "bravo" }, ctxB, { provider, settings });
        expect(ctxA.outputs[0]).toEqual({ response: "A-RESP" });
        expect(ctxB.outputs[0]).toEqual({ response: "B-RESP" });
        expect(provider.calls).toHaveLength(2);
    });

    // ─── cache_dir override (opt-in per-batch cache isolation) ───────────

    test("cache_dir override: cache lands under custom dir; same hash filename", async () => {
        const { readdirSync, existsSync } = await import("node:fs");
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ["X-RESP"] });
        const customDir = join(storage, "batch_42", "llm"); // arbitrary nested path
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "text",
            cache_dir: customDir,
        };

        const ctx = await buildRealCtx();
        await processEnvelope({ text: "hello" }, ctx, { provider, settings });
        expect(ctx.outputs[0]).toEqual({ response: "X-RESP" });

        // Cache landed UNDER customDir, NOT under the default $DPE_STORAGE/llm/.
        expect(existsSync(customDir)).toBe(true);
        const customEntries = readdirSync(customDir);
        expect(customEntries.length).toBe(1);
        expect(customEntries[0]).toMatch(/^[0-9a-f]{32}\.json$/);
        // Default location should be untouched.
        expect(existsSync(join(storage, "llm"))).toBe(false);
    });

    test("cache_dir override: hash filename matches default location's hash", async () => {
        // Same (prompt, model, user text) → SAME 32-hex filename whether
        // it's stored at $DPE_STORAGE/llm/ or at a custom cache_dir. This
        // pins the contract that only the directory changes; the key
        // derivation stays identical.
        const { readdirSync } = await import("node:fs");
        writePrompts({ "p_v1.txt": "BASE" });
        const provider1 = new MockProvider({ responses: ["A"] });
        const provider2 = new MockProvider({ responses: ["A"] });
        const baseSettings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "text",
        };

        // Run 1: default cache (no cache_dir).
        const ctxDefault = await buildRealCtx();
        await processEnvelope({ text: "same-input" }, ctxDefault, { provider: provider1, settings: baseSettings });
        const defaultFiles = readdirSync(join(storage, "llm"));
        expect(defaultFiles).toHaveLength(1);
        const defaultHash = defaultFiles[0];

        // Run 2: cache_dir set, fresh tempdir for the override location
        // (use a sibling of $DPE_STORAGE so the runs are isolated).
        const customDir = join(storage, "custom_root", "llm");
        const ctxCustom = await buildRealCtx();
        await processEnvelope({ text: "same-input" }, ctxCustom, {
            provider: provider2,
            settings: { ...baseSettings, cache_dir: customDir },
        });
        const customFiles = readdirSync(customDir);
        expect(customFiles).toHaveLength(1);
        expect(customFiles[0]).toBe(defaultHash); // same key derivation
    });

    test("cache_dir empty/undefined → falls back to default $DPE_STORAGE/llm/", async () => {
        const { existsSync } = await import("node:fs");
        writePrompts({ "p_v1.txt": "BASE" });
        const provider = new MockProvider({ responses: ["Y"] });
        const settings: Settings = {
            provider: "anthropic",
            model: "m",
            prompts: { dir, name: "p", version: "v1" },
            input_path: "v.text",
            output: "text",
            // cache_dir intentionally unset
        };
        const ctx = await buildRealCtx();
        await processEnvelope({ text: "hello" }, ctx, { provider, settings });
        // Default location populated (existing behavior unchanged).
        expect(existsSync(join(storage, "llm"))).toBe(true);
    });
});

// ─── regression — main() must await run() ────────────────────

describe("main() bootstrap", () => {
    test("main is async and awaits run() — regression", () => {
        // Source-shape lint. The bug was `function main(): void` + an
        // unawaited `run(...)` call: on Bun (Windows), the runtime
        // exits before the stdin async-iter pulls any chunks, so the
        // tool processes zero envelopes and `dpe test` reports a
        // vacuous PASS with empty actuals. Lock the fix at the source
        // level — a future refactor that drops the await will fail
        // here loudly.
        const src = require("node:fs").readFileSync(require("node:path").join(__dirname, "../src/main.ts"), "utf-8");
        // Strip line comments so they don't satisfy the patterns.
        const code = src
            .split("\n")
            .filter((l: string) => !l.trim().startsWith("//"))
            .join("\n");
        expect(code).toMatch(/async\s+function\s+main\s*\(\)/);
        expect(code).toMatch(/await\s+run\s*\(/);
        // The top-level invocation must also handle rejection so
        // unhandled errors don't vanish silently.
        expect(code).toMatch(/main\(\)\s*\.catch\s*\(/);
    });
});
