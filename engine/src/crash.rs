//! The crash test: a worker process writes transactions from two sessions at once and reports
//! each one only after its COMMIT has returned; the harness kills it at a random moment
//! (TerminateProcess on Windows, SIGKILL elsewhere: no chance to clean up), reopens the
//! database and checks it.
//!
//!   lodestone crash-worker <db>                              (run by the harness)
//!   lodestone crash-check <db> <kills> [--json out] [--seed n]
//!
//! Each transaction t writes k rows (t, 0..k, k, payload) and adds one to its writer's counter.
//! With two writers, one's commit flushes pages that hold the other's uncommitted versions, so
//! after a kill recovery must roll those back. Afterwards the database must hold every
//! transaction reported as committed, each one whole (k rows, all saying k), at most one
//! unreported transaction per writer (committed just before the kill), counters equal to the
//! number of transactions, and a sound B+tree. A small checkpoint threshold makes checkpoints
//! frequent, so kills also land in the middle of them.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lodestone::sql::{Database, Output, Value};
use lodestone::storage::Options;
use lodestone::storage::vfs::OsVfs;

const WRITERS: i64 = 2;

fn opts() -> Options {
    Options { cache_pages: 512, checkpoint_bytes: 512 * 1024 }
}

fn open(path: &Path) -> Database {
    Database::open_with(&OsVfs, path, opts()).unwrap_or_else(|e| {
        eprintln!("cannot open {}: {e}", path.display());
        std::process::exit(1);
    })
}

fn int(out: &[Output]) -> i64 {
    match out.last() {
        Some(Output::Rows { rows, .. }) => match rows.first().and_then(|r| r.first()) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        },
        _ => 0,
    }
}

fn transactions(db: &Database) -> BTreeSet<i64> {
    match db.session().execute("SELECT DISTINCT txn FROM ledger").unwrap().pop() {
        Some(Output::Rows { rows, .. }) => rows.iter().filter_map(|r| if let Value::Int(i) = r[0] { Some(i) } else { None }).collect(),
        _ => BTreeSet::new(),
    }
}

pub fn worker(args: &[String]) {
    let path = PathBuf::from(&args[0]);
    let db = open(&path);
    let mut s = db.session();
    s.execute(
        "CREATE TABLE IF NOT EXISTS ledger (txn INTEGER, i INTEGER, size INTEGER, payload TEXT, PRIMARY KEY (txn, i));
         CREATE TABLE IF NOT EXISTS counter (id INTEGER PRIMARY KEY, n INTEGER)",
    )
    .unwrap();
    for w in 0..WRITERS {
        if int(&s.execute(&format!("SELECT count(*) FROM counter WHERE id = {w}")).unwrap()) == 0 {
            s.execute(&format!("INSERT INTO counter VALUES ({w}, 0)")).unwrap();
        }
    }
    let next = Arc::new(AtomicI64::new(int(&s.execute("SELECT max(txn) FROM ledger").unwrap()) + 1));
    let out = Arc::new(Mutex::new(std::io::stdout()));
    let threads: Vec<_> = (0..WRITERS)
        .map(|w| {
            let (db, next, out) = (db.clone(), next.clone(), out.clone());
            std::thread::spawn(move || {
                let mut s = db.session();
                let mut rng = fastrand::Rng::new();
                let mut mine: Vec<i64> = Vec::new();
                loop {
                    let t = next.fetch_add(1, Ordering::SeqCst);
                    let k = rng.i64(1..=20);
                    let mut sql = String::from("BEGIN;");
                    for i in 0..k {
                        let payload: String = std::iter::repeat_n('x', rng.usize(0..400)).collect();
                        sql += &format!("INSERT INTO ledger VALUES ({t}, {i}, {k}, '{payload}');");
                    }
                    if !mine.is_empty() && rng.bool() {
                        // Rewrite one of this writer's old rows, so checkpointed pages change again.
                        let old = mine[rng.usize(..mine.len())];
                        let payload: String = std::iter::repeat_n('y', rng.usize(0..400)).collect();
                        sql += &format!("UPDATE ledger SET payload = '{payload}' WHERE txn = {old} AND i = 0;");
                    }
                    sql += &format!("UPDATE counter SET n = n + 1 WHERE id = {w}; COMMIT;");
                    if let Err(e) = s.execute(&sql) {
                        // The writers touch disjoint rows, so any failure is a real error.
                        eprintln!("writer {w}: {e}");
                        std::process::exit(3);
                    }
                    mine.push(t);
                    let mut o = out.lock().unwrap();
                    writeln!(o, "{t} {k}").unwrap();
                    o.flush().unwrap();
                }
            })
        })
        .collect();
    for t in threads {
        let _ = t.join();
    }
}

#[derive(Debug, serde::Serialize)]
pub struct Verdict {
    pub ok: bool,
    pub reported: usize,
    pub present: usize,
    pub problems: Vec<String>,
}

