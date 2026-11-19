/**
 * Unit tests for rule compilation and execution.
 * Uses the new ClassificationResult shape: {className, category, ...}.
 */

import { describe, test, expect } from "bun:test";
import { compileExpression, compileRule, runRules, buildBindings } from "../src/rules";
import { makeBindings } from "../src/helpers";

function makeCtx(item: unknown, env: unknown = {}, classes: string[] = [], dicts: Record<string, string[][]> = {}) {
    const helpers = makeBindings(classes, dicts);
    return { bindings: buildBindings(item, env, helpers), helpers };
}

describe("compileExpression", () => {
    test("auto-wrap in return when no 'return' keyword", () => {
        const fn = compileExpression("item.x + 1");
        const { bindings } = makeCtx({ x: 5 });
        expect(fn(bindings) as unknown).toBe(6);
    });

    test("uses body verbatim when 'return' present", () => {
        const fn = compileExpression("const t = item.x * 2; return t;");
        const { bindings } = makeCtx({ x: 4 });
        expect(fn(bindings) as unknown).toBe(8);
    });

    test("exposes normalize helper", () => {
        const fn = compileExpression("normalize(item)");
        const { bindings } = makeCtx("  RENDERING LTD ");
        expect(fn(bindings)).toBe("rendering");
    });

    test("exposes classes binding", () => {
        const fn = compileExpression("classes.length");
        const { bindings } = makeCtx({}, {}, ["a", "b", "c"]);
        expect(fn(bindings) as unknown).toBe(3);
    });

    test("exposes new dict helpers (alias still works for flat)", () => {
        const fn = compileExpression("alias('abbr', item)");
        const { bindings } = makeCtx("acm", {}, [], { abbr: [["acm", "Acme"]] });
        expect(fn(bindings)).toBe("Acme");
    });
});

describe("runRules — category derivation", () => {
    test("CLASS(x) where x ∈ classes → category FOUND", () => {
        const rules = [compileRule({ name: "r1", confidence: 80, expr: "'Alice'" })];
        const { bindings, helpers } = makeCtx({}, {}, ["Alice", "Bob"]);
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBe("Alice");
        expect(r.category).toBe("FOUND");
        expect(r.confidence).toBe(80);
    });

    test("CLASS(x) where x ∉ classes → category NEW (auto-upgrade)", () => {
        const rules = [compileRule({ name: "r1", confidence: 100, expr: "'StrangerDanger'" })];
        const { bindings, helpers } = makeCtx({}, {}, ["Alice", "Bob"]);
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBe("StrangerDanger");
        expect(r.category).toBe("NEW");
    });

    test("NEW(x) → category NEW, className = x", () => {
        const rules = [compileRule({ name: "r1", confidence: 80, expr: "NEW('Candidate', 'because reasons')" })];
        const { bindings, helpers } = makeCtx({}, {}, ["Alice"]);
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBe("Candidate");
        expect(r.category).toBe("NEW");
    });

    test("UNKNOWN() → className null, category UNKNOWN", () => {
        const rules = [compileRule({ name: "r1", confidence: 100, expr: "UNKNOWN('reason')" })];
        const { bindings, helpers } = makeCtx({});
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBeNull();
        expect(r.category).toBe("UNKNOWN");
    });

    test("bare 'UNKNOWN' string → className null, category UNKNOWN", () => {
        const rules = [compileRule({ name: "r1", confidence: 100, expr: "'UNKNOWN'" })];
        const { bindings, helpers } = makeCtx({});
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBeNull();
        expect(r.category).toBe("UNKNOWN");
    });

    test("no rule matched → className null, category UNKNOWN", () => {
        const rules = [compileRule({ name: "r1", confidence: 100, expr: "null" })];
        const { bindings, helpers } = makeCtx({});
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBeNull();
        expect(r.category).toBe("UNKNOWN");
        expect(r.reason).toBe("no-rule-matched");
    });

    test("explicit category in CLASS(...) is preserved", () => {
        const rules = [
            compileRule({
                name: "r1",
                confidence: 100,
                expr: "({class: 'Foo', category: 'NEW', confidence: 90})",
            }),
        ];
        const { bindings, helpers } = makeCtx({}, {}, ["Foo"]); // Foo would be FOUND
        const r = runRules(rules, bindings, helpers);
        expect(r.category).toBe("NEW"); // explicit override wins
    });
});

