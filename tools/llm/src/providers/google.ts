/**
 * Google Gemini generateContent API client — REST.
 *
 *   POST https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent?key=<KEY>
 *
 * Body:
 *   {
 *     "contents":          [{ "parts": [{ "text": "<user>" }], "role": "user" }],
 *     "systemInstruction": { "parts": [{ "text": "<system>" }] },
 *     "generationConfig":  { "temperature":..., "maxOutputTokens":..., "thinkingConfig":{ ... } }
 *   }
 *
 * Response:
 *   { "candidates": [{ "content": { "parts": [{ "text": "..." }] }, "finishReason": "STOP" }], ... }
 */

import type { ModelCfg, ProviderClient, RetryCfg } from "../types";

export interface GoogleOpts {
    apiKey: string;
    fetch?: typeof fetch;
    endpointBase?: string;
    retry?: RetryCfg;
}

export class GoogleProvider implements ProviderClient {
    private readonly apiKey: string;
    private readonly fetchFn: typeof fetch;
    private readonly endpointBase: string;
    private readonly maxRetries: number;
    private readonly delaysMs: number[];

    constructor(opts: GoogleOpts) {
        this.apiKey = opts.apiKey;
        this.fetchFn = opts.fetch ?? globalThis.fetch;
        this.endpointBase = opts.endpointBase ?? "https://generativelanguage.googleapis.com";
        this.maxRetries = opts.retry?.max_retries ?? 2;
        this.delaysMs = opts.retry?.delays_ms ?? [5000, 15000];
    }

    buildUrl(model: string): string {
        return `${this.endpointBase}/v1beta/models/${encodeURIComponent(model)}:generateContent?key=${encodeURIComponent(this.apiKey)}`;
    }

    buildBody(system: string, userText: string, cfg: ModelCfg): Record<string, any> {
        const generationConfig: Record<string, any> = {
            temperature: cfg.temperature,
            maxOutputTokens: cfg.max_tokens,
        };
        if (typeof cfg.thinking_budget === "number" && cfg.thinking_budget > 0) {
            generationConfig.thinkingConfig = { thinkingBudget: cfg.thinking_budget };
        }
        return {
            contents: [{ role: "user", parts: [{ text: userText }] }],
            systemInstruction: { parts: [{ text: system }] },
            generationConfig,
        };
    }

    async messageSingle(system: string, userText: string, cfg: ModelCfg): Promise<string> {
        const url = this.buildUrl(cfg.model);
        const body = this.buildBody(system, userText, cfg);
        let lastError: unknown = null;

        for (let attempt = 0; attempt <= this.maxRetries; attempt++) {
            let response: Response;
            try {
                response = await this.fetchFn(url, {
                    method: "POST",
                    headers: { "content-type": "application/json" },
                    body: JSON.stringify(body),
                });
            } catch (e) {
                lastError = e;
                if (!(await this._maybeDelay(attempt))) throw e;
                continue;
            }

            if (response.ok) {
                const data = (await response.json()) as any;
                return extractText(data);
            }

            const status = response.status;
            const errBody = await response.text().catch(() => "");
            const err = new Error(`google ${status} ${response.statusText}: ${errBody.slice(0, 500)}`);
            lastError = err;
            if (isTransient(status) && attempt < this.maxRetries) {
                await this._sleep(this.delaysMs[Math.min(attempt, this.delaysMs.length - 1)]!);
                continue;
            }
            throw err;
        }
        throw lastError ?? new Error("google: exhausted retries");
    }

    private _sleep(ms: number): Promise<void> {
        return new Promise((r) => setTimeout(r, ms));
    }
    private async _maybeDelay(attempt: number): Promise<boolean> {
        if (attempt >= this.maxRetries) return false;
        await this._sleep(this.delaysMs[Math.min(attempt, this.delaysMs.length - 1)]!);
        return true;
    }
}

function isTransient(status: number): boolean {
    return status === 429 || status === 500 || status === 502 || status === 503 || status === 504;
}

export function extractText(data: any): string {
    if (!data || !Array.isArray(data.candidates) || data.candidates.length === 0) {
        throw new Error(`google: no candidates in response: ${JSON.stringify(data)?.slice(0, 200)}`);
    }
    const cand = data.candidates[0];
    const parts = cand?.content?.parts;
    if (!Array.isArray(parts)) {
        throw new Error("google: candidate missing content.parts");
    }
    const texts: string[] = [];
    for (const p of parts) {
        if (p && typeof p.text === "string") texts.push(p.text);
    }
    if (texts.length === 0) throw new Error("google: no text parts");
    return texts.join("");
}
