//! A runner for SQLite's sqllogictest files.
//!
//! Records are separated by blank lines: `statement ok|error` with SQL, or
//! `query <types> <sort> [label]` with SQL, `----` and the expected values (one a line, or
//! "N values hashing to <md5>"). `skipif`/`onlyif <engine>` lines guard a record; Lodestone
//! runs records not reserved for another engine. Values are printed the way the reference
//! runner prints them: NULL, "(empty)" for '', integers for I columns, three decimals for R.

use std::path::Path;

use md5::{Digest, Md5};
use serde::Serialize;

use crate::sql::{Database, Output, Session, Value};
use crate::storage::Options;
use crate::storage::vfs::SimVfs;

pub const ENGINE: &str = "lodestone";

#[derive(Debug, Default, Clone, Serialize)]
pub struct FileResult {
    pub file: String,
    pub statements: usize,
    pub statements_failed: usize,
    pub queries: usize,
    pub queries_failed: usize,
    pub skipped: usize,
    /// The first few failures, for reading.
    pub failures: Vec<String>,
    /// The first line of every failure (the error, without the SQL), for counting.
    pub failure_heads: Vec<String>,
    pub seconds: f64,
}

impl FileResult {
    pub fn passed(&self) -> usize {
        self.statements + self.queries - self.statements_failed - self.queries_failed
    }
}

fn format_value(v: &Value, ty: char) -> String {
    let s = match (v, ty) {
        (Value::Null, _) => return "NULL".into(),
        (Value::Int(i), 'I') => i.to_string(),
        (Value::Float(f), 'I') => (f.trunc() as i64).to_string(),
        (Value::Bool(b), 'I') => (*b as i64).to_string(),
        (Value::Text(t), 'I') => {
            // The leading integer of the text, as SQLite's sqlite3_column_int64 reads it.
            let t = t.trim_start();
            let end = t.char_indices().take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+'))).count();
            t[..end].parse::<i64>().unwrap_or(0).to_string()
        }
        (Value::Int(i), 'R') => format!("{:.3}", *i as f64),
        (Value::Float(f), 'R') => format!("{:.3}", f),
        (Value::Bool(b), 'R') => format!("{:.3}", *b as i64 as f64),
        (Value::Text(t), 'R') => format!("{:.3}", t.trim().parse::<f64>().unwrap_or(0.0)),
        (Value::Text(t), _) => t.clone(),
        (Value::Int(i), _) => i.to_string(),
        (Value::Float(f), _) => format!("{:.3}", f),
        (Value::Bool(b), _) => (*b as i64).to_string(),
    };
    if s.is_empty() {
        return "(empty)".into();
    }
    s.chars().map(|c| if (' '..='~').contains(&c) { c } else { '@' }).collect()
}