describe("runRules — ordering and best-match", () => {
    test("100% short-circuits", () => {
        const rules = [
            compileRule({ name: "r1", confidence: 100, expr: "'First'" }),
            compileRule({ name: "r2", confidence: 100, expr: "'Second'" }),
        ];
        const { bindings, helpers } = makeCtx({}, {}, ["First", "Second"]);
        expect(runRules(rules, bindings, helpers).className).toBe("First");
    });

    test("below 100% keeps best", () => {
        const rules = [
            compileRule({ name: "r1", confidence: 50, expr: "'Low'" }),
            compileRule({ name: "r2", confidence: 80, expr: "'Mid'" }),
            compileRule({ name: "r3", confidence: 70, expr: "'OtherMid'" }),
        ];
        const { bindings, helpers } = makeCtx({}, {}, ["Low", "Mid", "OtherMid"]);
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBe("Mid");
        expect(r.reason).toBe("r2");
    });

    test("first rule wins ties", () => {
        const rules = [
            compileRule({ name: "r1", confidence: 80, expr: "'A'" }),
            compileRule({ name: "r2", confidence: 80, expr: "'B'" }),
        ];
        const { bindings, helpers } = makeCtx({}, {}, ["A", "B"]);
        expect(runRules(rules, bindings, helpers).className).toBe("A");
    });

    test("rule exception → warn and continue", () => {
        const rules = [
            compileRule({ name: "r1", confidence: 100, expr: "item.missing.nested" }),
            compileRule({ name: "r2", confidence: 80, expr: "'Survived'" }),
        ];
        const { bindings, helpers } = makeCtx({ ok: true }, {}, ["Survived"]);
        expect(runRules(rules, bindings, helpers).className).toBe("Survived");
    });

    test("class beats NEW when both occurred", () => {
        const rules = [
            compileRule({ name: "r1", confidence: 50, expr: "NEW('cand')" }),
            compileRule({ name: "r2", confidence: 80, expr: "'Foo'" }),
        ];
        const { bindings, helpers } = makeCtx({}, {}, ["Foo"]);
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBe("Foo");
        expect(r.category).toBe("FOUND");
    });

    test("only NEW raised → emit NEW with first token", () => {
        const rules = [
            compileRule({ name: "r1", confidence: 80, expr: "NEW('candidate1', 'in folder')" }),
            compileRule({ name: "r2", confidence: 80, expr: "null" }),
        ];
        const { bindings, helpers } = makeCtx({});
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBe("candidate1");
        expect(r.category).toBe("NEW");
    });
});

describe("runRules — realistic patterns", () => {
    const classes = ["Acme", "Rendering", "Delta"];
    const dicts = {
        aliases: [["rendring", "Rendering"]],
    };

    test("folder exact normalize match", () => {
        const rules = [
            compileRule({
                name: "folder_exact",
                confidence: 95,
                expr: `
                const folder = segment(item, -2);
                const hit = classes.find(c => normalize(c) === normalize(folder));
                return hit ? CLASS(hit) : null;
            `,
            }),
        ];
        const { bindings, helpers } = makeCtx("docs/Rendering/sample.pdf", {}, classes, dicts);
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBe("Rendering");
        expect(r.category).toBe("FOUND");
    });

    test("alias lookup", () => {
        const rules = [
            compileRule({
                name: "alias_lookup",
                confidence: 95,
                expr: `
                const first = words(fileStem(item))[0];
                const canonical = alias('aliases', first.toLowerCase());
                return canonical;
            `,
            }),
        ];
        const { bindings, helpers } = makeCtx("x/rendring_sample.pdf", {}, classes, dicts);
        const r = runRules(rules, bindings, helpers);
        expect(r.className).toBe("Rendering");
        expect(r.category).toBe("FOUND");
    });
});
