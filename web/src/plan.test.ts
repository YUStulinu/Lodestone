import { describe, expect, it } from "vitest";
import { flatten, formatMs, formatRows, hottest, misestimate, parseExplain } from "./plan";

// The shape Lodestone's EXPLAIN (ANALYZE, FORMAT JSON) produces.
const sample = [
  {
    Plan: {
      "Node Type": "Hash Join",
      "Plan Rows": 100,
      "Total Cost": 300,
      "Actual Rows": 2500,
      "Actual Loops": 1,
      "Actual Total Time": 12.0,
      "Hash Cond": "e.dept = d.id",
      Plans: [
        { "Node Type": "Seq Scan on emp", "Relation Name": "emp", "Plan Rows": 1000, "Total Cost": 100, "Actual Rows": 1000, "Actual Loops": 1, "Actual Total Time": 4.0, Filter: "(e.salary > 10)" },
        { "Node Type": "Seq Scan on dept", "Relation Name": "dept", "Plan Rows": 50, "Total Cost": 50, "Actual Rows": 5, "Actual Loops": 1, "Actual Total Time": 1.5 },
        { "Node Type": "Aggregate", "Subplan Name": "SubPlan 1", "Plan Rows": 1, "Total Cost": 10, "Actual Rows": 1, "Actual Loops": 3, "Actual Total Time": 0.5 },
      ],
    },
    "Planning Time": 0.2,
    "Execution Time": 12.5,
  },
];

describe("parseExplain", () => {
  const { root, planningMs, executionMs } = parseExplain(sample);

  it("reads the tree and the timings", () => {
    expect(root.type).toBe("Hash Join");
    expect(root.children.map((c) => c.type)).toEqual(["Seq Scan on emp", "Seq Scan on dept", "Aggregate"]);
    expect(flatten(root).map((n) => n.id)).toEqual([0, 1, 2, 3]);
    expect([planningMs, executionMs]).toEqual([0.2, 12.5]);
    expect(root.details).toEqual([["Hash Cond", "e.dept = d.id"]]);
    expect(root.children[0].details).toContainEqual(["Filter", "(e.salary > 10)"]);
    expect(root.children[2].subplan).toBe("SubPlan 1");
  });

  it("subtracts children's time, but not subplans' (they run inside expressions)", () => {
    expect(root.selfMs).toBeCloseTo(12 - 4 - 1.5);
    expect(root.children[0].selfMs).toBe(4);
    expect(hottest(root)?.type).toBe("Hash Join");
  });

  it("flags misestimates in both directions", () => {
    expect(misestimate(root)).toEqual({ factor: 25, under: true });
    expect(misestimate(root.children[1])).toEqual({ factor: 10, under: false });
    expect(misestimate(root.children[0])).toEqual({ factor: 1, under: true });
  });

  it("works without ANALYZE", () => {
    const { root } = parseExplain([{ Plan: { "Node Type": "Seq Scan on t", "Plan Rows": 3, "Total Cost": 3 } }]);
    expect(root.actualRows).toBeUndefined();
    expect(root.selfMs).toBeUndefined();
    expect(misestimate(root)).toBeUndefined();
  });

  it("rejects what is not a plan", () => {
    expect(() => parseExplain({ nope: 1 })).toThrow();
  });
});

describe("formatting", () => {
  it("formats times and row counts", () => {
    expect(formatMs(0.0123)).toBe("12 µs");
    expect(formatMs(3.456)).toBe("3.46 ms");
    expect(formatMs(2500)).toBe("2.50 s");
    expect(formatRows(42)).toBe("42");
    expect(formatRows(25_000)).toBe("25k");
    expect(formatRows(3_200_000)).toBe("3.2M");
  });
});
