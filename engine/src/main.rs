//! lodestone: the database's command line.
//!
//!   lodestone shell <db>                       an interactive SQL prompt
//!   lodestone serve <db> [--listen addr]       the engine protocol the Go PostgreSQL server speaks
//!   lodestone slt [--json out.json] files...   sqllogictest files, each on a fresh database
//!   lodestone crash-worker <db>                writes until killed (for the crash test)
//!   lodestone crash-check <db> <acked>         checks a database after a kill

use std::io::{BufRead, Write};
use std::path::PathBuf;

use lodestone::sql::{Database, Output, Value};

mod crash;
mod server;

fn usage() -> ! {
    eprintln!("usage: lodestone shell|serve|slt|crash-worker|crash-check ...");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("shell") => shell(args.get(2).map(PathBuf::from).unwrap_or_else(|| usage())),
        Some("serve") => {
            let db = args.get(2).map(PathBuf::from).unwrap_or_else(|| usage());
            let listen =
                args.iter().position(|a| a == "--listen").and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| "127.0.0.1:5480".into());
            server::serve(&db, &listen);
        }
        Some("slt") => slt(&args[2..]),
        Some("crash-worker") => crash::worker(&args[2..]),
        Some("crash-check") => crash::check(&args[2..]),
        _ => usage(),
    }
}

pub fn print(out: &Output) {
    match out {
        Output::Command(c) => println!("{c}"),
        Output::Rows { columns, rows } => {
            let cells: Vec<Vec<String>> = rows
                .iter()
                .map(|r| {
                    r.iter()
                        .map(|v| match v {
                            Value::Null => String::new(),
                            Value::Text(t) => t.clone(),
                            v => v.to_string(),
                        })
                        .collect()
                })
                .collect();
            let mut widths: Vec<usize> = columns.iter().map(|c| c.name.chars().count()).collect();
            for r in &cells {
                for (w, c) in widths.iter_mut().zip(r) {
                    *w = (*w).max(c.chars().count());
                }
            }
            let line = |vals: Vec<String>| vals.iter().zip(&widths).map(|(v, w)| format!(" {v:w$} ")).collect::<Vec<_>>().join("|");
            println!("{}", line(columns.iter().map(|c| c.name.clone()).collect()));
            println!("{}", widths.iter().map(|w| "-".repeat(w + 2)).collect::<Vec<_>>().join("+"));
            for r in cells {
                println!("{}", line(r));
            }
            println!("({} row{})", rows.len(), if rows.len() == 1 { "" } else { "s" });
        }
    }
}

fn shell(path: PathBuf) {
    let db = Database::open(&path).unwrap_or_else(|e| {
        eprintln!("cannot open {}: {e}", path.display());
        std::process::exit(1);
    });
    let mut session = db.session();
    let stdin = std::io::stdin();
    let mut buf = String::new();
    loop {
        print!("{}", if buf.is_empty() { "lodestone=> " } else { "lodestone-> " });
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if buf.is_empty() && matches!(line.trim(), "\\q" | "quit" | "exit") {
            break;
        }
        buf.push_str(&line);
        if !buf.trim_end().ends_with(';') {
            continue;
        }
        let (outs, err) = session.run(&buf, &[]);
        for o in &outs {
            print(o);
        }
        if let Some(e) = err {
            println!("ERROR:  {}", e.message);
        }
        buf.clear();
    }
}

fn slt(args: &[String]) {
    let mut json = None;
    let mut files = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--json" {
            json = args.get(i + 1).cloned();
            i += 2;
        } else {
            files.push(PathBuf::from(&args[i]));
            i += 1;
        }
    }
    let mut results = Vec::new();
    for f in &files {
        match lodestone::slt::run_file(f) {
            Ok(r) => {
                println!(
                    "{:60} {:6} / {:6} passed  ({} skipped, {:.1} s)",
                    r.file,
                    r.passed(),
                    r.statements + r.queries,
                    r.skipped,
                    r.seconds
                );
                results.push(r);
            }
            Err(e) => eprintln!("{}: {e}", f.display()),
        }
    }
    let total: usize = results.iter().map(|r| r.statements + r.queries).sum();
    let passed: usize = results.iter().map(|r| r.passed()).sum();
    println!("total: {passed} / {total} records passed ({:.2}%)", 100.0 * passed as f64 / total.max(1) as f64);
    if let Some(path) = json {
        std::fs::write(&path, serde_json::to_string_pretty(&results).unwrap()).expect("write results");
    }
}
