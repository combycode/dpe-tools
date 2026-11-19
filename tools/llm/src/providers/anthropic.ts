/**
 * Anthropic Messages API client — modern Messages endpoint.
 *
 * NOT the deprecated Completions API. We call:
 *   POST https://api.anthropic.com/v1/messages
 *
 * Headers:
 *   x-api-key:         <ANTHROPIC_API_KEY>
 *   anthropic-version: 2023-06-01
 *   content-type:      application/json
 *
 * Body:
 *   { model, max_tokens, system, messages: [{role:"user", content:"..."}],
 *     temperature?, thinking?, output_config? }
 *
 * Current Claude models reject `temperature`: a 400 about it is retried once
 * without the field, and the model is remembered for the rest of the process.
 *
 * Response: { content: [{type:"text", text:"..."}], ... }
 */

import type { ModelCfg, ProviderClient, RetryCfg } from "../types";

export interface AnthropicOpts {
    apiKey: string;
    /** Used in tests to bypass real network. Defaults to globalThis.fetch. */
    fetch?: typeof fetch;
    endpoint?: string;
    retry?: RetryCfg;
}

/** Models that answered with a 400 about `temperature`; requests to them leave it out. */
const NO_TEMPERATURE = new Set<string>();

/** True for a 400 whose error text is about the temperature parameter. */
export function isTemperatureRejection(status: number, body: string): boolean {
    return status === 400 && body.toLowerCase().includes("temperature");
}

class AnthropicHttpError extends Error {
    constructor(
        readonly status: number,
        readonly body: string,
        message: string,
    ) {
        super(message);
    }
}

export class AnthropicProvider implements ProviderClient {
    private readonly apiKey: string;
    private readonly fetchFn: typeof fetch;
    private readonly endpoint: string;
    private readonly maxRetries: number;
    private readonly delaysMs: number[];

    constructor(opts: AnthropicOpts) {
        this.apiKey = opts.apiKey;
        this.fetchFn = opts.fetch ?? globalThis.fetch;
        this.endpoint = opts.endpoint ?? "https://api.anthropic.com/v1/messages";
        this.maxRetries = opts.retry?.max_retries ?? 2;
        this.delaysMs = opts.retry?.delays_ms ?? [5000, 15000];
    }

    buildBody(system: string, userText: string, cfg: ModelCfg): Record<string, any> {
        const budgetOn = typeof cfg.thinking_budget === "number" && cfg.thinking_budget > 0;
        const body: Record<string, any> = {
            model: cfg.model,
            max_tokens: cfg.max_tokens,
            system,
            messages: [{ role: "user", content: userText }],
        };
        // Adaptive thinking runs with the model's own sampling; with a thinking budget the
        // API requires temperature 1. A model known to reject temperature gets none.
        const temperature = cfg.thinking_adaptive ? null : budgetOn ? 1 : cfg.temperature;
        if (temperature !== null && !NO_TEMPERATURE.has(cfg.model)) body.temperature = temperature;
        if (cfg.thinking_adaptive) body.thinking = { type: "adaptive" };
        else if (budgetOn) body.thinking = { type: "enabled", budget_tokens: cfg.thinking_budget };
        if (cfg.effort !== null) body.output_config = { effort: cfg.effort };
        return body;
    }

    async messageSingle(system: string, userText: string, cfg: ModelCfg): Promise<string> {
        const body = this.buildBody(system, userText, cfg);
        try {
            return await this.post(body);
        } catch (e) {
            if (
                !("temperature" in body) ||
                !(e instanceof AnthropicHttpError) ||
                !isTemperatureRejection(e.status, e.body)
            ) {
                throw e;
            }
            if (!NO_TEMPERATURE.has(cfg.model)) {
                NO_TEMPERATURE.add(cfg.model);
                process.stderr.write(
                    `${JSON.stringify({ type: "log", level: "warn", msg: `anthropic: model ${cfg.model} rejects temperature; resending without it (set temperature: null to skip this)` })}\n`,
                );
            }
            return await this.post(this.buildBody(system, userText, cfg));
        }
    }

    private async post(body: Record<string, any>): Promise<string> {
        let lastError: unknown = null;

        for (let attempt = 0; attempt <= this.maxRetries; attempt++) {
            let response: Response;
            try {
                response = await this.fetchFn(this.endpoint, {
                    method: "POST",
                    headers: {
                        "x-api-key": this.apiKey,
                        "anthropic-version": "2023-06-01",
                        "content-type": "application/json",
                    },
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
            const err = new AnthropicHttpError(
                status,
                errBody,
                `anthropic ${status} ${response.statusText}: ${errBody.slice(0, 500)}`,
            );
            lastError = err;
            if (isTransient(status) && attempt < this.maxRetries) {
                await this._sleep(this.delaysMs[Math.min(attempt, this.delaysMs.length - 1)]!);
                continue;
            }
            throw err;
        }
        throw lastError ?? new Error("anthropic: exhausted retries");
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
    return status === 429 || status === 500 || status === 502 || status === 503 || status === 504 || status === 529;
}

export function extractText(data: any): string {
    // Expected shape: { content: [{type:"text", text:"..."}, ...], ... }
    if (!data || !Array.isArray(data.content)) {
        throw new Error(`anthropic: unexpected response shape: ${JSON.stringify(data)?.slice(0, 200)}`);
    }
    const textParts: string[] = [];
    for (const block of data.content) {
        if (block && block.type === "text" && typeof block.text === "string") {
            textParts.push(block.text);
        }
    }
    if (textParts.length === 0) {
        throw new Error("anthropic: no text blocks in response");
    }
    return textParts.join("");
}
