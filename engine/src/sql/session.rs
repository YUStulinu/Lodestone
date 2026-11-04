//! Databases and sessions: statements run in an explicit transaction (BEGIN ... COMMIT) or each
//! in its own (autocommit; read-only statements get a read-only snapshot and no disk write).
//! As in PostgreSQL, an error inside an explicit transaction aborts it until ROLLBACK.

use std::path::Path;
use std::time::Instant;

use super::ast::{self, Statement};
use super::catalog::{self, Column as CatColumn, Index, Table, View, index_prefix};
use super::exec::{self, Ctx, build, eval_const};
use super::explain;
use super::lexer::{Token, lex};
use super::parser;
use super::plan::{Binder, Node, Plan};
use super::types::{DataType, Error, Result, Row, Value, err};
use crate::mvcc::{Mvcc, Range, Transaction};
use crate::storage::Options;
use crate::storage::vfs::{OsVfs, Vfs};

#[derive(Clone)]
pub struct Database {
    mvcc: Mvcc,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Column {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: DataType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Rows { columns: Vec<Column>, rows: Vec<Row> },
    Command(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TxnStatus {
    Idle,
    InTransaction,
    Failed,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        Database::open_with(&OsVfs, path.as_ref(), Options::default())
    }

    pub fn open_with(vfs: &dyn Vfs, path: &Path, opts: Options) -> Result<Database> {
        Ok(Database { mvcc: Mvcc::open(vfs, path, opts)? })
    }

    pub fn session(&self) -> Session {
        Session { db: self.clone(), txn: None, failed: false }
    }

    pub fn mvcc(&self) -> &Mvcc {
        &self.mvcc
    }
}

pub struct Session {
    db: Database,
    txn: Option<Transaction>,
    failed: bool,
}

/// Statements that only read run in a read-only snapshot when autocommitted.
fn read_only(s: &Statement) -> bool {
    match s {
        Statement::Select(_) | Statement::ShowTables => true,
        Statement::Explain { analyze, statement, .. } => !*analyze || read_only(statement),
        _ => false,
    }
}

/// The highest $n in a statement (its parameter count).
pub fn param_count(sql: &str) -> usize {
    lex(sql).map(|t| t.iter().filter_map(|t| if let Token::Param(n) = t { Some(*n) } else { None }).max().unwrap_or(0)).unwrap_or(0)
}

impl Session {
    pub fn status(&self) -> TxnStatus {
        match (&self.txn, self.failed) {
            (_, true) => TxnStatus::Failed,
            (Some(_), false) => TxnStatus::InTransaction,
            (None, false) => TxnStatus::Idle,
        }
    }

    /// Runs every statement; stops at the first error.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<Output>> {
        self.execute_params(sql, &[])
    }

    pub fn execute_params(&mut self, sql: &str, params: &[Value]) -> Result<Vec<Output>> {
        let (out, e) = self.run(sql, params);
        match e {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    /// Runs statements until one fails: the outputs so far, and the error if any.
    pub fn run(&mut self, sql: &str, params: &[Value]) -> (Vec<Output>, Option<Error>) {
        let stmts = match parser::parse(sql) {
            Ok(s) => s,
            Err(e) => {
                if self.txn.is_some() {
                    self.failed = true;
                }
                return (vec![], Some(e));
            }
        };
        let mut out = Vec::new();
        for s in &stmts {
            match self.statement(s, params) {
                Ok(o) => out.push(o),
                Err(e) => return (out, Some(e)),
            }
        }
        (out, None)
    }

    /// The columns a query would return, without running it (for the wire protocol's Describe).
    pub fn describe(&mut self, sql: &str) -> Result<Vec<Column>> {
        let stmts = parser::parse(sql)?;
        let Some(Statement::Select(q)) = stmts.first() else { return Ok(vec![]) };
        let fresh;
        let txn = match &self.txn {
            Some(t) => t,
            None => {
                fresh = self.db.mvcc.begin_read_only()?;
                &fresh
            }
        };
        let plan = Binder::new(txn, &self.db.mvcc).query(q, false)?;
        Ok(columns(&plan))
    }

    pub fn statement(&mut self, s: &Statement, params: &[Value]) -> Result<Output> {
        match s {
            Statement::Begin { read_only } => {
                if self.txn.is_none() {
                    self.txn = Some(if *read_only { self.db.mvcc.begin_read_only()? } else { self.db.mvcc.begin()? });
                    self.failed = false;
                }
                return Ok(Output::Command("BEGIN".into()));
            }
            Statement::Commit => {
                let failed = std::mem::take(&mut self.failed);
                return match self.txn.take() {
                    Some(t) if failed => {
                        t.rollback()?;
                        Ok(Output::Command("ROLLBACK".into()))
                    }
                    Some(t) => {
                        t.commit()?;
                        Ok(Output::Command("COMMIT".into()))
                    }
                    None => Ok(Output::Command("COMMIT".into())),
                };
            }
            Statement::Rollback => {
                self.failed = false;
                if let Some(t) = self.txn.take() {
                    t.rollback()?;
                }
                return Ok(Output::Command("ROLLBACK".into()));
            }
            _ => {}
        }
        if self.failed {
            return err!("25P02", "current transaction is aborted, commands ignored until end of transaction block");
        }
        match s {
            Statement::Vacuum | Statement::Checkpoint if self.txn.is_some() => {
                return err!("25001", "{} cannot run inside a transaction block", if matches!(s, Statement::Vacuum) { "VACUUM" } else { "CHECKPOINT" });
            }
            Statement::Vacuum => {
                self.db.mvcc.vacuum()?;
                return Ok(Output::Command("VACUUM".into()));
            }
            Statement::Checkpoint => {
                self.db.mvcc.checkpoint()?;
                return Ok(Output::Command("CHECKPOINT".into()));
            }
            _ => {}
        }
        if let Some(t) = &self.txn {
            let r = run(&self.db.mvcc, t, s, params);
            if r.is_err() {
                self.failed = true;
            }
            return r;
        }
        let txn = if read_only(s) { self.db.mvcc.begin_read_only()? } else { self.db.mvcc.begin()? };
        match run(&self.db.mvcc, &txn, s, params) {
            Ok(o) => {
                txn.commit()?;
                Ok(o)
            }
            Err(e) => {
                txn.rollback()?;
                Err(e)
            }
        }
    }
}

fn columns(plan: &Plan) -> Vec<Column> {
    plan.cols.iter().map(|c| Column { name: c.name.clone(), ty: c.ty.unwrap_or(DataType::Text) }).collect()
}

/// Binds a statement that has a plan (queries and writes).
fn plan_of(mvcc: &Mvcc, txn: &Transaction, s: &Statement) -> Result<Option<Plan>> {
    let mut b = Binder::new(txn, mvcc);
    let mut plan = match s {
        Statement::Select(q) => b.query(q, false)?,
        Statement::Insert { table, columns, source } => b.insert(table, columns, source)?,
        Statement::Update { table, alias, set, filter } => b.update(table, alias, set, filter)?,
        Statement::Delete { table, alias, filter } => b.delete(table, alias, filter)?,
        _ => return Ok(None),
    };
    plan.number();
    Ok(Some(plan))
}

/// Runs a planned statement.
fn execute(ctx: &Ctx, plan: &Plan) -> Result<Output> {
    Ok(match &plan.node {
        Node::Insert { table, input } => Output::Command(format!("INSERT 0 {}", exec::insert(ctx, table, input)?)),
        Node::Update { table, input, set } => Output::Command(format!("UPDATE {}", exec::update(ctx, table, input, set)?)),
        Node::Delete { table, input } => Output::Command(format!("DELETE {}", exec::delete(ctx, table, input)?)),
        _ => {
            let mut ex = build(plan, ctx)?;
            let mut rows = Vec::new();
            while let Some(r) = ex.next()? {
                rows.push(r);
            }
            Output::Rows { columns: columns(plan), rows }
        }
    })
}

fn run(mvcc: &Mvcc, txn: &Transaction, s: &Statement, params: &[Value]) -> Result<Output> {
    if let Some(plan) = plan_of(mvcc, txn, s)? {
        let ctx = Ctx::new(txn, mvcc, params, false);
        return execute(&ctx, &plan);
    }
    match s {
        Statement::Explain { analyze, json, statement } => {
            let t0 = Instant::now();
            let Some(plan) = plan_of(mvcc, txn, statement)? else {
                return err!("0A000", "EXPLAIN works on SELECT, INSERT, UPDATE and DELETE");
            };
            let planning = t0.elapsed();
            let ctx = Ctx::new(txn, mvcc, params, *analyze);
            let mut elapsed = None;
            if *analyze {
                let t = Instant::now();
                execute(&ctx, &plan)?;
                elapsed = Some(t.elapsed());
            }
            let stats = ctx.stats.as_ref().map(|s| s.borrow().clone());
            let text = if *json {
                let mut top = serde_json::json!({ "Plan": explain::json(&plan, stats.as_ref()) });
                top["Planning Time"] = serde_json::json!((planning.as_secs_f64() * 1e6).round() / 1e3);
                if let Some(e) = elapsed {
                    top["Execution Time"] = serde_json::json!((e.as_secs_f64() * 1e6).round() / 1e3);
                }
                vec![serde_json::to_string_pretty(&serde_json::json!([top])).unwrap()]
            } else {
                let mut lines = explain::text(&plan, stats.as_ref());
                lines.push(format!("Planning Time: {:.3} ms", planning.as_secs_f64() * 1e3));
                if let Some(e) = elapsed {
                    lines.push(format!("Execution Time: {:.3} ms", e.as_secs_f64() * 1e3));
                }
                lines
            };
            Ok(Output::Rows {
                columns: vec![Column { name: "QUERY PLAN".into(), ty: DataType::Text }],
                rows: text.into_iter().map(|l| vec![Value::Text(l)]).collect(),
            })
        }
        Statement::CreateTable { name, if_not_exists, columns, primary_key, unique } => {
            if catalog::get_table(txn, name)?.is_some() || catalog::get_view(txn, name)?.is_some() {
                if *if_not_exists {
                    return Ok(Output::Command("CREATE TABLE".into()));
                }
                return err!("42P07", "relation \"{name}\" already exists");
            }
            create_table(mvcc, txn, name, columns, primary_key, unique)?;
            Ok(Output::Command("CREATE TABLE".into()))
        }
        Statement::DropTable { name, if_exists } => {
            let Some(t) = catalog::get_table(txn, name)? else {
                if *if_exists {
                    return Ok(Output::Command("DROP TABLE".into()));
                }
                return err!("42P01", "table \"{name}\" does not exist");
            };
            let ctx = Ctx::new(txn, mvcc, params, false);
            exec::truncate(&ctx, &t)?;
            catalog::delete_table(txn, name)?;
            Ok(Output::Command("DROP TABLE".into()))
        }
        Statement::CreateIndex { name, table, columns, unique, if_not_exists } => {
            let mut t = catalog::table(txn, table)?;
            let all = catalog::tables(txn)?;
            if all.iter().any(|t| t.indexes.iter().any(|i| &i.name == name)) || all.iter().any(|t| &t.name == name) {
                if *if_not_exists {
                    return Ok(Output::Command("CREATE INDEX".into()));
                }
                return err!("42P07", "relation \"{name}\" already exists");
            }
            let cols = columns
                .iter()
                .map(|c| t.column(c).ok_or_else(|| Error::new("42703", format!("column \"{c}\" does not exist"))))
                .collect::<Result<Vec<_>>>()?;
            let id = mvcc.bump(b"index_id", 1)? as u32;
            t.indexes.push(Index { id, name: name.clone(), columns: cols, unique: *unique });
            let ctx = Ctx::new(txn, mvcc, params, false);
            exec::build_index(&ctx, &t, t.indexes.len() - 1)?;
            catalog::put_table(txn, &t)?;
            Ok(Output::Command("CREATE INDEX".into()))
        }
        Statement::DropIndex { name, if_exists } => {
            for mut t in catalog::tables(txn)? {
                if let Some(pos) = t.indexes.iter().position(|i| &i.name == name) {
                    let ix = t.indexes.remove(pos);
                    let prefix = index_prefix(ix.id);
                    let range = Range { prefix: &prefix, from: std::ops::Bound::Unbounded, to: std::ops::Bound::Unbounded };
                    for (k, _) in txn.scan(&range, usize::MAX)? {
                        txn.delete(&[prefix.as_slice(), &k].concat())?;
                    }
                    catalog::put_table(txn, &t)?;
                    return Ok(Output::Command("DROP INDEX".into()));
                }
            }
            if *if_exists {
                return Ok(Output::Command("DROP INDEX".into()));
            }
            err!("42704", "index \"{name}\" does not exist")
        }
        Statement::CreateView { name, if_not_exists, query, sql } => {
            if catalog::get_table(txn, name)?.is_some() || catalog::get_view(txn, name)?.is_some() {
                if *if_not_exists {
                    return Ok(Output::Command("CREATE VIEW".into()));
                }
                return err!("42P07", "relation \"{name}\" already exists");
            }
            let plan = Binder::new(txn, mvcc).query(query, false)?;
            let view = View { name: name.clone(), sql: sql.clone(), columns: plan.cols.iter().map(|c| c.name.clone()).collect() };
            catalog::put_view(txn, &view)?;
            Ok(Output::Command("CREATE VIEW".into()))
        }
        Statement::DropView { name, if_exists } => {
            if catalog::get_view(txn, name)?.is_none() {
                if *if_exists {
                    return Ok(Output::Command("DROP VIEW".into()));
                }
                return err!("42P01", "view \"{name}\" does not exist");
            }
            catalog::delete_view(txn, name)?;
            Ok(Output::Command("DROP VIEW".into()))
        }
        Statement::ShowTables => {
            let rows = catalog::tables(txn)?.into_iter().map(|t| vec![Value::Text(t.name)]).collect();
            Ok(Output::Rows { columns: vec![Column { name: "table".into(), ty: DataType::Text }], rows })
        }
        Statement::Reindex => Ok(Output::Command("REINDEX".into())),
        _ => err!("XX000", "unexpected statement"),
    }
}

fn create_table(mvcc: &Mvcc, txn: &Transaction, name: &str, defs: &[ast::ColumnDef], pk: &[String], unique: &[Vec<String>]) -> Result<()> {
    let mut columns = Vec::new();
    for d in defs {
        if columns.iter().any(|c: &CatColumn| c.name == d.name) {
            return err!("42701", "column \"{}\" specified more than once", d.name);
        }
        let default = match &d.default {
            None => None,
            Some(e) => {
                let mut b = Binder::new(txn, mvcc);
                let v = eval_const(&b.constant(e)?)?;
                Some(v.cast(d.data_type)?)
            }
        };
        columns.push(CatColumn { name: d.name.clone(), data_type: d.data_type, nullable: d.nullable, default });
    }
    let names: Vec<String> = columns.iter().map(|c: &CatColumn| c.name.clone()).collect();
    let index_of = |n: &String| names.iter().position(|c| c == n).ok_or_else(|| Error::new("42703", format!("column \"{n}\" named in key does not exist")));
    let inline: Vec<usize> = defs.iter().enumerate().filter(|(_, d)| d.primary_key).map(|(i, _)| i).collect();
    if !inline.is_empty() && !pk.is_empty() || inline.len() > 1 {
        return err!("42P16", "multiple primary keys for table \"{name}\" are not allowed");
    }
    let primary_key: Vec<usize> = if pk.is_empty() { inline } else { pk.iter().map(index_of).collect::<Result<_>>()? };
    for &c in &primary_key {
        columns[c].nullable = false;
    }
    let id = mvcc.bump(b"table_id", 1)? as u32;
    let mut indexes = Vec::new();
    let mut uniques: Vec<Vec<usize>> = defs.iter().enumerate().filter(|(_, d)| d.unique).map(|(i, _)| vec![i]).collect();
    for u in unique {
        uniques.push(u.iter().map(index_of).collect::<Result<_>>()?);
    }
    for cols in uniques {
        let names: Vec<&str> = cols.iter().map(|&c| columns[c].name.as_str()).collect();
        let ix_id = mvcc.bump(b"index_id", 1)? as u32;
        indexes.push(Index { id: ix_id, name: format!("{name}_{}_key", names.join("_")), columns: cols, unique: true });
    }
    catalog::put_table(txn, &Table { id, name: name.into(), columns, primary_key, indexes })?;
    Ok(())
}
