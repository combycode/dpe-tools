import { test, expect, describe, beforeEach, afterEach } from "bun:test";
import { mkdirSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
    loadPrompt,
    resolveOverlayValues,
    resolveInputPath,
    stringifyInput,
    baseFilename,
    overlayFilename,
} from "../src/prompt-loader";

let dir: string;

beforeEach(() => {
    dir = join(tmpdir(), `prompt-test-${Date.now()}-${Math.random().toString(36).slice(2)}`);
    mkdirSync(dir, { recursive: true });
});
afterEach(() => {
    rmSync(dir, { recursive: true, force: true });
});

function writeFile(rel: string, body: string) {
    writeFileSync(join(dir, rel), body, "utf8");
}

// ─── filename helpers ────────────────────────────────────────────────

test("baseFilename: convert_v1.txt", () => {
    expect(baseFilename("convert", "v1")).toBe("convert_v1.txt");
});
test("overlayFilename: convert_v1_alpha.txt", () => {
    expect(overlayFilename("convert", "v1", "alpha")).toBe("convert_v1_alpha.txt");
});

// ─── loadPrompt ──────────────────────────────────────────────────────

test("loadPrompt: base only", () => {
    writeFile("convert_v1.txt", "BASE");
    const p = loadPrompt({ dir, name: "convert", version: "v1" });
    expect(p).toBe("BASE");
});

test("loadPrompt: base + one overlay joined with separator", () => {
    writeFile("convert_v1.txt", "BASE");
    writeFile("convert_v1_alpha.txt", "OVERLAY_A_DEMO");
    const p = loadPrompt({
        dir,
        name: "convert",
        version: "v1",
        overlays: [["category", "alpha"]],
    });
    expect(p).toBe("BASE\n\n---\n\nOVERLAY_A_DEMO");
});

test("loadPrompt: base + multiple overlays in order", () => {
    writeFile("convert_v1.txt", "BASE");
    writeFile("convert_v1_alpha.txt", "OVERLAY_A");
    writeFile("convert_v1_bravo.txt", "OVERLAY_B");
    const p = loadPrompt({
        dir,
        name: "convert",
        version: "v1",
        overlays: [
            ["category", "alpha"],
            ["kind", "bravo"],
        ],
    });
    expect(p).toBe("BASE\n\n---\n\nOVERLAY_A\n\n---\n\nOVERLAY_B");
});

test("loadPrompt: missing overlay silently skipped", () => {
    writeFile("convert_v1.txt", "BASE");
    writeFile("convert_v1_bravo.txt", "OVERLAY_B");
    const p = loadPrompt({
        dir,
        name: "convert",
        version: "v1",
        overlays: [
            ["category", "nope"],
            ["kind", "bravo"],
        ],
    });
    expect(p).toBe("BASE\n\n---\n\nOVERLAY_B");
});

test("loadPrompt: empty overlay value skipped", () => {
    writeFile("convert_v1.txt", "BASE");
    const p = loadPrompt({
        dir,
        name: "convert",
        version: "v1",
        overlays: [
            ["category", ""],
            ["kind", ""],
        ],
    });
    expect(p).toBe("BASE");
});

test("loadPrompt: missing base throws", () => {
    expect(() => loadPrompt({ dir, name: "missing", version: "v1" })).toThrow(/base prompt not found/);
});

test("loadPrompt: trims trailing whitespace in each layer", () => {
    writeFile("convert_v1.txt", "BASE   \n\n\n");
    writeFile("convert_v1_alpha.txt", "\n\nOVERLAY\n");
    const p = loadPrompt({
        dir,
        name: "convert",
        version: "v1",
        overlays: [["category", "alpha"]],
    });
    expect(p).toBe("BASE\n\n---\n\nOVERLAY");
});

// ─── resolveOverlayValues ────────────────────────────────────────────

describe("resolveOverlayValues", () => {
    test("envelope value takes precedence over default", () => {
        const r = resolveOverlayValues(["category"], { category: "bravo" }, { category: "alpha" });
        expect(r).toEqual([["category", "bravo"]]);
    });
    test("default used when envelope missing", () => {
        const r = resolveOverlayValues(["category"], {}, { category: "alpha" });
        expect(r).toEqual([["category", "alpha"]]);
    });
    test("empty envelope value falls through to default", () => {
        const r = resolveOverlayValues(["category"], { category: "" }, { category: "alpha" });
        expect(r).toEqual([["category", "alpha"]]);
    });
    test("non-string envelope value ignored", () => {
        const r = resolveOverlayValues(["category"], { category: 42 as any }, { category: "alpha" });
        expect(r).toEqual([["category", "alpha"]]);
    });
    test("no default, no envelope → empty value (will be skipped by loader)", () => {
        const r = resolveOverlayValues(["category"], {}, undefined);
        expect(r).toEqual([["category", ""]]);
    });
    test("preserves order across multiple overlays", () => {
        const r = resolveOverlayValues(
            ["category", "kind", "region"],
            { kind: "bravo" },
            { category: "alpha", region: "EU" },
        );
        expect(r).toEqual([
            ["category", "alpha"],
            ["kind", "bravo"],
            ["region", "EU"],
        ]);
    });
    test("undefined overlays list returns []", () => {
        expect(resolveOverlayValues(undefined, {}, undefined)).toEqual([]);
    });
});

// ─── resolveInputPath ────────────────────────────────────────────────

describe("resolveInputPath", () => {
    test('"v" returns whole v', () => {
        expect(resolveInputPath({ a: 1 }, "v")).toEqual({ a: 1 });
    });
    test("single-level access", () => {
        expect(resolveInputPath({ markdown: "hi" }, "v.markdown")).toBe("hi");
    });
    test("nested access", () => {
        expect(resolveInputPath({ a: { b: { c: "deep" } } }, "v.a.b.c")).toBe("deep");
    });
    test("missing key → undefined", () => {
        expect(resolveInputPath({}, "v.markdown")).toBeUndefined();
    });
    test("missing nested key → undefined", () => {
        expect(resolveInputPath({ a: {} }, "v.a.b.c")).toBeUndefined();
    });
    test("non-object intermediate → undefined", () => {
        expect(resolveInputPath({ a: "string" }, "v.a.b")).toBeUndefined();
    });
    test("path not starting with 'v' throws", () => {
        expect(() => resolveInputPath({}, "foo.bar")).toThrow(/must start with 'v'/);
    });
});

// ─── stringifyInput ──────────────────────────────────────────────────

describe("stringifyInput", () => {
    test("string passes through", () => {
        expect(stringifyInput("hello")).toBe("hello");
    });
    test("object → pretty JSON", () => {
        expect(stringifyInput({ a: 1 })).toBe('{\n  "a": 1\n}');
    });
    test("array → pretty JSON", () => {
        expect(stringifyInput([1, 2])).toBe("[\n  1,\n  2\n]");
    });
    test("null → empty string", () => {
        expect(stringifyInput(null)).toBe("");
    });
    test("undefined → empty string", () => {
        expect(stringifyInput(undefined)).toBe("");
    });
    test("number → stringified number", () => {
        expect(stringifyInput(42)).toBe("42");
    });
});
