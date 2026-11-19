/**
 * Integration tests for dpe-tool-filter.
 * Spawn the tool, feed NDJSON stdin, assert on stdout/stderr.
 */
import { describe, test, expect } from "bun:test";
import { spawn } from "node:child_process";
import { join } from "node:path";

const TOOL = join(import.meta.dir, "..", "src", "main.ts");

function runTool(settings: unknown, stdinLines: unknown[]): Promise<{ stdout: any[]; stderr: string; code: number }> {
    const stdin = stdinLines.map((l) => JSON.stringify(l)).join("\n") + "\n";
    return new Promise((resolve) => {
        const child = spawn("bun", [TOOL, JSON.stringify(settings)], { stdio: ["pipe", "pipe", "pipe"] });
        let stdout = "",
            stderr = "";
        child.stdout.on("data", (d) => {
            stdout += d.toString();
        });
        child.stderr.on("data", (d) => {
            stderr += d.toString();
        });
        child.on("close", (code) => {
            const out = stdout
                .split("\n")
                .filter((l) => l.trim())
                .map((l) => JSON.parse(l));
            resolve({ stdout: out, stderr, code: code ?? 0 });
        });
        child.stdin.write(stdin);
        child.stdin.end();
    });
}

function dataEnv(v: unknown, id = "i", src = "s") {
    return { t: "d", id, src, v };
}
function metaEnv(v: unknown) {
    return { t: "m", v };
}
function dataOut(out: any[]) {
    return out.filter((e) => e.t === "d");
}
function metaOut(out: any[]) {
    return out.filter((e) => e.t === "m" && e.v.tool !== "filter"); // skip the shutdown meta
}

// ═══ 1. Filter-only ═══════════════════════════════════════════════════
describe("filter", () => {
    test("drops rows where predicate is false", async () => {
        const { stdout } = await runTool({ ops: [{ filter: "item.x >= 0" }] }, [
            dataEnv({ row: { x: 5 } }),
            dataEnv({ row: { x: -1 } }),
            dataEnv({ row: { x: 10 } }),
        ]);
        const data = dataOut(stdout);
        expect(data.length).toBe(2);
        expect(data.map((d) => d.v.row.x)).toEqual([5, 10]);
    });

    test("truthy non-boolean passes the filter", async () => {
        const { stdout } = await runTool({ ops: [{ filter: "item.name" }] }, [
            dataEnv({ row: { name: "Alice" } }),
            dataEnv({ row: { name: "" } }),
            dataEnv({ row: { name: "Bob" } }),
        ]);
        expect(dataOut(stdout).length).toBe(2);
    });

    test("filter with return body", async () => {
        const { stdout } = await runTool({ ops: [{ filter: "const ok = item.x > 2; return ok;" }] }, [
            dataEnv({ row: { x: 1 } }),
            dataEnv({ row: { x: 3 } }),
        ]);
        expect(dataOut(stdout).length).toBe(1);
    });
});

// ═══ 2. Map-only ══════════════════════════════════════════════════════
describe("map", () => {
    test("transforms each item", async () => {
        const { stdout } = await runTool({ ops: [{ map: "({id: item.id, doubled: item.x * 2})" }] }, [
            dataEnv({ row: { id: 1, x: 5 } }),
            dataEnv({ row: { id: 2, x: 10 } }),
        ]);
        const data = dataOut(stdout);
        expect(data[0].v.row).toEqual({ id: 1, doubled: 10 });
        expect(data[1].v.row).toEqual({ id: 2, doubled: 20 });
    });

    test("map returning null drops the row", async () => {
        const { stdout } = await runTool({ ops: [{ map: "item.x > 0 ? item : null" }] }, [
            dataEnv({ row: { x: 5 } }),
            dataEnv({ row: { x: -1 } }),
            dataEnv({ row: { x: 10 } }),
        ]);
        const data = dataOut(stdout);
        expect(data.length).toBe(2);
    });

    test("map returning undefined also drops", async () => {
        const { stdout } = await runTool({ ops: [{ map: "item.x > 0 ? item : undefined" }] }, [
            dataEnv({ row: { x: 1 } }),
            dataEnv({ row: { x: 0 } }),
        ]);
        expect(dataOut(stdout).length).toBe(1);
    });

    test("map with multi-line body", async () => {
        const script = "const sum = item.a + item.b; return { total: sum };";
        const { stdout } = await runTool({ ops: [{ map: script }] }, [dataEnv({ row: { a: 3, b: 4 } })]);
        expect(dataOut(stdout)[0].v.row).toEqual({ total: 7 });
    });
});

