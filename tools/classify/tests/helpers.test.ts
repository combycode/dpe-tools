/**
 * Unit tests for helpers.ts — positive / negative / false-positive per helper.
 * Matches the matcher.rs discipline for the table-read tool.
 */

import { describe, test, expect } from "bun:test";
import * as H from "../src/helpers";
import { makeBindings } from "../src/helpers";

// ═══════════════════════════════════════════════════════════════════════
// Path / extraction
// ═══════════════════════════════════════════════════════════════════════
describe("split", () => {
    test("splits on single separator", () => {
        expect(H.split("a/b/c", "/")).toEqual(["a", "b", "c"]);
    });
    test("splits on any char in seps", () => {
        expect(H.split("a_b-c.d", "_-.")).toEqual(["a", "b", "c", "d"]);
    });
    test("drops empty segments", () => {
        expect(H.split("//a//b//", "/")).toEqual(["a", "b"]);
    });
    test("empty input → empty array", () => {
        expect(H.split("", "/")).toEqual([]);
    });
    test("no separators in input → single-element array", () => {
        expect(H.split("abc", "/")).toEqual(["abc"]);
    });
});

describe("segment", () => {
    test("positive index", () => {
        expect(H.segment("a/b/c/d", 1)).toBe("b");
    });
    test("negative index = from end", () => {
        expect(H.segment("a/b/c/d", -2)).toBe("c");
    });
    test("custom separator", () => {
        expect(H.segment("a_b_c", 0, "_")).toBe("a");
    });
    test("out of range → empty", () => {
        expect(H.segment("a/b", 5)).toBe("");
    });
    test("trailing slash doesn't break", () => {
        expect(H.segment("a/b/c/", -1)).toBe("c");
    });
});

describe("fileName / fileStem / fileExt", () => {
    test("normal file", () => {
        expect(H.fileName("a/b/report.pdf")).toBe("report.pdf");
        expect(H.fileStem("a/b/report.pdf")).toBe("report");
        expect(H.fileExt("a/b/report.pdf")).toBe("pdf");
    });
    test("file with multi-dot name keeps earlier dots", () => {
        expect(H.fileStem("a.b.c.pdf")).toBe("a.b.c");
        expect(H.fileExt("a.b.c.pdf")).toBe("pdf");
    });
    test("no extension", () => {
        expect(H.fileStem("Makefile")).toBe("Makefile");
        expect(H.fileExt("Makefile")).toBe("");
    });
    test("backslash separator (windows)", () => {
        expect(H.fileName("C:\\a\\b\\c.pdf")).toBe("c.pdf");
    });
    test("hidden file with leading dot is kept whole as stem", () => {
        expect(H.fileStem(".gitignore")).toBe(".gitignore");
    });
    test("case normalized in extension", () => {
        expect(H.fileExt("report.PDF")).toBe("pdf");
    });
});

describe("get (dot-path)", () => {
    test("nested read", () => {
        expect(H.get({ a: { b: { c: 42 } } }, "a.b.c")).toBe(42);
    });
    test("missing path → undefined", () => {
        expect(H.get({ a: {} }, "a.b.c")).toBeUndefined();
    });
    test("empty path → obj itself", () => {
        expect(H.get({ a: 1 }, "")).toEqual({ a: 1 });
    });
    test("null anywhere → undefined", () => {
        expect(H.get(null, "a.b")).toBeNull();
    });
});

describe("words", () => {
    test("default split on whitespace + punct", () => {
        const b = makeBindings([], {});
        expect(H.words("hello world", undefined, b)).toEqual(["hello", "world"]);
        expect(H.words("a,b;c", undefined, b)).toEqual(["a", "b", "c"]);
    });
    test("preserves dict entries as single tokens", () => {
        const b = makeBindings([], { abbr: [["X.YZ"], ["Foo Bar"], ["ACM"]] });
        expect(H.words("X.YZ report", "abbr", b)).toEqual(["X.YZ", "report"]);
    });
    test("dict match is case-insensitive, output preserves source casing", () => {
        const b = makeBindings([], { abbr: [["X.YZ"]] });
        expect(H.words("X.YZ is important", "abbr", b)).toEqual(["X.YZ", "is", "important"]);
    });
    test("longest dict entry wins at a position", () => {
        const b = makeBindings([], { abbr: [["Foo Bar"], ["Online"]] });
        expect(H.words("Foo Bar LTD", "abbr", b)).toEqual(["Foo Bar", "LTD"]);
    });
    test("no dict match at position falls through to default tokenize", () => {
        const b = makeBindings([], { abbr: [["X.YZ"]] });
        expect(H.words("Y.YZ report", "abbr", b)).toEqual(["Y.YZ", "report"]);
    });
    test("unknown dict name → default tokenize", () => {
        const b = makeBindings([], {});
        expect(H.words("a b c", "nonexistent", b)).toEqual(["a", "b", "c"]);
    });
    test("empty input → empty array", () => {
        const b = makeBindings([], {});
        expect(H.words("", undefined, b)).toEqual([]);
    });
});

