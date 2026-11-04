// Reading EXPLAIN (FORMAT JSON) output: Lodestone uses PostgreSQL's keys, so this reads
// PostgreSQL plans as well.

export interface RawNode {
  "Node Type": string;
  "Plan Rows"?: number;
  "Total Cost"?: number;
  "Actual Rows"?: number;
  "Actual Loops"?: number;
  "Actual Total Time"?: number;
  "Subplan Name"?: string;
  Plans?: RawNode[];
  [key: string]: unknown;
}

export interface RawExplain {
  Plan: RawNode;
  "Planning Time"?: number;
  "Execution Time"?: number;
}

export interface PlanNode {
  id: number;
  type: string;
  subplan?: string;
  estRows: number;
  cost: number;
  /** Rows per loop, as PostgreSQL reports them; undefined without ANALYZE. */
  actualRows?: number;
  loops?: number;
  /** Milliseconds, including children. */
  totalMs?: number;
  /** Milliseconds spent in this node alone. */
  selfMs?: number;
  details: [string, string][];
  children: PlanNode[];
}

const KNOWN = new Set([
  "Node Type", "Plan Rows", "Total Cost", "Actual Rows", "Actual Loops", "Actual Total Time", "Plans", "Subplan Name", "Correlated",
]);

export function parseExplain(json: unknown): { root: PlanNode; planningMs?: number; executionMs?: number } {
  const top = (Array.isArray(json) ? json[0] : json) as RawExplain;
  if (!top || typeof top !== "object" || !top.Plan) throw new Error("not an EXPLAIN (FORMAT JSON) result");
  let next = 0;
  const walk = (raw: RawNode): PlanNode => {
    const id = next++; // pre-order: a node before its children
    const children = (raw.Plans ?? []).map(walk);
    const details: [string, string][] = Object.entries(raw)
      .filter(([k]) => !KNOWN.has(k))
      .map(([k, v]) => [k, String(v)]);
    const node: PlanNode = {
      id,
      type: raw["Node Type"],
      subplan: raw["Subplan Name"],
      estRows: raw["Plan Rows"] ?? 0,
      cost: raw["Total Cost"] ?? 0,
      actualRows: raw["Actual Rows"],
      loops: raw["Actual Loops"],
      totalMs: raw["Actual Total Time"],
      details,
      children,
    };
    return node;
  };
  const root = walk(top.Plan);
  computeSelfTime(root);
  return { root, planningMs: top["Planning Time"], executionMs: top["Execution Time"] };
}

/** Self time = total time minus the children's total time (never below zero: timers are noisy). */
export function computeSelfTime(n: PlanNode): void {
  for (const c of n.children) computeSelfTime(c);
  if (n.totalMs === undefined) return;
  const inChildren = n.children.filter((c) => !c.subplan).reduce((s, c) => s + (c.totalMs ?? 0), 0);
  n.selfMs = Math.max(0, n.totalMs - inChildren);
}

/** How far the row estimate was off: actual / estimated as a factor >= 1, and in which direction. */
export function misestimate(n: PlanNode): { factor: number; under: boolean } | undefined {
  if (n.actualRows === undefined || !n.loops) return undefined;
  const actual = Math.max(n.actualRows, 1);
  const est = Math.max(n.estRows, 1);
  return actual >= est ? { factor: actual / est, under: true } : { factor: est / actual, under: false };
}

export function flatten(n: PlanNode): PlanNode[] {
  return [n, ...n.children.flatMap(flatten)];
}

/** The node where most time was spent on its own. */
export function hottest(root: PlanNode): PlanNode | undefined {
  const all = flatten(root).filter((n) => n.selfMs !== undefined);
  return all.sort((a, b) => (b.selfMs ?? 0) - (a.selfMs ?? 0))[0];
}

export function formatMs(ms: number | undefined): string {
  if (ms === undefined) return "";
  if (ms < 1) return `${(ms * 1000).toFixed(0)} µs`;
  if (ms < 1000) return `${ms.toFixed(ms < 10 ? 2 : 1)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

export function formatRows(n: number): string {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e4) return `${(n / 1e3).toFixed(0)}k`;
  return String(Math.round(n));
}