// ═══ 3. Ordering via ops array ════════════════════════════════════════
describe("ops order", () => {
    test("filter-then-map: map operates on filtered survivors", async () => {
        const { stdout } = await runTool({ ops: [{ filter: "item.x > 0" }, { map: "({doubled: item.x * 2})" }] }, [
            dataEnv({ row: { x: 1 } }),
            dataEnv({ row: { x: -1 } }),
            dataEnv({ row: { x: 2 } }),
        ]);
        const data = dataOut(stdout);
        expect(data.map((d) => d.v.row.doubled)).toEqual([2, 4]);
    });

    test("map-then-filter: filter sees mapped shape", async () => {
        const { stdout } = await runTool(
            { ops: [{ map: "({doubled: item.x * 2})" }, { filter: "item.doubled >= 4" }] },
            [
                dataEnv({ row: { x: 1 } }), // doubled=2 → drop
                dataEnv({ row: { x: 2 } }), // doubled=4 → keep
                dataEnv({ row: { x: 3 } }), // doubled=6 → keep
            ],
        );
        const data = dataOut(stdout);
        expect(data.map((d) => d.v.row.doubled)).toEqual([4, 6]);
    });

    test("chain of filter-map-filter works", async () => {
        const { stdout } = await runTool(
            { ops: [{ filter: "item.x > 0" }, { map: "({sq: item.x * item.x})" }, { filter: "item.sq < 20" }] },
            [
                dataEnv({ row: { x: -1 } }), // first filter drops
                dataEnv({ row: { x: 2 } }), // 4 → keep
                dataEnv({ row: { x: 4 } }), // 16 → keep
                dataEnv({ row: { x: 5 } }), // 25 → drop
            ],
        );
        const data = dataOut(stdout);
        expect(data.map((d) => d.v.row.sq)).toEqual([4, 16]);
    });
});

// ═══ 4. apply_to matrix ═══════════════════════════════════════════════
describe("apply_to", () => {
    test('"data" processes data, drops meta', async () => {
        const { stdout } = await runTool({ apply_to: "data", ops: [{ map: "({x: item.x})" }] }, [
            dataEnv({ row: { x: 1, extra: "skip" } }),
            metaEnv({ summary: true }),
            dataEnv({ row: { x: 2, extra: "skip" } }),
        ]);
        expect(dataOut(stdout).length).toBe(2);
        expect(metaOut(stdout).length).toBe(0); // meta dropped
    });

    test('"meta" processes meta, drops data', async () => {
        const { stdout } = await runTool(
            {
                apply_to: "meta",
                target: "summary",
                ops: [{ map: "({ n: item.n })" }],
            },
            [dataEnv({ row: { x: 1 } }), metaEnv({ summary: { n: 5, extra: "skip" } })],
        );
        expect(dataOut(stdout).length).toBe(0);
        const metas = metaOut(stdout);
        expect(metas.length).toBe(1);
        expect(metas[0].v.summary).toEqual({ n: 5 });
    });

    test('"both" exposes env.t for branching', async () => {
        const expr = 'env.t === "d" ? ({doubled: item.x*2}) : ({...item, processed: true})';
        const { stdout } = await runTool({ apply_to: "both", ops: [{ map: expr }] }, [
            dataEnv({ row: { x: 3 } }),
            metaEnv({ rows: 10 }),
        ]);
        const data = dataOut(stdout);
        const metas = metaOut(stdout);
        expect(data[0].v.row).toEqual({ doubled: 6 });
        expect(metas[0].v).toEqual({ rows: 10, processed: true });
    });

    test('default is "data"', async () => {
        const { stdout } = await runTool(
            { ops: [{ map: "item" }] }, // no apply_to
            [dataEnv({ row: { x: 1 } }), metaEnv({ rows: 5 })],
        );
        expect(dataOut(stdout).length).toBe(1);
        expect(metaOut(stdout).length).toBe(0);
    });
});

// ═══ 5. target ═════════════════════════════════════════════════════════
describe("target", () => {
    test('default target is "row"', async () => {
        const { stdout } = await runTool({ ops: [{ filter: "item.x > 0" }] }, [
            dataEnv({ row: { x: 1 }, file: "a.ndjson" }),
            dataEnv({ row: { x: -1 }, file: "b.ndjson" }),
        ]);
        // Second dropped; outer .file field preserved for the survivor
        const data = dataOut(stdout);
        expect(data.length).toBe(1);
        expect(data[0].v.file).toBe("a.ndjson");
    });

    test('target="v" operates on the full v', async () => {
        const { stdout } = await runTool({ target: "v", ops: [{ filter: "item.file === 'keep.ndjson'" }] }, [
            dataEnv({ row: { x: 1 }, file: "keep.ndjson" }),
            dataEnv({ row: { x: 2 }, file: "drop.ndjson" }),
        ]);
        // target="v" doesn't match any key in v, so item falls back to v itself
        expect(dataOut(stdout).length).toBe(1);
    });

    test("map replaces only v[target], preserves other v fields", async () => {
        const { stdout } = await runTool({ ops: [{ map: "({renamed: item.original})" }] }, [
            dataEnv({ row: { original: 42 }, file: "x.ndjson", sheet: "S1" }),
        ]);
        const out = dataOut(stdout)[0];
        expect(out.v.row).toEqual({ renamed: 42 });
        expect(out.v.file).toBe("x.ndjson");
        expect(out.v.sheet).toBe("S1");
    });
});

