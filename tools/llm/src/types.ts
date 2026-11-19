/**
 * Shared types for dpe-tool-llm.
 *
 * Settings are parsed from the tool's argv[1] JSON string on startup;
 * the runtime dispatches envelopes through `main.ts::processInput`.
 */

export type Provider = "anthropic" | "google";

export interface PromptsCfg {
    /** Absolute or $prefix-resolved directory containing prompt files. */
    dir: string;
    /** Prompt base name (e.g. "extract_summary"). Filename = <name>_<version>.txt */
    name: string;
    /** Version string (e.g. "v1"). */
    version: string;
    /** Ordered list of overlay names to apply (e.g. ["category", "kind"]). */
    overlays?: string[];
}

export interface RetryCfg {
    max_retries?: number;
    delays_ms?: number[];
}

/**
 * How `processEnvelope` surfaces terminal LLM failures.
 *
 *   "stderr"   (default) — `ctx.error(v, msg)` only. Envelope dropped from
 *                          the data stream; downstream sees nothing.
 *   "envelope"          — emit fail-envelope on data channel with
 *                          `v.failed=true`, `v.error_class`, `v.error_msg`,
 *                          `v.error_attempt`, and source-identity fields.
 *                          Lets pipeline YAML route failures to a sink or
 *                          back into the tool after a cooldown.
 *   "both"             — emit fail-envelope AND call `ctx.error`. Useful
 *                          while migrating an existing pipeline.
 */
export type OnFailureMode = "stderr" | "envelope" | "both";

export interface Settings {
    provider: Provider;
    model: string;
    temperature?: number;
    max_tokens?: number;
    thinking_budget?: number | null;

    prompts: PromptsCfg;

    /**
     * Fallback overlay values when the envelope doesn't carry them.
     * Example: { category: "alpha" }. Envelope values ALWAYS override.
     */
    overlay_defaults?: Record<string, string>;

    /**
     * Path into envelope to pick the user-content. Supported syntax:
     *   "v"            → the whole v field
     *   "v.markdown"   → v.markdown
     *   "v.a.b.c"      → nested
     */
    input_path: string;

    /**
     * "text": wrap response in {response: "..."}; preserves other v fields.
     * "json": parse response as JSON (after stripping fences); replace v.
     */
    output: "text" | "json";

    retry?: RetryCfg;

    /**
     * Optional override for the LLM response cache directory. If unset,
     * cache lands at the framework's default `$DPE_STORAGE/llm/<hash>.json`.
     * Set to e.g. `"$storage/batch_${BATCH}/llm"` to keep cache entries
     * inside a per-batch storage subtree so `rm -rf storage/batch_N/`
     * wipes the cache atomically with the batch's other state.
     *
     * Token substitution (`$storage`, `$temp`, `$configs`, env vars) is
     * applied by the runner before the tool receives this setting.
     *
     * Cache filename (`<32-hex-hash>.json`) and key derivation are
     * unchanged; only the directory differs. Trade-off: per-batch
     * caching means cross-batch cache misses — if the same contract
     * appears in two batches, batch 2 re-pays the LLM cost.
     */
    cache_dir?: string;

    /**
     * How to surface terminal LLM failures. Default "stderr" — the legacy
     * behaviour that drops the envelope from the data stream. Set to
     * "envelope" to emit a fail-envelope on the data channel for
     * pipeline-side routing (dead-letter sink, retry loop, etc.); see
     * README's "Failure handling & retry loop" section.
     */
    on_failure?: OnFailureMode;
}

/** One fully-resolved LLM call config passed to a Provider. */
export interface ModelCfg {
    model: string;
    temperature: number;
    max_tokens: number;
    thinking_budget: number | null;
}

/** Interface every provider impl satisfies. */
export interface ProviderClient {
    /**
     * Single-shot text call.
     * Returns the raw text response. Caller strips fences + parses JSON.
     * Throws on non-transient errors; retries transient errors internally.
     */
    messageSingle(system: string, userText: string, cfg: ModelCfg): Promise<string>;
}
