/**
 * Prompt loader — layered composition.
 *
 * Directory layout (flat, one file per layer):
 *   <dir>/<name>_<version>.txt                  base (required)
 *   <dir>/<name>_<version>_<overlay_value>.txt  overlay (optional)
 *
 * Resolution for a given envelope:
 *   1. Load base.
 *   2. For each overlay-name listed in settings.prompts.overlays[]:
 *        value = envelope.v[name] ?? settings.overlay_defaults[name]
 *        if value is truthy, attempt to load the overlay file.
 *   3. Join with `\n\n---\n\n` between sections.
 *
 * Missing overlay files are skipped silently. Missing base is a hard error.
 */

import { readFileSync, existsSync } from "node:fs";
import { join } from "node:path";

export interface LoadOpts {
    dir: string;
    name: string;
    version: string;
    /** Ordered pairs [overlayName, resolvedValue]; value="" → skip. */
    overlays?: Array<[string, string]>;
}

const SEPARATOR = "\n\n---\n\n";

export function baseFilename(name: string, version: string): string {
    return `${name}_${version}.txt`;
}

export function overlayFilename(name: string, version: string, overlayValue: string): string {
    return `${name}_${version}_${overlayValue}.txt`;
}

export function loadPrompt(opts: LoadOpts): string {
    const basePath = join(opts.dir, baseFilename(opts.name, opts.version));
    if (!existsSync(basePath)) {
        throw new Error(`base prompt not found: ${basePath}`);
    }
    const parts: string[] = [readFileSync(basePath, "utf8").trim()];

    for (const [overlayName, value] of opts.overlays ?? []) {
        if (!value) continue; // skip blanks
        const p = join(opts.dir, overlayFilename(opts.name, opts.version, value));
        if (!existsSync(p)) continue; // missing overlay = silent skip
        const text = readFileSync(p, "utf8").trim();
        if (text) parts.push(text);
        void overlayName; // kept for caller introspection
    }

    return parts.join(SEPARATOR);
}

/**
 * Resolve the value for each overlay name by walking envelope.v then falling
 * back to settings.overlay_defaults. Returns ordered pairs ready for loadPrompt().
 */
export function resolveOverlayValues(
    overlays: string[] | undefined,
    envelopeV: Record<string, any>,
    defaults: Record<string, string> | undefined,
): Array<[string, string]> {
    if (!overlays) return [];
    return overlays.map((name) => {
        const raw = envelopeV?.[name];
        const fromEnv = typeof raw === "string" && raw ? raw : "";
        const fromDefault = (defaults && defaults[name]) || "";
        return [name, fromEnv || fromDefault] as [string, string];
    });
}

/**
 * Resolve a dotted path inside the envelope `v` and return the value.
 *   "v"          → the whole v
 *   "v.markdown" → v.markdown
 *   "v.a.b"      → v.a.b
 * Returns undefined if any step is missing.
 */
export function resolveInputPath(envelopeV: any, inputPath: string): any {
    if (inputPath === "v") return envelopeV;
    const parts = inputPath.split(".");
    if (parts.shift() !== "v") {
        throw new Error(`input_path must start with 'v' (got '${inputPath}')`);
    }
    let cur: any = envelopeV;
    for (const p of parts) {
        if (cur == null || typeof cur !== "object") return undefined;
        cur = cur[p];
    }
    return cur;
}

/** Stringify the resolved input for LLM consumption. */
export function stringifyInput(value: any): string {
    if (typeof value === "string") return value;
    if (value === undefined || value === null) return "";
    return JSON.stringify(value, null, 2);
}