// ═══════════════════════════════════════════════════════════════════════
// Normalisation
// ═══════════════════════════════════════════════════════════════════════
describe("normalize", () => {
    test("lowercases", () => {
        expect(H.normalize("MixedCase")).toBe("mixedcase");
    });
    test("collapses whitespace", () => {
        expect(H.normalize("Hello   World  ")).toBe("hello world");
    });
    test("strips punctuation", () => {
        expect(H.normalize("A.B-C_D!")).toBe("a b c d");
    });
    test("strips company suffix 'LTD'", () => {
        expect(H.normalize("RENDERING LTD")).toBe("rendering");
    });
    test("strips company suffix 'Limited'", () => {
        expect(H.normalize("Acme Limited")).toBe("acme");
    });
    test("strips s.r.o.", () => {
        expect(H.normalize("Example s.r.o.")).toBe("example");
    });
    test("empty → empty", () => {
        expect(H.normalize("")).toBe("");
    });
    test("preserves cyrillic letters", () => {
        expect(H.normalize("Привет")).toBe("привет");
    });
    test("doesn't strip suffix inside name", () => {
        // "ltd-services" shouldn't strip because suffix is in middle
        expect(H.normalize("ltd-services")).toBe("ltd services");
    });
});

describe("lower", () => {
    test("just lowercases", () => {
        expect(H.lower("Hello World")).toBe("hello world");
    });
    test("no punctuation removal", () => {
        expect(H.lower("A.B")).toBe("a.b");
    });
});

describe("strip", () => {
    test("removes string tokens (case-insensitive)", () => {
        expect(H.strip("Foo.BAR baz CC", ["bar", "cc"])).toBe("Foo. baz");
    });
    test("removes regex matches", () => {
        expect(H.strip("file_20250101_report", [/_\d+_/])).toBe("file report");
    });
    test("empty tokens list → unchanged", () => {
        expect(H.strip("hello", [])).toBe("hello");
    });
    test("empty input", () => {
        expect(H.strip("", ["x"])).toBe("");
    });
});

describe("stripParens", () => {
    test("removes single paren group", () => {
        expect(H.stripParens("name (PZ) rest")).toBe("name rest");
    });
    test("removes multiple", () => {
        expect(H.stripParens("a (b) c (d) e")).toBe("a c e");
    });
    test("no parens → unchanged", () => {
        expect(H.stripParens("no parens")).toBe("no parens");
    });
});

describe("stripWords", () => {
    test("removes dict entries from text", () => {
        const b = makeBindings([], { stop: [["the"], ["a"], ["of"]] });
        expect(H.stripWords("the quick brown fox of the day", "stop", b)).toBe("quick brown fox day");
    });
    test("case-insensitive", () => {
        const b = makeBindings([], { stop: [["LTD"]] });
        expect(H.stripWords("Foo LTD Bar", "stop", b)).toBe("Foo Bar");
    });
    test("word boundary — 'ltd' inside word is not stripped", () => {
        const b = makeBindings([], { stop: [["ltd"]] });
        expect(H.stripWords("Foobar ltdx", "stop", b)).toBe("Foobar ltdx");
    });
    test("missing dict → unchanged", () => {
        const b = makeBindings([], {});
        expect(H.stripWords("x", "missing", b)).toBe("x");
    });
});

describe("replace", () => {
    test("string replace", () => {
        expect(H.replace("a.b.c", ".", "-")).toBe("a-b-c");
    });
    test("regex replace", () => {
        expect(H.replace("abc123def", /\d+/, "#")).toBe("abc#def");
    });
});

