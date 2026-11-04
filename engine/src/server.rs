//! The engine's own protocol, which the Go PostgreSQL server speaks to it: one JSON object per
//! line each way, one session per connection.
//!
//! ```text
//! -> {"op": "query", "sql": "...", "params": [1, "x", null]}
//! <- {"results": [{"columns": [{"name": "a", "type": "Int"}], "rows": [[1]], "tag": "SELECT 1"}],
//!     "error": null, "status": "Idle"}
//! -> {"op": "describe", "sql": "..."}
//! <- {"columns": [...], "params": 2, "error": null}
//! ```

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

use lodestone::sql::{Database, Output, Value, session::param_count};
use serde_json::{Value as Json, json};

fn to_json(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) if f.is_finite() => json!(f),
        Value::Float(f) => json!(lodestone::sql::types::format_float(*f)),
        Value::Text(s) => json!(s),
    }
}

fn from_json(j: &Json) -> Value {
    match j {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Json::String(s) => Value::Text(s.clone()),
        other => Value::Text(other.to_string()),
    }
}

fn error_json(e: &lodestone::sql::Error) -> Json {
    json!({"code": e.code, "message": e.message})
}

fn handle(db: Database, stream: TcpStream) -> std::io::Result<()> {
    let mut session = db.session();
    let mut out = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let req: Json = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                writeln!(out, "{}", json!({"error": {"code": "08P01", "message": format!("bad request: {e}")}}))?;
                continue;
            }
        };
        let sql = req["sql"].as_str().unwrap_or("");
        let reply = match req["op"].as_str() {
            Some("describe") => match session.describe(sql) {
                Ok((cols, params)) => json!({"columns": cols, "params": params, "error": null}),
                Err(e) => json!({"columns": [], "params": vec![Json::Null; param_count(sql)], "error": error_json(&e)}),
            },
            _ => {
                let params: Vec<Value> = req["params"].as_array().map(|a| a.iter().map(from_json).collect()).unwrap_or_default();
                let (outs, err) = session.run(sql, &params);
                let results: Vec<Json> = outs
                    .iter()
                    .map(|o| match o {
                        Output::Rows { columns, rows } => json!({
                            "columns": columns,
                            "rows": rows.iter().map(|r| r.iter().map(to_json).collect::<Vec<_>>()).collect::<Vec<_>>(),
                            "tag": format!("SELECT {}", rows.len()),
                        }),
                        Output::Command(tag) => json!({"tag": tag}),
                    })
                    .collect();
                json!({"results": results, "error": err.as_ref().map(error_json), "status": session.status()})
            }
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}

pub fn serve(path: &Path, listen: &str) -> ! {
    let db = Database::open(path).unwrap_or_else(|e| {
        eprintln!("cannot open {}: {e}", path.display());
        std::process::exit(1);
    });
    let listener = TcpListener::bind(listen).unwrap_or_else(|e| {
        eprintln!("cannot listen on {listen}: {e}");
        std::process::exit(1);
    });
    eprintln!("lodestone: {} on {listen}", path.display());
    for stream in listener.incoming().flatten() {
        let db = db.clone();
        let _ = stream.set_nodelay(true);
        std::thread::spawn(move || {
            let _ = handle(db, stream);
        });
    }
    unreachable!()
}
