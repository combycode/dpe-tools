import { test, expect, describe } from "bun:test";
import { AnthropicProvider, extractText, isTemperatureRejection } from "../src/providers/anthropic";
import type { ModelCfg } from "../src/types";

const BASE_CFG: ModelCfg = {
    model: "claude-haiku-4-5-20251001",
    temperature: 0,
    max_tokens: 1000,
    thinking_budget: null,
    thinking_adaptive: false,
    effort: null,
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

// ─── temperature / thinking / effort ─────────────────────────────────

describe("temperature, adaptive thinking and effort", () => {
    const noFetch = (async () => new Response()) as any;

    test("temperature null is not sent", () => {
        const p = new AnthropicProvider({ apiKey: "t", fetch: noFetch });
        expect(p.buildBody("S", "U", { ...BASE_CFG, model: "m-null", temperature: null })).not.toHaveProperty(
            "temperature",
        );
    });

    test("adaptive thinking + effort, no temperature", () => {
        const p = new AnthropicProvider({ apiKey: "t", fetch: noFetch });
        const body = p.buildBody("S", "U", {
            ...BASE_CFG,
            model: "m-adaptive",
            thinking_adaptive: true,
            effort: "low",
        });
        expect(body.thinking).toEqual({ type: "adaptive" });
        expect(body.output_config).toEqual({ effort: "low" });
        expect(body).not.toHaveProperty("temperature");
    });

    test("isTemperatureRejection only for a 400 about an in-range temperature", () => {
        const msg = '{"error":{"message":"`temperature` is deprecated for this model."}}';
        expect(isTemperatureRejection(400, msg, 0)).toBe(true);
        expect(isTemperatureRejection(400, "max_tokens too large", 0)).toBe(false);
        expect(isTemperatureRejection(500, "temperature", 0)).toBe(false);
        expect(isTemperatureRejection(400, "temperature: range 0..1", 1.5)).toBe(false);
        expect(isTemperatureRejection(400, msg, undefined)).toBe(false);
    });

    test("temperature null also wins over the thinking-budget rule", () => {
        const p = new AnthropicProvider({ apiKey: "t", fetch: noFetch });
        expect(p.buildBody("S", "U", { ...BASE_CFG, model: "m-b1", thinking_budget: 2000 }).temperature).toBe(1);
        expect(
            p.buildBody("S", "U", { ...BASE_CFG, model: "m-b2", thinking_budget: 2000, temperature: null }),
        ).not.toHaveProperty("temperature");
    });

    test("temperature null is never retried", async () => {
        let calls = 0;
        const fetchFn = (async () => {
            calls++;
            return new Response("temperature: something", { status: 400 });
        }) as any;
        const p = new AnthropicProvider({ apiKey: "t", fetch: fetchFn, retry: { max_retries: 0 } });
        await expect(
            p.messageSingle("S", "U", { ...BASE_CFG, model: "m-null-400", temperature: null }),
        ).rejects.toThrow("400");
        expect(calls).toBe(1);
    });

    test("out-of-range temperature is a config error, not retried", async () => {
        let calls = 0;
        const fetchFn = (async () => {
            calls++;
            return new Response("temperature: range 0..1", { status: 400 });
        }) as any;
        const p = new AnthropicProvider({ apiKey: "t", fetch: fetchFn, retry: { max_retries: 0 } });
        await expect(p.messageSingle("S", "U", { ...BASE_CFG, model: "m-range", temperature: 1.5 })).rejects.toThrow(
            "400",
        );
        expect(calls).toBe(1);
    });

    test("a 400 about temperature is retried without it, then remembered for the model", async () => {
        const bodies: any[] = [];
        const fetchFn = (async (_url: string, init: any) => {
            const b = JSON.parse(init.body);
            bodies.push(b);
            if ("temperature" in b) {
                return new Response(
                    JSON.stringify({
                        type: "error",
                        error: {
                            type: "invalid_request_error",
                            message: "`temperature` is deprecated for this model.",
                        },
                    }),
                    { status: 400 },
                );
            }
            return new Response(JSON.stringify({ content: [{ type: "text", text: "ok" }] }), { status: 200 });
        }) as any;
        const p = new AnthropicProvider({ apiKey: "t", fetch: fetchFn, retry: { max_retries: 0 } });
        const cfg = { ...BASE_CFG, model: "m-rejects-temperature" };
        expect(await p.messageSingle("S", "U", cfg)).toBe("ok");
        expect(await p.messageSingle("S", "U", cfg)).toBe("ok");
        expect(bodies.map((b) => "temperature" in b)).toEqual([true, false, false]);
    });

    test("other 400s are not retried", async () => {
        let calls = 0;
        const fetchFn = (async () => {
            calls++;
            return new Response("max_tokens: too large", { status: 400 });
        }) as any;
        const p = new AnthropicProvider({ apiKey: "t", fetch: fetchFn, retry: { max_retries: 0 } });
        await expect(p.messageSingle("S", "U", { ...BASE_CFG, model: "m-other-400" })).rejects.toThrow("400");
        expect(calls).toBe(1);
    });
});
