import "./style.css";
import { formatMs, formatRows, hottest, misestimate, parseExplain, type PlanNode } from "./plan";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

const SAMPLES: [string, string][] = [
  [
    "Join, filter and group",
    "SELECT c.country, count(*) AS orders, sum(o.total) AS revenue\nFROM customers c JOIN orders o ON o.customer_id = c.id\nWHERE o.total > 100\nGROUP BY c.country\nORDER BY revenue DESC",
  ],
  ["Primary key lookup", "SELECT * FROM orders WHERE id = 4242"],
  ["Secondary index", "SELECT count(*), avg(total) FROM orders WHERE customer_id = 17"],
  ["Range on the primary key", "SELECT sum(total) FROM orders WHERE id BETWEEN 1000 AND 1200"],
  [
    "Correlated subquery",
    "SELECT name FROM customers c\nWHERE (SELECT count(*) FROM orders o WHERE o.customer_id = c.id) > 12\nORDER BY name",
  ],
  [
    "Three tables",
    "SELECT p.category, count(*)\nFROM orders o JOIN lines l ON l.order_id = o.id JOIN products p ON p.id = l.product_id\nWHERE o.total > 400 AND p.price < 20\nGROUP BY p.category",
  ],
  ["An update (rolled back)", "UPDATE orders SET total = total * 1.1 WHERE customer_id IN (SELECT id FROM customers WHERE country = 'RO')"],
];

const COUNTRIES = ["RO", "FI", "DE", "FR", "PL", "SE", "IT", "ES"];
const CATEGORIES = ["tools", "garden", "books", "toys", "food"];

/** Demo tables, generated client-side and sent in batches. */
function demoStatements(): string[] {
  const out = [
    "DROP TABLE IF EXISTS lines; DROP TABLE IF EXISTS orders; DROP TABLE IF EXISTS customers; DROP TABLE IF EXISTS products",
    "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT, country TEXT)",
    "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER, total REAL)",
    "CREATE TABLE products (id INTEGER PRIMARY KEY, category TEXT, price REAL)",
    "CREATE TABLE lines (order_id INTEGER, product_id INTEGER, qty INTEGER, PRIMARY KEY (order_id, product_id))",
  ];
  let seed = 25;
  const rand = () => ((seed = (seed * 1103515245 + 12345) % 2147483648) / 2147483648);
  const batch = (table: string, rows: string[]) => {
    for (let i = 0; i < rows.length; i += 500) out.push(`INSERT INTO ${table} VALUES ${rows.slice(i, i + 500).join(",")}`);
  };
  batch("customers", Array.from({ length: 1000 }, (_, i) => `(${i}, 'customer ${i}', '${COUNTRIES[Math.floor(rand() ** 2 * COUNTRIES.length)]}')`));
  batch("orders", Array.from({ length: 10000 }, (_, i) => `(${i}, ${Math.floor(rand() * 1000)}, ${(rand() * 500).toFixed(2)})`));
  batch("products", Array.from({ length: 200 }, (_, i) => `(${i}, '${CATEGORIES[i % 5]}', ${(1 + rand() * 60).toFixed(2)})`));
  const lines: string[] = [];
  for (let o = 0; o < 10000; o++) {
    const used = new Set<number>();
    for (let k = 0; k < 1 + Math.floor(rand() * 3); k++) {
      const p = Math.floor(rand() * 200);
      if (!used.has(p)) {
        used.add(p);
        lines.push(`(${o}, ${p}, ${1 + Math.floor(rand() * 5)})`);
      }
    }
  }
  batch("lines", lines);
  out.push("CREATE INDEX orders_customer ON orders (customer_id)");
  return out;
}

function setStatus(text: string, error = false) {
  const s = $("status");
  s.textContent = text;
  s.classList.toggle("error", error);
}

