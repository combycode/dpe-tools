import { test, expect, describe } from "bun:test";
import { AnthropicProvider, extractText } from "../src/providers/anthropic";
import type { ModelCfg } from "../src/types";

const BASE_CFG: ModelCfg = {
    model: "claude-haiku-4-5-20251001",
    temperature: 0,
    max_tokens: 1000,
    thinking_budget: null,
};

// ─── buildBody ───────────────────────────────────────────────────────

describe("AnthropicProvider.buildBody", () => {
    test("produces modern Messages API body (NOT completions)", () => {
        const p = new AnthropicProvider({ apiKey: "test", fetch: (async () => new Response()) as any });
        const body = p.buildBody("SYSTEM", "USER", BASE_CFG);

        // Modern API requires these top-level keys:
        expect(body).toHaveProperty("model");
        expect(body).toHaveProperty("max_tokens");
        expect(body).toHaveProperty("messages");
        expect(body).toHaveProperty("system");

        // NOT the legacy Completions API — these must NOT be present:
        expect(body).not.toHaveProperty("prompt");
        expect(body).not.toHaveProperty("max_tokens_to_sample");

        // messages is a single-user-turn with string content
        expect(body.messages).toEqual([{ role: "user", content: "USER" }]);
        expect(body.system).toBe("SYSTEM");
        expect(body.model).toBe("claude-haiku-4-5-20251001");
        expect(body.temperature).toBe(0);
    });

    test("thinking_budget sets thinking config and forces temperature=1", () => {
        const p = new AnthropicProvider({ apiKey: "t", fetch: (async () => new Response()) as any });
        const body = p.buildBody("S", "U", { ...BASE_CFG, thinking_budget: 8000 });
        expect(body.thinking).toEqual({ type: "enabled", budget_tokens: 8000 });
        expect(body.temperature).toBe(1);
    });

    test("thinking_budget null → no thinking field", () => {
        const p = new AnthropicProvider({ apiKey: "t", fetch: (async () => new Response()) as any });
        const body = p.buildBody("S", "U", BASE_CFG);
        expect(body.thinking).toBeUndefined();
    });
});

// ─── request shape ───────────────────────────────────────────────────

test("Anthropic: sends correct headers + URL", async () => {
    let capturedUrl = "";
    let capturedInit: any = null;
    const fakeFetch = async (url: string, init: any) => {
        capturedUrl = url.toString();
        capturedInit = init;
        return new Response(
            JSON.stringify({
                content: [{ type: "text", text: "ok" }],
            }),
            { status: 200 },
        );
    };
    const p = new AnthropicProvider({ apiKey: "SECRET", fetch: fakeFetch as any });
    await p.messageSingle("s", "u", BASE_CFG);

    expect(capturedUrl).toBe("https://api.anthropic.com/v1/messages");
    expect(capturedInit.method).toBe("POST");
    expect(capturedInit.headers["x-api-key"]).toBe("SECRET");
    expect(capturedInit.headers["anthropic-version"]).toBe("2023-06-01");
    expect(capturedInit.headers["content-type"]).toBe("application/json");
});

// ─── response parsing ────────────────────────────────────────────────

describe("extractText", () => {
    test("single text block", () => {
        expect(extractText({ content: [{ type: "text", text: "hi" }] })).toBe("hi");
    });
    test("joins multiple text blocks", () => {
        expect(
            extractText({
                content: [
                    { type: "text", text: "a" },
                    { type: "text", text: "b" },
                ],
            }),
        ).toBe("ab");
    });
    test("ignores thinking / tool_use blocks, returns text only", () => {
        expect(
            extractText({
                content: [
                    { type: "thinking", thinking: "..." },
                    { type: "text", text: "answer" },
                ],
            }),
        ).toBe("answer");
    });
    test("throws on missing content array", () => {
        expect(() => extractText({})).toThrow(/unexpected response shape/);
    });
    test("throws when no text blocks found", () => {
        expect(() => extractText({ content: [{ type: "thinking" }] })).toThrow(/no text blocks/);
    });
});

// ─── retries on transient errors ─────────────────────────────────────

test("Anthropic: retries 429 then succeeds", async () => {
    let calls = 0;
    const fakeFetch = async () => {
        calls++;
        if (calls < 2) return new Response("rate limit", { status: 429, statusText: "Too Many Requests" });
        return new Response(JSON.stringify({ content: [{ type: "text", text: "ok" }] }), { status: 200 });
    };
    const p = new AnthropicProvider({
        apiKey: "t",
        fetch: fakeFetch as any,
        retry: { max_retries: 2, delays_ms: [1, 1] },
    });
    const result = await p.messageSingle("s", "u", BASE_CFG);
    expect(result).toBe("ok");
    expect(calls).toBe(2);
});

test("Anthropic: 400 is NOT retried", async () => {
    let calls = 0;
    const fakeFetch = async () => {
        calls++;
        return new Response("bad request", { status: 400, statusText: "Bad Request" });
    };
    const p = new AnthropicProvider({
        apiKey: "t",
        fetch: fakeFetch as any,
        retry: { max_retries: 3, delays_ms: [1, 1, 1] },
    });
    await expect(p.messageSingle("s", "u", BASE_CFG)).rejects.toThrow(/400/);
    expect(calls).toBe(1);
});

test("Anthropic: exhausts retries on persistent 529", async () => {
    let calls = 0;
    const fakeFetch = async () => {
        calls++;
        return new Response("overloaded", { status: 529, statusText: "Overloaded" });
    };
    const p = new AnthropicProvider({
        apiKey: "t",
        fetch: fakeFetch as any,
        retry: { max_retries: 2, delays_ms: [1, 1] },
    });
    await expect(p.messageSingle("s", "u", BASE_CFG)).rejects.toThrow(/529/);
    expect(calls).toBe(3); // 1 initial + 2 retries
});

test("Anthropic: retries on network error (fetch throws)", async () => {
    let calls = 0;
    const fakeFetch = async () => {
        calls++;
        if (calls < 2) throw new Error("ECONNRESET");
        return new Response(JSON.stringify({ content: [{ type: "text", text: "recovered" }] }), { status: 200 });
    };
    const p = new AnthropicProvider({
        apiKey: "t",
        fetch: fakeFetch as any,
        retry: { max_retries: 2, delays_ms: [1, 1] },
    });
    const r = await p.messageSingle("s", "u", BASE_CFG);
    expect(r).toBe("recovered");
    expect(calls).toBe(2);
});