// ═══════════════════════════════════════════════════════════════════════
// Matching
// ═══════════════════════════════════════════════════════════════════════
describe("include", () => {
    test("strict exact match", () => {
        const r = H.include("abc", ["ABC", "def"], "strict");
        expect(r.found).toBe(true);
        expect(r.matched).toEqual(["ABC"]);
    });
    test("strict no match", () => {
        expect(H.include("abc", ["def"], "strict").found).toBe(false);
    });
    test("substring match both directions", () => {
        expect(H.include("abc", ["abcdef"], "substring").found).toBe(true);
        expect(H.include("abcdef", ["abc"], "substring").found).toBe(true);
    });
    test("fuzzy:1 tolerates one typo", () => {
        const r = H.include("rendering", ["rendring"], "fuzzy:1");
        expect(r.found).toBe(true);
    });
    test("fuzzy:1 rejects 2 typos", () => {
        expect(H.include("rendering", ["redring"], "fuzzy:1").found).toBe(false);
    });
    test("fuzzy default 2 via just 'fuzzy'", () => {
        expect(H.include("rendering", ["redring"], "fuzzy").found).toBe(true);
    });
    test("empty inputs safe", () => {
        expect(H.include("", ["a"], "strict").found).toBe(false);
        expect(H.include("a", [], "strict").found).toBe(false);
    });
});

describe("match", () => {
    test("matches regex pattern", () => {
        expect(H.match("report_2025_01.pdf", [/\d{4}_\d{2}/])).toBe("2025_01");
    });
    test("matches string substring when fuzzy=0", () => {
        expect(H.match("acm report", ["acm", "other"])).toBe("acm");
    });
    test("matches fuzzy string", () => {
        expect(H.match("rendering", ["rendring"], 1)).toBe("rendring");
    });
    test("no match → null", () => {
        expect(H.match("foo", [/bar/])).toBeNull();
    });
});

describe("contains / hasPrefix / hasSuffix", () => {
    test("contains — any word present", () => {
        expect(H.contains("hello world", ["foo", "world"])).toBe(true);
        expect(H.contains("hello world", ["foo", "bar"])).toBe(false);
    });
    test("hasPrefix", () => {
        expect(H.hasPrefix("archive/x.xlsx", ["archive/"])).toBe(true);
        expect(H.hasPrefix("archive/x.xlsx", ["items/"])).toBe(false);
    });
    test("hasSuffix", () => {
        expect(H.hasSuffix("report.pdf", [".pdf", ".docx"])).toBe(true);
        expect(H.hasSuffix("report.pdf", [".csv"])).toBe(false);
    });
});

describe("closest / editDistance", () => {
    test("exact match → distance 0", () => {
        expect(H.editDistance("abc", "abc")).toBe(0);
    });
    test("one insertion", () => {
        expect(H.editDistance("abc", "abcd")).toBe(1);
    });
    test("one deletion", () => {
        expect(H.editDistance("abcd", "abc")).toBe(1);
    });
    test("one substitution", () => {
        expect(H.editDistance("abc", "abd")).toBe(1);
    });
    test("closest finds nearest within threshold", () => {
        expect(H.closest("rendering", ["rendring", "wording", "other"], 1)).toBe("rendring");
    });
    test("closest returns exact first", () => {
        expect(H.closest("x", ["y", "x", "xy"], 2)).toBe("x");
    });
    test("closest rejects over-threshold", () => {
        expect(H.closest("abcdef", ["xyz"], 2)).toBeNull();
    });
    test("closest with empty list → null", () => {
        expect(H.closest("x", [], 2)).toBeNull();
    });
});

// ═══════════════════════════════════════════════════════════════════════
// Numeric
// ═══════════════════════════════════════════════════════════════════════
describe("inRange", () => {
    test("inclusive bounds", () => {
        expect(H.inRange(5, 1, 10)).toBe(true);
        expect(H.inRange(1, 1, 10)).toBe(true);
        expect(H.inRange(10, 1, 10)).toBe(true);
    });
    test("below → false", () => {
        expect(H.inRange(0, 1, 10)).toBe(false);
    });
    test("above → false", () => {
        expect(H.inRange(11, 1, 10)).toBe(false);
    });
    test("non-number → false", () => {
        expect(H.inRange("5", 1, 10)).toBe(false);
        expect(H.inRange(null, 1, 10)).toBe(false);
        expect(H.inRange(NaN, 1, 10)).toBe(false);
    });
});

describe("parseNumber", () => {
    test("integer string", () => {
        expect(H.parseNumber("42")).toBe(42);
    });
    test("decimal string", () => {
        expect(H.parseNumber("3.14")).toBe(3.14);
    });
    test("commas and spaces stripped", () => {
        expect(H.parseNumber("1,234,567")).toBe(1234567);
    });
    test("already a number", () => {
        expect(H.parseNumber(7)).toBe(7);
    });
    test("invalid → null", () => {
        expect(H.parseNumber("abc")).toBeNull();
        expect(H.parseNumber(null)).toBeNull();
    });
});