// ═══ 6. helpers exposed to expressions ════════════════════════════════
describe("helpers", () => {
    test("hash() available", async () => {
        const { stdout } = await runTool({ ops: [{ map: "({h: hash(item.id)})" }] }, [
            dataEnv({ row: { id: "hello" } }),
        ]);
        const h = dataOut(stdout)[0].v.row.h;
        expect(h).toMatch(/^[0-9a-f]{16}$/);
    });

    test("Math / JSON / Date / RegExp available", async () => {
        const expr = "({abs: Math.abs(item.x), len: JSON.stringify(item).length, hasA: /a/.test(item.name)})";
        const { stdout } = await runTool({ ops: [{ map: expr }] }, [dataEnv({ row: { x: -5, name: "alice" } })]);
        const out = dataOut(stdout)[0].v.row;
        expect(out.abs).toBe(5);
        expect(typeof out.len).toBe("number");
        expect(out.hasA).toBe(true);
    });

    test("now() returns a number", async () => {
        const { stdout } = await runTool({ ops: [{ map: "({ts: now()})" }] }, [dataEnv({ row: {} })]);
        const ts = dataOut(stdout)[0].v.row.ts;
        expect(typeof ts).toBe("number");
        expect(ts).toBeGreaterThan(0);
    });

    test("env.id / env.src accessible in expression", async () => {
        const { stdout } = await runTool({ ops: [{ map: "({src: env.src, id: env.id})" }] }, [
            dataEnv({ row: { x: 1 } }, "myid", "mysrc"),
        ]);
        const out = dataOut(stdout)[0].v.row;
        expect(out.id).toBe("myid");
        expect(out.src).toBe("mysrc");
    });
});

// ═══ 7. envelope id/src preservation ══════════════════════════════════
describe("envelope metadata", () => {
    test("id and src pass through unchanged", async () => {
        const { stdout } = await runTool({ ops: [{ map: "({x: item.x * 2})" }] }, [
            dataEnv({ row: { x: 5 } }, "abc123", "upstream:42"),
        ]);
        const out = dataOut(stdout)[0];
        expect(out.id).toBe("abc123");
        expect(out.src).toBe("upstream:42");
    });
});

// ═══ 8. Errors ════════════════════════════════════════════════════════
describe("errors", () => {
    test("runtime error in expression → stderr, stream continues", async () => {
        const { stdout, stderr } = await runTool(
            { ops: [{ filter: "item.missing.nested >= 0" }] }, // throws on null deref
            [
                dataEnv({ row: { x: 1 } }), // will throw
                dataEnv({ row: { missing: { nested: 5 } } }), // ok
            ],
        );
        const data = dataOut(stdout);
        expect(data.length).toBe(1);
        expect(stderr).toContain("error");
    });

    test("malformed op in settings → fatal, non-zero exit", async () => {
        const { code, stderr } = await runTool(
            { ops: [{ filter: "x", map: "y" }] }, // both in same op
            [dataEnv({ row: {} })],
        );
        expect(code).not.toBe(0);
        expect(stderr).toContain("filter");
    });

    test("invalid apply_to → fatal", async () => {
        const { code } = await runTool({ apply_to: "elsewhere", ops: [] }, [dataEnv({ row: {} })]);
        expect(code).not.toBe(0);
    });

    test("syntax error in expression → fatal at startup", async () => {
        const { code } = await runTool(
            { ops: [{ filter: "item.x ><" }] }, // syntax error
            [dataEnv({ row: {} })],
        );
        expect(code).not.toBe(0);
    });
});

// ═══ 9. edge cases ════════════════════════════════════════════════════
describe("edge cases", () => {
    test("empty ops → passes all data through unchanged", async () => {
        const { stdout } = await runTool({ ops: [] }, [dataEnv({ row: { x: 1 } }), dataEnv({ row: { x: 2 } })]);
        expect(dataOut(stdout).length).toBe(2);
    });

    test("no settings at all → empty behavior (pass data through)", async () => {
        const { stdout } = await runTool({}, [dataEnv({ row: { x: 1 } })]);
        expect(dataOut(stdout).length).toBe(1);
    });

    test("shutdown meta has tool:'filter' and counts", async () => {
        const { stdout } = await runTool({ ops: [{ filter: "item.x > 0" }] }, [
            dataEnv({ row: { x: 1 } }),
            dataEnv({ row: { x: -1 } }),
            dataEnv({ row: { x: 2 } }),
        ]);
        const shutdownMeta = stdout.find((e) => e.t === "m" && e.v.tool === "filter");
        expect(shutdownMeta).toBeDefined();
        expect(shutdownMeta.v.data_in).toBe(3);
        expect(shutdownMeta.v.dropped_filter).toBe(1);
        expect(shutdownMeta.v.emitted).toBe(2);
    });
});