/// Checks a database after a kill, given every transaction reported committed so far.
pub fn verify(path: &Path, reported: &BTreeSet<i64>) -> Verdict {
    let mut problems = Vec::new();
    let db = open(path);
    if let Err(e) = db.mvcc().check() {
        problems.push(format!("B+tree: {e}"));
    }
    let mut s = db.session();
    let present = transactions(&db);
    let lost: Vec<_> = reported.difference(&present).take(5).collect();
    if !lost.is_empty() {
        problems.push(format!("reported committed but missing: {lost:?}"));
    }
    let unreported = present.difference(reported).count() as i64;
    if unreported > WRITERS {
        problems.push(format!("{unreported} transactions present that were never reported (at most {WRITERS} may be)"));
    }
    let counter = int(&s.execute("SELECT sum(n) FROM counter").unwrap());
    if counter != present.len() as i64 {
        problems.push(format!("counters add up to {counter}, but {} transactions are present", present.len()));
    }
    let broken = int(&s
        .execute("SELECT count(*) FROM (SELECT txn FROM ledger GROUP BY txn HAVING count(*) <> min(size) OR min(size) <> max(size)) x")
        .unwrap());
    if broken > 0 {
        problems.push(format!("{broken} transactions partly present"));
    }
    if db.mvcc().status().map(|s| s.active).unwrap_or(1) != 0 {
        problems.push("transactions still marked active after recovery".into());
    }
    Verdict { ok: problems.is_empty(), reported: reported.len(), present: present.len(), problems }
}

#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub kills: usize,
    pub failed_checks: usize,
    pub transactions_committed: usize,
    pub unreported_but_committed: usize,
    pub kills_after_a_checkpoint: usize,
    pub problems: Vec<Vec<String>>,
}

/// Runs the worker `runs` times on one database, killing it each time.
pub fn campaign(exe: &Path, path: &Path, runs: usize, seed: u64) -> Report {
    let mut rng = fastrand::Rng::with_seed(seed);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(lodestone::storage::pager::wal_path(path));
    let mut reported = BTreeSet::new();
    let (mut failed, mut unreported, mut checkpoints, mut last_wal) = (0, 0, 0, 0);
    let mut problems = Vec::new();
    for run in 0..runs {
        let mut child =
            Command::new(exe).arg("crash-worker").arg(path).stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().expect("spawn worker");
        let stdout = child.stdout.take().unwrap();
        let fresh = Arc::new(Mutex::new(Vec::new()));
        let f2 = fresh.clone();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(t) = line.split_whitespace().next().and_then(|t| t.parse::<i64>().ok()) {
                    f2.lock().unwrap().push(t);
                }
            }
        });
        // Let it run, then kill it at a random moment.
        std::thread::sleep(Duration::from_millis(rng.u64(30..400)));
        child.kill().expect("kill");
        child.wait().ok();
        reader.join().ok();
        reported.extend(fresh.lock().unwrap().iter().copied());
        let wal = std::fs::metadata(lodestone::storage::pager::wal_path(path)).map(|m| m.len()).unwrap_or(0);
        if wal < last_wal {
            checkpoints += 1;
        }
        last_wal = wal;
        let v = verify(path, &reported);
        if !v.ok {
            eprintln!("kill {run}: {:?}", v.problems);
            failed += 1;
            problems.push(v.problems.clone());
        }
        // Transactions that committed without being reported are part of the history now.
        let present = transactions(&open(path));
        unreported += present.difference(&reported).count();
        reported.extend(present);
    }
    Report {
        kills: runs,
        failed_checks: failed,
        transactions_committed: reported.len(),
        unreported_but_committed: unreported,
        kills_after_a_checkpoint: checkpoints,
        problems,
    }
}

pub fn check(args: &[String]) {
    let path = PathBuf::from(args.first().cloned().unwrap_or_else(|| "crash.db".into()));
    let runs: usize = args.get(1).and_then(|r| r.parse().ok()).unwrap_or(50);
    let json = args.iter().position(|a| a == "--json").and_then(|i| args.get(i + 1)).cloned();
    let seed = args.iter().position(|a| a == "--seed").and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(25);
    let exe = std::env::current_exe().unwrap();
    let t0 = Instant::now();
    let r = campaign(&exe, &path, runs, seed);
    println!(
        "{} kills, {} failed checks; {} transactions committed ({} of them committed but not yet reported when killed); a checkpoint ran before {} of the kills; {:.0} s",
        r.kills,
        r.failed_checks,
        r.transactions_committed,
        r.unreported_but_committed,
        r.kills_after_a_checkpoint,
        t0.elapsed().as_secs_f64()
    );
    if let Some(p) = json {
        let mut v = serde_json::to_value(&r).unwrap();
        v["seconds"] = serde_json::json!(t0.elapsed().as_secs_f64().round());
        v["seed"] = serde_json::json!(seed);
        v["platform"] = serde_json::json!(std::env::consts::OS);
        std::fs::write(p, serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }
    std::process::exit(if r.failed_checks == 0 { 0 } else { 1 });
}