// ═══════════════════════════════════════════════════════════════════════
// Dictionaries
// ═══════════════════════════════════════════════════════════════════════
describe("legacy flat-dict helpers (alias / aliasReverse / lookup / dict)", () => {
    const aliases: string[][] = [
        ["acm", "Acme"],
        ["brv", "Bravo"],
        ["foo bar", "FooBar"],
    ];

    test("alias col0 → col1 case-insensitive", () => {
        const b = makeBindings([], { aliases });
        expect(H.alias("aliases", "ACM", b)).toBe("Acme");
        expect(H.alias("aliases", "FOO BAR", b)).toBe("FooBar");
    });

    test("alias missing returns null", () => {
        const b = makeBindings([], { aliases });
        expect(H.alias("aliases", "nope", b)).toBeNull();
    });

    test("aliasReverse col1 → col0", () => {
        const b = makeBindings([], { aliases });
        expect(H.aliasReverse("aliases", "Acme", b)).toBe("acm");
    });

    test("lookup with custom indices", () => {
        const extra: string[][] = [["row1c0", "row1c1", "row1c2"]];
        const b = makeBindings([], { extra });
        expect(H.lookup("extra", "row1c1", { key: 1, value: 2 }, b)).toBe("row1c2");
    });

    test("missing dict handled safely", () => {
        const b = makeBindings([], {});
        expect(H.alias("missing", "x", b)).toBeNull();
        expect(H.dict("missing", b)).toBeNull();
    });
});

// ═══════════════════════════════════════════════════════════════════════
// New dict helpers — keys / values / entry / members / pairs
// ═══════════════════════════════════════════════════════════════════════
describe("keys / values / entry / members / pairs (flat)", () => {
    const classesList = ["foo", "Bar", "Baz"];
    const aliasesTSV: string[][] = [
        ["acm", "Acme"],
        ["brv", "Bravo"],
        ["foo bar", "FooBar"],
    ];
    const example: string[][] = [
        ["A", "B", "F"],
        ["C", "D"],
        ["E", "B"],
    ];

    test("keys: primary-key column (col 0), deduped", () => {
        const b = makeBindings([], { aliases: aliasesTSV });
        expect(H.keys("aliases", b)).toEqual(["acm", "brv", "foo bar"]);
    });

    test("values(name, colIdx) — all unique values in column", () => {
        const b = makeBindings([], { aliases: aliasesTSV });
        expect(H.valuesImpl("aliases", [0], b)).toEqual(["acm", "brv", "foo bar"]);
        expect(H.valuesImpl("aliases", [1], b)).toEqual(["Acme", "Bravo", "FooBar"]);
    });

    test("values(name, key, colIdx) — records matching primary key", () => {
        const b = makeBindings([], { aliases: aliasesTSV });
        expect(H.valuesImpl("aliases", ["acm", 1], b)).toEqual(["Acme"]);
        expect(H.valuesImpl("aliases", ["nope", 1], b)).toEqual([]);
    });

    test("entry returns the matching row as array", () => {
        const b = makeBindings([], { aliases: aliasesTSV });
        expect(H.entry("aliases", "acm", b)).toEqual(["acm", "Acme"]);
        expect(H.entry("aliases", "nope", b)).toBeNull();
    });

    test("members(name, colIdx, value) — all rows matching", () => {
        const multi: string[][] = [
            ["ACM", "Charlie"],
            ["ACM", "Alpha"],
            ["BRV", "Beta"],
        ];
        const b = makeBindings([], { m: multi });
        const acm = H.members("m", 0, "ACM", b);
        expect(acm.length).toBe(2);
        expect(acm).toEqual([
            ["ACM", "Charlie"],
            ["ACM", "Alpha"],
        ]);
    });

    test("pairs(name, key) — rows containing key, non-key cells deduped", () => {
        const b = makeBindings([], { ex: example });
        expect(H.pairs("ex", "A", undefined, b)).toEqual(["B", "F"]);
        expect(H.pairs("ex", "B", undefined, b)).toEqual(["A", "F", "E"]); // row-major
    });

    test("pairs(name, key, col) — specific column from matching rows", () => {
        const b = makeBindings([], { ex: example });
        expect(H.pairs("ex", "A", 1, b)).toEqual(["B"]);
        expect(H.pairs("ex", "B", 0, b)).toEqual(["A", "E"]);
    });

    test("missing dict → empty/null", () => {
        const b = makeBindings([], {});
        expect(H.keys("nope", b)).toEqual([]);
        expect(H.valuesImpl("nope", [0], b)).toEqual([]);
        expect(H.entry("nope", "x", b)).toBeNull();
        expect(H.members("nope", 0, "x", b)).toEqual([]);
        expect(H.pairs("nope", "x", undefined, b)).toEqual([]);
    });
});

