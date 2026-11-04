//! Lodestone against SQLite, in one process, on the same disk and data.
//!
//!   cargo run --release --features bench --bin lodestone-bench -- [--rows 100000] [--json out.json]
//!
//! Both are durable at commit: SQLite in WAL mode with synchronous=FULL, Lodestone syncing its
//! log at every commit. SQLite runs prepared statements (its normal use); Lodestone has no plan
//! cache, so each statement is parsed and planned again -- part of what is being measured.

#![allow(clippy::needless_range_loop)] // rows are numbered: the index is the row id

use std::path::{Path, PathBuf};
use std::time::Instant;

use lodestone::sql::{Database, Output, Value};
use rusqlite::{Connection, params};
use serde_json::json;

struct Case {
    name: &'static str,
    ops: usize,
}

fn rate(ops: usize, secs: f64) -> f64 {
    ops as f64 / secs
}

fn fresh(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
    }
    p
}

fn payload(i: usize) -> String {
    format!("row {i} {}", "x".repeat(40 + i % 60))
}

fn lode_int(out: Vec<Output>) -> i64 {
    match out.into_iter().last() {
        Some(Output::Rows { rows, .. }) => match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(i)) => *i,
            Some(Value::Float(f)) => *f as i64,
            _ => 0,
        },
        _ => 0,
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    let n: usize = arg("--rows").and_then(|r| r.parse().ok()).unwrap_or(100_000);
    let dir = PathBuf::from(arg("--dir").unwrap_or_else(|| "data/bench".into()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut rng = fastrand::Rng::with_seed(25);
    let keys: Vec<i64> = (0..n).map(|_| rng.i64(0..(n as i64 / 10).max(1))).collect();

    // ---- setup ----------------------------------------------------------------------------
    let lpath = fresh(&dir, "bench.lodestone");
    let ldb = Database::open(&lpath).unwrap();
    let mut l = ldb.session();
    let spath = fresh(&dir, "bench.sqlite");
    let s = Connection::open(&spath).unwrap();
    s.pragma_update(None, "journal_mode", "WAL").unwrap();
    s.pragma_update(None, "synchronous", "FULL").unwrap();
    let schema =
        "CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, v TEXT); CREATE TABLE u (id INTEGER PRIMARY KEY, t_id INTEGER, w INTEGER);";
    l.execute(schema).unwrap();
    s.execute_batch(schema).unwrap();

    let mut results = Vec::new();
    let mut record = |case: Case, ls: f64, ss: f64| {
        let (lr, sr) = (rate(case.ops, ls), rate(case.ops, ss));
        println!("{:44} Lodestone {:>12.0}/s   SQLite {:>12.0}/s   SQLite {:>6.1}x", case.name, lr, sr, sr / lr);
        results.push(json!({"case": case.name, "ops": case.ops, "lodestone_per_s": lr.round(), "sqlite_per_s": sr.round(), "sqlite_faster_by": ((sr / lr) * 10.0).round() / 10.0}));
    };

    // 1. Bulk load in one transaction.
    let t0 = Instant::now();
    l.execute("BEGIN").unwrap();
    for i in 0..n {
        l.execute_params("INSERT INTO t VALUES ($1, $2, $3)", &[Value::Int(i as i64), Value::Int(keys[i]), Value::Text(payload(i))])
            .unwrap();
    }
    l.execute("COMMIT").unwrap();
    let ls = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    {
        let tx = s.unchecked_transaction().unwrap();
        let mut st = tx.prepare("INSERT INTO t VALUES (?1, ?2, ?3)").unwrap();
        for i in 0..n {
            st.execute(params![i as i64, keys[i], payload(i)]).unwrap();
        }
        drop(st);
        tx.commit().unwrap();
    }
    record(Case { name: "insert, one transaction (rows)", ops: n }, ls, t0.elapsed().as_secs_f64());

    // Second table and index, untimed.
    l.execute("BEGIN").unwrap();
    for i in 0..n {
        l.execute_params(
            "INSERT INTO u VALUES ($1, $2, $3)",
            &[Value::Int(i as i64), Value::Int(((i * 7) % n) as i64), Value::Int(i as i64 % 13)],
        )
        .unwrap();
    }
    l.execute("COMMIT; CREATE INDEX t_k ON t (k)").unwrap();
    {
        let tx = s.unchecked_transaction().unwrap();
        let mut st = tx.prepare("INSERT INTO u VALUES (?1, ?2, ?3)").unwrap();
        for i in 0..n {
            st.execute(params![i as i64, ((i * 7) % n) as i64, i as i64 % 13]).unwrap();
        }
        drop(st);
        tx.commit().unwrap();
    }
    s.execute_batch("CREATE INDEX t_k ON t (k); ANALYZE;").unwrap();

    // 2. Durable single-row commits.
    let m = 2_000;
    let t0 = Instant::now();
    for i in 0..m {
        l.execute_params("INSERT INTO t VALUES ($1, $2, $3)", &[Value::Int((n + i) as i64), Value::Int(1), Value::Text(payload(i))])
            .unwrap();
    }
    let ls = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    {
        let mut st = s.prepare("INSERT INTO t VALUES (?1, ?2, ?3)").unwrap();
        for i in 0..m {
            st.execute(params![(n + i) as i64, 1, payload(i)]).unwrap();
        }
    }
    record(Case { name: "insert, a durable commit each (rows)", ops: m }, ls, t0.elapsed().as_secs_f64());

    // 3. Point lookups by primary key.
    let lookups: Vec<i64> = (0..n).map(|_| rng.i64(0..n as i64)).collect();
    let t0 = Instant::now();
    let mut found = 0usize;
    for &id in &lookups {
        if let Output::Rows { rows, .. } = &l.execute_params("SELECT v FROM t WHERE id = $1", &[Value::Int(id)]).unwrap()[0] {
            found += rows.len();
        }
    }
    let ls = t0.elapsed().as_secs_f64();
    assert_eq!(found, n);
    let t0 = Instant::now();
    {
        let mut st = s.prepare("SELECT v FROM t WHERE id = ?1").unwrap();
        for &id in &lookups {
            let _: String = st.query_row(params![id], |r| r.get(0)).unwrap();
        }
    }
    record(Case { name: "point lookup by primary key (queries)", ops: n }, ls, t0.elapsed().as_secs_f64());

    // 4. Secondary index lookups.
    let q = 20_000.min(n);
    let t0 = Instant::now();
    let mut lsum = 0;
    for &k in &keys[..q] {
        lsum += lode_int(l.execute_params("SELECT count(*) FROM t WHERE k = $1", &[Value::Int(k)]).unwrap());
    }
    let ls = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    let mut ssum = 0i64;
    {
        let mut st = s.prepare("SELECT count(*) FROM t WHERE k = ?1").unwrap();
        for &k in &keys[..q] {
            ssum += st.query_row(params![k], |r| r.get::<_, i64>(0)).unwrap();
        }
    }
    assert_eq!(lsum, ssum);
    record(Case { name: "count by secondary index (queries)", ops: q }, ls, t0.elapsed().as_secs_f64());

    // 5. Short range scans.
    let r = 1_000;
    let starts: Vec<i64> = (0..r).map(|_| rng.i64(0..n as i64 - 100)).collect();
    let t0 = Instant::now();
    let mut lsum = 0;
    for &a in &starts {
        lsum +=
            lode_int(l.execute_params("SELECT sum(k) FROM t WHERE id BETWEEN $1 AND $2", &[Value::Int(a), Value::Int(a + 100)]).unwrap());
    }
    let ls = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    let mut ssum = 0i64;
    {
        let mut st = s.prepare("SELECT sum(k) FROM t WHERE id BETWEEN ?1 AND ?2").unwrap();
        for &a in &starts {
            ssum += st.query_row(params![a, a + 100], |r| r.get::<_, i64>(0)).unwrap();
        }
    }
    assert_eq!(lsum, ssum);
    record(Case { name: "range of 101 rows by primary key (queries)", ops: r }, ls, t0.elapsed().as_secs_f64());

    // 6. Full scan with grouping.
    let g = 5;
    let sql = "SELECT k % 100, count(*), sum(id) FROM t GROUP BY k % 100";
    let t0 = Instant::now();
    for _ in 0..g {
        l.execute(sql).unwrap();
    }
    let ls = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    for _ in 0..g {
        let mut st = s.prepare(sql).unwrap();
        let rows: Vec<i64> = st.query_map([], |r| r.get(1)).unwrap().map(|x| x.unwrap()).collect();
        assert!(!rows.is_empty());
    }
    record(Case { name: "full scan + GROUP BY (queries)", ops: g }, ls, t0.elapsed().as_secs_f64());

    // 7. Join.
    let j = 5;
    let sql = "SELECT count(*), sum(u.w) FROM t JOIN u ON u.t_id = t.id WHERE t.k < 1000";
    let t0 = Instant::now();
    let mut lc = 0;
    for _ in 0..j {
        lc = lode_int(l.execute(sql).unwrap());
    }
    let ls = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    let mut sc = 0i64;
    for _ in 0..j {
        sc = s.query_row(sql, [], |r| r.get(0)).unwrap();
    }
    assert_eq!(lc, sc);
    record(Case { name: "join of two 100k-row tables (queries)", ops: j }, ls, t0.elapsed().as_secs_f64());

    // 8. Updates by primary key in one transaction.
    let u = 10_000.min(n);
    let ids: Vec<i64> = (0..u).map(|_| rng.i64(0..n as i64)).collect();
    let t0 = Instant::now();
    l.execute("BEGIN").unwrap();
    for &id in &ids {
        l.execute_params("UPDATE t SET k = k + 1 WHERE id = $1", &[Value::Int(id)]).unwrap();
    }
    l.execute("COMMIT").unwrap();
    let ls = t0.elapsed().as_secs_f64();
    let t0 = Instant::now();
    {
        let tx = s.unchecked_transaction().unwrap();
        let mut st = tx.prepare("UPDATE t SET k = k + 1 WHERE id = ?1").unwrap();
        for &id in &ids {
            st.execute(params![id]).unwrap();
        }
        drop(st);
        tx.commit().unwrap();
    }
    record(Case { name: "update by primary key, one transaction (rows)", ops: u }, ls, t0.elapsed().as_secs_f64());

    let size = |p: &Path| {
        std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) + std::fs::metadata(format!("{}-wal", p.display())).map(|m| m.len()).unwrap_or(0)
    };
    let (lb, sb) = (size(&lpath), size(&spath));
    println!("on disk: Lodestone {:.1} MB, SQLite {:.1} MB (files and logs)", lb as f64 / 1e6, sb as f64 / 1e6);
    if let Some(out) = arg("--json") {
        let doc = json!({
            "rows": n,
            "sqlite_version": rusqlite::version(),
            "sqlite_settings": "journal_mode=WAL, synchronous=FULL, prepared statements",
            "lodestone_settings": "log synced at every commit, statements parsed and planned each time",
            "cases": results,
            "bytes_on_disk": {"lodestone": lb, "sqlite": sb},
        });
        std::fs::write(out, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    }
}
