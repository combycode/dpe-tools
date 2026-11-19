import { test, expect, describe } from "bun:test";
import { GoogleProvider, extractText } from "../src/providers/google";
import type { ModelCfg } from "../src/types";

const BASE_CFG: ModelCfg = {
    model: "gemini-3-flash-preview",
    temperature: 0,
    max_tokens: 1000,
    thinking_budget: null,
    thinking_adaptive: false,
    effort: null,
};

// ─── URL + body shape ────────────────────────────────────────────────

describe("GoogleProvider.buildUrl / buildBody", () => {
    test("URL includes model and key as query string", () => {
        const p = new GoogleProvider({ apiKey: "SECRET", fetch: (async () => new Response()) as any });
        const u = p.buildUrl("gemini-3-flash-preview");
        expect(u).toBe(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-flash-preview:generateContent?key=SECRET",
        );
    });

    test("body uses modern generateContent shape", () => {
        const p = new GoogleProvider({ apiKey: "t", fetch: (async () => new Response()) as any });
        const body = p.buildBody("SYSTEM", "USER", BASE_CFG);
        expect(body.contents).toEqual([{ role: "user", parts: [{ text: "USER" }] }]);
        expect(body.systemInstruction).toEqual({ parts: [{ text: "SYSTEM" }] });
        expect(body.generationConfig.temperature).toBe(0);
        expect(body.generationConfig.maxOutputTokens).toBe(1000);
    });

    test("thinking_budget populates generationConfig.thinkingConfig", () => {
        const p = new GoogleProvider({ apiKey: "t", fetch: (async () => new Response()) as any });
        const body = p.buildBody("S", "U", { ...BASE_CFG, thinking_budget: 10000 });
        expect(body.generationConfig.thinkingConfig).toEqual({ thinkingBudget: 10000 });
    });

    test("thinking_budget null/0 → no thinkingConfig", () => {
        const p = new GoogleProvider({ apiKey: "t", fetch: (async () => new Response()) as any });
        const b1 = p.buildBody("s", "u", BASE_CFG);
        expect(b1.generationConfig.thinkingConfig).toBeUndefined();
        const b2 = p.buildBody("s", "u", { ...BASE_CFG, thinking_budget: 0 });
        expect(b2.generationConfig.thinkingConfig).toBeUndefined();
    });
});

// ─── request shape ───────────────────────────────────────────────────

test("Google: sends correct headers + URL", async () => {
    let capturedUrl = "";
    let capturedInit: any = null;
    const fakeFetch = async (url: string, init: any) => {
        capturedUrl = url.toString();
        capturedInit = init;
        return new Response(
            JSON.stringify({
                candidates: [{ content: { parts: [{ text: "ok" }] } }],
            }),
            { status: 200 },
        );
    };
    const p = new GoogleProvider({ apiKey: "SECRET", fetch: fakeFetch as any });
    await p.messageSingle("s", "u", BASE_CFG);

    expect(capturedUrl).toContain("generateContent?key=SECRET");
    expect(capturedInit.method).toBe("POST");
    expect(capturedInit.headers["content-type"]).toBe("application/json");
});

// ─── response parsing ────────────────────────────────────────────────

describe("extractText (google)", () => {
    test("single part", () => {
        expect(
            extractText({
                candidates: [{ content: { parts: [{ text: "hi" }] } }],
            }),
        ).toBe("hi");
    });
    test("joins multiple parts", () => {
        expect(
            extractText({
                candidates: [{ content: { parts: [{ text: "a" }, { text: "b" }] } }],
            }),
        ).toBe("ab");
    });
    test("ignores non-text parts", () => {
        expect(
            extractText({
                candidates: [{ content: { parts: [{ inlineData: {} }, { text: "visible" }] } }],
            }),
        ).toBe("visible");
    });
    test("throws on empty candidates", () => {
        expect(() => extractText({ candidates: [] })).toThrow(/no candidates/);
    });
    test("throws on missing parts", () => {
        expect(() => extractText({ candidates: [{ content: {} }] })).toThrow(/content.parts/);
    });
});

// ─── retries on transient errors ─────────────────────────────────────

test("Google: retries 503 then succeeds", async () => {
    let calls = 0;
    const fakeFetch = async () => {
        calls++;
        if (calls < 2) return new Response("unavail", { status: 503, statusText: "Service Unavailable" });
        return new Response(
            JSON.stringify({
                candidates: [{ content: { parts: [{ text: "ok" }] } }],
            }),
            { status: 200 },
        );
    };
    const p = new GoogleProvider({
        apiKey: "t",
        fetch: fakeFetch as any,
        retry: { max_retries: 2, delays_ms: [1, 1] },
    });
    expect(await p.messageSingle("s", "u", BASE_CFG)).toBe("ok");
    expect(calls).toBe(2);
});

test("Google: 400 is NOT retried", async () => {
    let calls = 0;
    const fakeFetch = async () => {
        calls++;
        return new Response("bad", { status: 400, statusText: "Bad Request" });
    };
    const p = new GoogleProvider({
        apiKey: "t",
        fetch: fakeFetch as any,
        retry: { max_retries: 3, delays_ms: [1, 1, 1] },
    });
    await expect(p.messageSingle("s", "u", BASE_CFG)).rejects.toThrow(/400/);
    expect(calls).toBe(1);
});

test("Google: temperature null is not sent", () => {
    const p = new GoogleProvider({ apiKey: "k", fetch: (async () => new Response()) as any });
    expect(p.buildBody("S", "U", { ...BASE_CFG, temperature: null }).generationConfig).not.toHaveProperty(
        "temperature",
    );
    expect(p.buildBody("S", "U", BASE_CFG).generationConfig.temperature).toBe(0);
});
