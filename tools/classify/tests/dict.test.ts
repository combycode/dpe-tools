import { describe, test, expect, afterAll } from "bun:test";
import { loadDict, loadList } from "../src/dict";
import { writeFileSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const DIR = mkdtempSync(join(tmpdir(), "dpe-classify-dict-"));
function file(name: string, content: string): string {
    const p = join(DIR, name);
    writeFileSync(p, content);
    return p;
}
afterAll(() => {
    rmSync(DIR, { recursive: true, force: true });
});

describe("loadDict — flat (.txt / .tsv / .csv)", () => {
    test("single-column .txt", () => {
        const p = file("classes.txt", "alpha\nbeta\ngamma\n");
        expect(loadDict(p)).toEqual({ kind: "flat", rows: [["alpha"], ["beta"], ["gamma"]] });
    });

    test("TSV multi-column", () => {
        const p = file("aliases.tsv", "acm\tAcme\nbrv\tBravo\n");
        expect(loadDict(p)).toEqual({
            kind: "flat",
            rows: [
                ["acm", "Acme"],
                ["brv", "Bravo"],
            ],
        });
    });

    test("' : ' colon separator auto-detected in .txt", () => {
        const p = file("a.txt", "key : value\nanother : one\n");
        expect(loadDict(p)).toEqual({
            kind: "flat",
            rows: [
                ["key", "value"],
                ["another", "one"],
            ],
        });
    });

    test("blank and # comment lines ignored", () => {
        const p = file("c.txt", "# comment\n\nalpha\n\n# another\nbeta\n");
        expect(loadDict(p)).toEqual({ kind: "flat", rows: [["alpha"], ["beta"]] });
    });

    test("BOM stripped", () => {
        const p = file("bom.txt", "\uFEFFalpha\nbeta\n");
        expect(loadDict(p)).toEqual({ kind: "flat", rows: [["alpha"], ["beta"]] });
    });

    test("3-column TSV", () => {
        const p = file("multi.tsv", "a\tb\tc\nd\te\tf\n");
        expect(loadDict(p)).toEqual({
            kind: "flat",
            rows: [
                ["a", "b", "c"],
                ["d", "e", "f"],
            ],
        });
    });

    test("internal spaces preserved in TSV cells", () => {
        const p = file("sp.tsv", "Foo Bar\tFooBar\n");
        expect(loadDict(p)).toEqual({ kind: "flat", rows: [["Foo Bar", "FooBar"]] });
    });
});

describe("loadDict — obj (.json)", () => {
    test("top-level object → obj dict", () => {
        const p = file(
            "m.json",
            JSON.stringify({
                Acme: { group: "ACM", points: [1, 2, 3] },
                Bravo: { group: "BRV", points: [4, 5] },
            }),
        );
        const d = loadDict(p);
        expect(d.kind).toBe("obj");
        if (d.kind !== "obj") return;
        expect(d.keys).toEqual(["Acme", "Bravo"]);
        expect(d.entries.Acme).toEqual({ group: "ACM", points: [1, 2, 3] });
    });

    test("top-level array throws (guide to use jsonl)", () => {
        const p = file("arr.json", JSON.stringify([{ id: "a" }]));
        expect(() => loadDict(p)).toThrow(/jsonl/);
    });

    test("preserves scalar / string / mixed values", () => {
        const p = file("mixed.json", JSON.stringify({ a: "hello", b: 42, c: { nested: true } }));
        const d = loadDict(p);
        if (d.kind !== "obj") throw new Error("expected obj");
        expect(d.entries.a).toBe("hello");
        expect(d.entries.b).toBe(42);
        expect(d.entries.c).toEqual({ nested: true });
    });
});

describe("loadDict — jsonl (.jsonl / .ndjson)", () => {
    test("one record per line keyed by 'id' (default)", () => {
        const p = file("t.jsonl", JSON.stringify({ id: "A", x: 1 }) + "\n" + JSON.stringify({ id: "B", x: 2 }) + "\n");
        const d = loadDict(p);
        if (d.kind !== "obj") throw new Error("expected obj");
        expect(d.keys).toEqual(["A", "B"]);
        expect(d.entries.A).toEqual({ id: "A", x: 1 });
    });

    test("custom key field", () => {
        const p = file(
            "t2.jsonl",
            JSON.stringify({ name: "Foo", n: 1 }) + "\n" + JSON.stringify({ name: "Bar", n: 2 }) + "\n",
        );
        const d = loadDict(p, { key: "name" });
        if (d.kind !== "obj") throw new Error("expected obj");
        expect(d.keys).toEqual(["Foo", "Bar"]);
    });

    test("blank lines and # comments ignored", () => {
        const p = file("t3.jsonl", "# comment\n\n" + JSON.stringify({ id: "A" }) + "\n");
        const d = loadDict(p);
        if (d.kind !== "obj") throw new Error("expected obj");
        expect(d.keys).toEqual(["A"]);
    });

    test("missing key field throws with line number", () => {
        const p = file("t4.jsonl", JSON.stringify({ nokey: 1 }) + "\n");
        expect(() => loadDict(p)).toThrow(/key field 'id'/);
    });

    test(".ndjson extension also works", () => {
        const p = file("t.ndjson", JSON.stringify({ id: "X" }) + "\n");
        const d = loadDict(p);
        expect(d.kind).toBe("obj");
    });
});

describe("loadList (legacy)", () => {
    test("returns col 0 of a flat dict", () => {
        const p = file("list.txt", "alpha\nbeta\ngamma\n");
        expect(loadList(p)).toEqual(["alpha", "beta", "gamma"]);
    });
});