fn md5_hex(values: &[String]) -> String {
    let mut h = Md5::new();
    for v in values {
        h.update(v.as_bytes());
        h.update(b"\n");
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Runs one file against a fresh in-memory database.
pub fn run_file(path: &Path) -> std::io::Result<FileResult> {
    let text = std::fs::read_to_string(path)?;
    let t0 = std::time::Instant::now();
    let db = Database::open_with(&SimVfs::new(), Path::new("slt.db"), Options::default()).expect("open");
    let mut session = db.session();
    let mut res = run_script(&mut session, &text);
    res.file = path.display().to_string().replace('\\', "/");
    res.seconds = t0.elapsed().as_secs_f64();
    Ok(res)
}

pub fn run_script(session: &mut Session, text: &str) -> FileResult {
    let mut res = FileResult::default();
    let mut hash_threshold = 0usize;
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    let fail = |res: &mut FileResult, msg: String| {
        res.failure_heads.push(msg.lines().next().unwrap_or("").to_string());
        if res.failures.len() < 20 {
            res.failures.push(msg);
        }
    };
    'records: while i < lines.len() {
        let line = lines[i].trim_end();
        if line.is_empty() || line.starts_with('#') {
            i += 1;
            continue;
        }
        // Conditions.
        let mut skip = false;
        while let Some(l) = lines.get(i).map(|l| l.trim_end()) {
            if let Some(engine) = l.strip_prefix("skipif ") {
                skip |= engine.trim() == ENGINE;
            } else if let Some(engine) = l.strip_prefix("onlyif ") {
                skip |= engine.trim() != ENGINE;
            } else {
                break;
            }
            i += 1;
        }
        let Some(head) = lines.get(i).map(|l| l.trim_end()) else { break };
        let start_line = i + 1;
        i += 1;
        let words: Vec<&str> = head.split_whitespace().collect();
        match words.first().copied() {
            Some("halt") => {
                if skip {
                    continue;
                }
                break 'records;
            }
            Some("hash-threshold") => {
                hash_threshold = words.get(1).and_then(|w| w.parse().ok()).unwrap_or(0);
                continue;
            }
            Some("statement") | Some("query") => {}
            _ => {
                // Unknown record: skip to the next blank line.
                while i < lines.len() && !lines[i].trim().is_empty() {
                    i += 1;
                }
                continue;
            }
        }
        // The SQL, up to a blank line or ----.
        let mut sql = String::new();
        while i < lines.len() && !lines[i].trim().is_empty() && lines[i].trim_end() != "----" {
            sql.push_str(lines[i]);
            sql.push('\n');
            i += 1;
        }
        let mut expected: Vec<String> = Vec::new();
        if i < lines.len() && lines[i].trim_end() == "----" {
            i += 1;
            while i < lines.len() && !lines[i].trim().is_empty() {
                expected.push(lines[i].trim_end().to_string());
                i += 1;
            }
        }
        if skip {
            res.skipped += 1;
            continue;
        }
        let result = session.execute(&sql);
        if words[0] == "statement" {
            res.statements += 1;
            let want_ok = words.get(1) == Some(&"ok");
            match (&result, want_ok) {
                (Ok(_), true) | (Err(_), false) => {}
                (Err(e), true) => {
                    res.statements_failed += 1;
                    fail(&mut res, format!("line {start_line}: statement failed: {e}\n  {}", sql.trim()));
                }
                (Ok(_), false) => {
                    res.statements_failed += 1;
                    fail(&mut res, format!("line {start_line}: statement should have failed\n  {}", sql.trim()));
                }
            }
            continue;
        }
        // A query.
        res.queries += 1;
        let types: Vec<char> = words.get(1).map(|t| t.chars().collect()).unwrap_or_default();
        let sort = words.get(2).copied().unwrap_or("nosort");
        let rows = match result {
            Ok(out) => match out.into_iter().last() {
                Some(Output::Rows { rows, .. }) => rows,
                _ => {
                    res.queries_failed += 1;
                    fail(&mut res, format!("line {start_line}: query returned no rows\n  {}", sql.trim()));
                    continue;
                }
            },
            Err(e) => {
                res.queries_failed += 1;
                fail(&mut res, format!("line {start_line}: query failed: {e}\n  {}", sql.trim()));
                continue;
            }
        };
        let mut formatted: Vec<Vec<String>> = rows
            .iter()
            .map(|r| r.iter().enumerate().map(|(c, v)| format_value(v, types.get(c).copied().unwrap_or('T'))).collect())
            .collect();
        if formatted.first().is_some_and(|r| r.len() != types.len()) {
            res.queries_failed += 1;
            fail(&mut res, format!("line {start_line}: {} columns, expected {}\n  {}", formatted[0].len(), types.len(), sql.trim()));
            continue;
        }
        let mut values: Vec<String> = match sort {
            "rowsort" => {
                formatted.sort();
                formatted.into_iter().flatten().collect()
            }
            "valuesort" => {
                let mut v: Vec<String> = formatted.into_iter().flatten().collect();
                v.sort();
                v
            }
            _ => formatted.into_iter().flatten().collect(),
        };
        let hashed = expected.len() == 1 && expected[0].contains(" values hashing to ");
        let ok = if hashed {
            let mut parts = expected[0].split_whitespace();
            let n: usize = parts.next().and_then(|n| n.parse().ok()).unwrap_or(usize::MAX);
            let hash = expected[0].rsplit(' ').next().unwrap_or("");
            n == values.len() && md5_hex(&values) == hash
        } else {
            // One value a line, or a row a line with values separated by spaces.
            let want: Vec<String> = if expected.len() == values.len() {
                expected.clone()
            } else {
                expected.iter().flat_map(|l| l.split_whitespace().map(String::from)).collect()
            };
            if hash_threshold > 0 && values.len() > hash_threshold && want.len() != values.len() {
                false
            } else {
                values.iter_mut().zip(&want).all(|(a, b)| a == b) && want.len() == values.len()
            }
        };
        if !ok {
            res.queries_failed += 1;
            let got: Vec<String> = values.iter().take(12).cloned().collect();
            let want: Vec<String> = expected.iter().take(12).cloned().collect();
            fail(
                &mut res,
                format!("line {start_line}: wrong result\n  {}\n  got:  {}\n  want: {}", sql.trim(), got.join(" "), want.join(" ")),
            );
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_records() {
        let db = Database::open_with(&SimVfs::new(), Path::new("x"), Options::default()).unwrap();
        let mut s = db.session();
        let script = "\
statement ok
CREATE TABLE t1(a INTEGER, b TEXT)

statement ok
INSERT INTO t1 VALUES(1,'x'),(2,''),(NULL,'y')

query IT rowsort
SELECT a, b FROM t1
----
1
x
2
(empty)
NULL
y

query I nosort
SELECT a FROM t1 WHERE a IS NOT NULL ORDER BY a
----
2 values hashing to 6ddb4095eb719e2a9f0a3f95677d24e0

onlyif mysql
query I nosort
SELECT 'not us'
----
1

statement error
SELECT * FROM nope

query R nosort
SELECT 1.0/3
----
0.333

query I nosort
SELECT a FROM t1 WHERE a = 1
----
2
";
        let r = run_script(&mut s, script);
        assert_eq!((r.statements, r.queries, r.skipped), (3, 4, 1));
        assert_eq!(r.queries_failed, 1, "{:#?}", r.failures);
        assert!(r.failures[0].contains("wrong result"));
    }
}