async function post(path: string, body: unknown): Promise<unknown> {
  const r = await fetch(path, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
  const data = await r.json();
  if (!r.ok) throw new Error(data.error ?? r.statusText);
  return data;
}

function el(tag: string, cls?: string, text?: string): HTMLElement {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
}

function nodeCard(n: PlanNode, totalMs: number | undefined, hot: PlanNode | undefined): HTMLElement {
  const card = el("div", "node");
  if (hot && hot.id === n.id && (n.selfMs ?? 0) > 0) card.classList.add("hot");
  const head = el("div", "head");
  if (n.subplan) head.append(el("span", "subplan", n.subplan));
  head.append(el("span", "type", n.type));
  card.append(head);

  const stats = el("div", "stats");
  const rows = el("span", "rows");
  rows.append(el("b", undefined, formatRows(n.actualRows ?? n.estRows)), document.createTextNode(n.actualRows === undefined ? " rows (est.)" : ` rows × ${n.loops} loop${n.loops === 1 ? "" : "s"}`));
  stats.append(rows);
  const m = misestimate(n);
  if (m && m.factor >= 10) {
    stats.append(el("span", `misest ${m.under ? "under" : "over"}`, `${m.factor >= 100 ? Math.round(m.factor) : m.factor.toFixed(0)}× ${m.under ? "more" : "fewer"} than the ${formatRows(n.estRows)} estimated`));
  } else if (n.actualRows !== undefined) {
    stats.append(el("span", "est", `est. ${formatRows(n.estRows)}`));
  }
  stats.append(el("span", "cost", `cost ${n.cost.toFixed(0)}`));
  card.append(stats);

  if (n.selfMs !== undefined && totalMs) {
    const share = Math.min(1, n.selfMs / totalMs);
    const bar = el("div", "bar");
    const fill = el("div", "fill");
    fill.style.width = `${(share * 100).toFixed(1)}%`;
    bar.append(fill);
    card.append(bar, el("div", "time", `${formatMs(n.selfMs)} here · ${formatMs(n.totalMs)} with inputs · ${(share * 100).toFixed(0)}% of the query`));
  }
  if (n.details.length) {
    const dl = el("dl");
    for (const [k, v] of n.details) {
      dl.append(el("dt", undefined, k), el("dd", undefined, v));
    }
    card.append(dl);
  }
  return card;
}

function renderTree(n: PlanNode, totalMs: number | undefined, hot: PlanNode | undefined): HTMLElement {
  const li = el("li");
  li.append(nodeCard(n, totalMs, hot));
  if (n.children.length) {
    const ul = el("ul");
    for (const c of n.children) ul.append(renderTree(c, totalMs, hot));
    li.append(ul);
  }
  return li;
}

async function explain(analyze: boolean) {
  const sql = $<HTMLTextAreaElement>("sql").value.trim();
  if (!sql) return;
  setStatus(analyze ? "Running…" : "Planning…");
  try {
    const json = await post("/api/explain", { sql, analyze });
    const { root, planningMs, executionMs } = parseExplain(json);
    const hot = hottest(root);
    const total = root.totalMs;
    const summary = $("summary");
    summary.replaceChildren();
    const fact = (label: string, value: string) => {
      const d = el("div", "fact");
      d.append(el("span", undefined, label), el("b", undefined, value));
      summary.append(d);
    };
    fact("planning", formatMs(planningMs) || "–");
    if (executionMs !== undefined) fact("execution", formatMs(executionMs));
    if (root.actualRows !== undefined) fact("rows out", formatRows(root.actualRows));
    if (hot && total) fact("most time", `${hot.type} (${((100 * (hot.selfMs ?? 0)) / total).toFixed(0)}%)`);
    const ul = el("ul", "tree");
    ul.append(renderTree(root, total, hot));
    $("tree").replaceChildren(ul);
    setStatus("");
  } catch (e) {
    setStatus((e as Error).message, true);
  }
}

const samples = $<HTMLSelectElement>("samples");
for (const [name, sql] of SAMPLES) {
  const o = document.createElement("option");
  o.textContent = name;
  o.value = sql;
  samples.append(o);
}
samples.addEventListener("change", () => {
  $<HTMLTextAreaElement>("sql").value = samples.value;
  void explain(true);
});
$<HTMLTextAreaElement>("sql").value = SAMPLES[0][1];
$("explain").addEventListener("click", () => void explain(false));
$("analyze").addEventListener("click", () => void explain(true));
$<HTMLTextAreaElement>("sql").addEventListener("keydown", (e) => {
  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) void explain(true);
});
$("demo").addEventListener("click", async () => {
  try {
    const stmts = demoStatements();
    for (let i = 0; i < stmts.length; i++) {
      setStatus(`Loading demo tables… ${Math.round((100 * i) / stmts.length)}%`);
      const rep = (await post("/api/query", { sql: stmts[i] })) as { error?: { message: string } };
      if (rep.error) throw new Error(rep.error.message);
    }
    setStatus("Demo tables loaded: customers (1,000), orders (10,000), products (200), lines (~20,000).");
    void explain(true);
  } catch (e) {
    setStatus((e as Error).message, true);
  }
});