describe("keys / values / entry / members / pairs (obj dict)", () => {
    const vendors: H.ObjDict = {
        kind: "obj",
        keys: ["Acme", "Alpha", "Bravo"],
        entries: {
            Acme: { group: "ACM", points: [1, 2, 3, 4], labels: ["hardware", "software"] },
            Alpha: { group: "ACM", points: [2, 5], labels: ["software"] },
            Bravo: { group: "BRV", points: [6, 7], labels: ["services"] },
        },
    };

    test("keys returns top-level object keys", () => {
        const b = makeBindings([], { vendors });
        expect(H.keys("vendors", b)).toEqual(["Acme", "Alpha", "Bravo"]);
    });

    test("values(name, field) — unique flattened across all records", () => {
        const b = makeBindings([], { vendors });
        expect(H.valuesImpl("vendors", ["group"], b)).toEqual(["ACM", "BRV"]);
        expect(H.valuesImpl("vendors", ["points"], b)).toEqual([1, 2, 3, 4, 5, 6, 7]);
        expect(H.valuesImpl("vendors", ["labels"], b)).toEqual(["hardware", "software", "services"]);
    });

    test("values(name, key, field) — single record's field flattened", () => {
        const b = makeBindings([], { vendors });
        expect(H.valuesImpl("vendors", ["Acme", "points"], b)).toEqual([1, 2, 3, 4]);
    });

    test("values(name, filterField, filterValue, collectField) — filter + collect", () => {
        const b = makeBindings([], { vendors });
        // All points across ACM group members (Acme + Alpha)
        const pts = H.valuesImpl("vendors", ["group", "ACM", "points"], b);
        expect(pts.sort((a, b) => (a as number) - (b as number))).toEqual([1, 2, 3, 4, 5]);
    });

    test("entry returns the full record object", () => {
        const b = makeBindings([], { vendors });
        expect(H.entry("vendors", "Acme", b)).toEqual(vendors.entries.Acme);
        expect(H.entry("vendors", "Nope", b)).toBeNull();
    });

    test("members(name, field, value) — records matching field", () => {
        const b = makeBindings([], { vendors });
        const acm = H.members("vendors", "group", "ACM", b);
        expect(acm.length).toBe(2);
        expect((acm[0] as any).group).toBe("ACM");
    });

    test("CLASS with entry as data (enrichment flow)", () => {
        const b = makeBindings([], { vendors });
        const record = H.entry("vendors", "Acme", b);
        const cls = H.CLASS("Acme", record as object);
        expect(cls.class).toBe("Acme");
        expect((cls.data as any).group).toBe("ACM");
    });
});

// ═══════════════════════════════════════════════════════════════════════
// Predicates / structural
// ═══════════════════════════════════════════════════════════════════════
describe("empty / has / uniq / count", () => {
    test("empty — string/array/object", () => {
        expect(H.empty(null)).toBe(true);
        expect(H.empty(undefined)).toBe(true);
        expect(H.empty("")).toBe(true);
        expect(H.empty([])).toBe(true);
        expect(H.empty({})).toBe(true);
        expect(H.empty(" ")).toBe(false);
        expect(H.empty([0])).toBe(false);
        expect(H.empty({ a: 1 })).toBe(false);
        expect(H.empty(0)).toBe(false); // zero is not empty
    });

    test("has existence", () => {
        expect(H.has({ a: { b: 1 } }, "a.b")).toBe(true);
        expect(H.has({ a: {} }, "a.b")).toBe(false);
    });

    test("uniq removes duplicates preserving first order", () => {
        expect(H.uniq([1, 2, 1, 3, 2])).toEqual([1, 2, 3]);
    });

    test("count with and without pred", () => {
        expect(H.count([1, 2, 3])).toBe(3);
        expect(H.count([1, 2, 3, 4], (x) => x > 2)).toBe(2);
    });
});

// ═══════════════════════════════════════════════════════════════════════
// Result shorthand
// ═══════════════════════════════════════════════════════════════════════
describe("CLASS / NEW / UNKNOWN", () => {
    test("CLASS with data only", () => {
        expect(H.CLASS("Foo", { extra: 1 })).toEqual({ class: "Foo", data: { extra: 1 } });
    });
    test("CLASS with explicit confidence", () => {
        expect(H.CLASS("Foo", 85, { x: 1 })).toEqual({ class: "Foo", confidence: 85, data: { x: 1 } });
    });
    test("NEW with token and reason", () => {
        expect(H.NEW("candidate", "why")).toEqual({ new: "candidate", reason: "why" });
    });
    test("UNKNOWN with reason", () => {
        expect(H.UNKNOWN("no-rule-matched")).toEqual({ unknown: true, reason: "no-rule-matched" });
    });
});
