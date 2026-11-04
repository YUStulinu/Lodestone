//! The executor: each plan node becomes an iterator that pulls rows from its children.
//! Table scans read in batches, so a scan never holds the storage lock between rows.
//! Uncorrelated subqueries run once per statement and are cached; correlated ones run once per
//! row with that row pushed on the outer-row stack.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Bound;
use std::time::Instant;

use super::ast::{BinOp, SetOp};
use super::catalog::{self, Table, index_prefix, row_prefix};
use super::plan::*;
use super::types::{DataType, Error, Result, Row, Value, decode_row, encode_row, err};
use crate::keycode::prefix_end;
use crate::mvcc::{Mvcc, Range, Transaction};

/// Rows a scan reads per trip to storage.
const SCAN_BATCH: usize = 256;

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct NodeStats {
    pub rows: u64,
    pub loops: u64,
    pub nanos: u64,
}

enum Cached {
    Scalar(Value),
    Exists(bool),
    In(HashSet<Value>, bool),
}

pub struct Ctx<'a> {
    pub txn: &'a Transaction,
    pub mvcc: &'a Mvcc,
    pub params: &'a [Value],
    outer: RefCell<Vec<Row>>,
    cache: RefCell<HashMap<usize, Cached>>,
    pub stats: Option<RefCell<HashMap<usize, NodeStats>>>,
}

impl<'a> Ctx<'a> {
    pub fn new(txn: &'a Transaction, mvcc: &'a Mvcc, params: &'a [Value], analyze: bool) -> Ctx<'a> {
        Ctx {
            txn,
            mvcc,
            params,
            outer: RefCell::new(Vec::new()),
            cache: RefCell::new(HashMap::new()),
            stats: analyze.then(|| RefCell::new(HashMap::new())),
        }
    }

    fn record(&self, id: usize, rows: u64, loops: u64, nanos: u64) {
        if let Some(s) = &self.stats {
            let mut s = s.borrow_mut();
            let e = s.entry(id).or_default();
            e.rows += rows;
            e.loops += loops;
            e.nanos += nanos;
        }
    }
}

/// What expression evaluation needs from its surroundings.
pub trait Env {
    fn param(&self, i: usize) -> Result<Value>;
    fn outer(&self, depth: usize, i: usize) -> Result<Value>;
    fn subquery(&self, sp: &SubPlan, left: Option<Value>, row: &[Value]) -> Result<Value>;
}

struct NoEnv;

impl Env for NoEnv {
    fn param(&self, i: usize) -> Result<Value> {
        err!("42P02", "there is no parameter ${i}")
    }
    fn outer(&self, _: usize, _: usize) -> Result<Value> {
        err!("XX000", "outer reference in a constant")
    }
    fn subquery(&self, _: &SubPlan, _: Option<Value>, _: &[Value]) -> Result<Value> {
        err!("XX000", "subquery in a constant")
    }
}

pub fn eval_const(e: &Expr) -> Result<Value> {
    eval(e, &[], &NoEnv)
}

impl Env for Ctx<'_> {
    fn param(&self, i: usize) -> Result<Value> {
        self.params.get(i - 1).cloned().ok_or_else(|| Error::new("08P01", format!("there is no parameter ${i}")))
    }

    fn outer(&self, depth: usize, i: usize) -> Result<Value> {
        let outer = self.outer.borrow();
        let n = outer.len();
        if depth == 0 || depth > n {
            return err!("XX000", "outer reference beyond the query");
        }
        Ok(outer[n - depth].get(i).cloned().unwrap_or(Value::Null))
    }

    fn subquery(&self, sp: &SubPlan, left: Option<Value>, row: &[Value]) -> Result<Value> {
        let cached = if sp.correlated { None } else { self.cache.borrow_mut().remove(&sp.id) };
        let result = match cached {
            Some(c) => c,
            None => {
                self.outer.borrow_mut().push(row.to_vec());
                let r = self.run_subquery(sp);
                self.outer.borrow_mut().pop();
                r?
            }
        };
        let value = match &result {
            Cached::Scalar(v) => v.clone(),
            Cached::Exists(b) => Value::Bool(*b != sp.negated),
            Cached::In(set, has_null) => {
                let left = left.unwrap_or(Value::Null);
                let v = if set.is_empty() && !*has_null {
                    Value::Bool(false)
                } else if left.is_null() {
                    Value::Null
                } else if set.contains(&left) || set.iter().any(|x| x.compare(&left).ok().flatten() == Some(Ordering::Equal)) {
                    Value::Bool(true)
                } else if *has_null {
                    Value::Null
                } else {
                    Value::Bool(false)
                };
                match (v, sp.negated) {
                    (Value::Bool(b), true) => Value::Bool(!b),
                    (v, _) => v,
                }
            }
        };
        if !sp.correlated {
            self.cache.borrow_mut().insert(sp.id, result);
        }
        Ok(value)
    }
}

impl Ctx<'_> {
    fn run_subquery(&self, sp: &SubPlan) -> Result<Cached> {
        let mut ex = build(&sp.plan, self)?;
        Ok(match sp.kind {
            SubKind::Scalar => {
                let first = ex.next()?;
                if first.is_some() && ex.next()?.is_some() {
                    return err!("21000", "more than one row returned by a subquery used as an expression");
                }
                Cached::Scalar(first.and_then(|r| r.into_iter().next()).unwrap_or(Value::Null))
            }
            SubKind::Exists => Cached::Exists(ex.next()?.is_some()),
            SubKind::In => {
                let (mut set, mut has_null) = (HashSet::new(), false);
                while let Some(r) = ex.next()? {
                    let v = r.into_iter().next().unwrap_or(Value::Null);
                    if v.is_null() {
                        has_null = true;
                    } else {
                        set.insert(v);
                    }
                }
                Cached::In(set, has_null)
            }
        })
    }
}

// ---- expressions ---------------------------------------------------------------------------

fn arith(op: BinOp, a: Value, b: Value) -> Result<Value> {
    use Value::*;
    let num = |v: &Value| -> Result<Value> {
        match v {
            Text(s) => Value::Text(s.clone()).cast(if s.contains(['.', 'e', 'E']) { DataType::Float } else { DataType::Int }),
            Bool(_) => err!("42883", "operator does not exist: boolean {op:?} ..."),
            v => Ok(v.clone()),
        }
    };
    let (a, b) = (num(&a)?, num(&b)?);
    let overflow = || Error::new("22003", "integer out of range");
    Ok(match (a, b) {
        (Null, _) | (_, Null) => Null,
        (Int(x), Int(y)) => match op {
            BinOp::Add => Int(x.checked_add(y).ok_or_else(overflow)?),
            BinOp::Sub => Int(x.checked_sub(y).ok_or_else(overflow)?),
            BinOp::Mul => Int(x.checked_mul(y).ok_or_else(overflow)?),
            BinOp::Div => {
                if y == 0 {
                    return err!("22012", "division by zero");
                }
                Int(x.checked_div(y).ok_or_else(overflow)?)
            }
            _ => {
                if y == 0 {
                    return err!("22012", "division by zero");
                }
                Int(x.checked_rem(y).unwrap_or(0))
            }
        },
        (x, y) => {
            let (x, y) = (as_f64(&x), as_f64(&y));
            Float(match op {
                BinOp::Add => x + y,
                BinOp::Sub => x - y,
                BinOp::Mul => x * y,
                BinOp::Div => {
                    if y == 0.0 {
                        return err!("22012", "division by zero");
                    }
                    x / y
                }
                _ => {
                    if y == 0.0 {
                        return err!("22012", "division by zero");
                    }
                    x % y
                }
            })
        }
    })
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => f64::NAN,
    }
}

fn like(s: &str, p: &str, ci: bool) -> bool {
    let (s, p): (Vec<char>, Vec<char>) = if ci {
        (s.to_lowercase().chars().collect(), p.to_lowercase().chars().collect())
    } else {
        (s.chars().collect(), p.chars().collect())
    };
    // Iterative matching with backtracking to the last %.
    let (mut i, mut j) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while i < s.len() {
        if j < p.len() && p[j] == '\\' && j + 1 < p.len() && p[j + 1] == s[i] {
            i += 1;
            j += 2;
        } else if j < p.len() && (p[j] == '_' || (p[j] == s[i] && p[j] != '%' && p[j] != '\\')) {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '%' {
            star = Some(j);
            mark = i;
            j += 1;
        } else if let Some(sj) = star {
            j = sj + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '%' {
        j += 1;
    }
    j == p.len()
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Text(s) => Some(s.clone()),
        Value::Float(f) => Some(super::types::format_float(*f)),
        Value::Int(i) => Some(i.to_string()),
        Value::Bool(b) => Some(if *b { "true".into() } else { "false".into() }),
    }
}

fn func(f: Func, args: Vec<Value>) -> Result<Value> {
    use Value::*;
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Null);
    Ok(match f {
        Func::Coalesce => args.into_iter().find(|v| !v.is_null()).unwrap_or(Null),
        Func::NullIf => {
            if arg(0).compare(&arg(1))? == Some(Ordering::Equal) {
                Null
            } else {
                arg(0)
            }
        }
        Func::Least | Func::Greatest => {
            let mut best: Option<Value> = None;
            for v in args {
                if v.is_null() {
                    continue;
                }
                best = Some(match best {
                    None => v,
                    Some(b) => {
                        let c = v.compare(&b)?.unwrap_or(Ordering::Equal);
                        if (f == Func::Least && c == Ordering::Less) || (f == Func::Greatest && c == Ordering::Greater) { v } else { b }
                    }
                });
            }
            best.unwrap_or(Null)
        }
        Func::Version => Text(format!("PostgreSQL 16.0 (Lodestone {})", env!("CARGO_PKG_VERSION"))),
        Func::Typeof => Text(match arg(0) {
            Null => "null",
            Bool(_) => "boolean",
            Int(_) => "integer",
            Float(_) => "real",
            Text(_) => "text",
        }
        .into()),
        _ if arg(0).is_null() => Null,
        Func::Abs => match arg(0) {
            Int(i) => Int(i.checked_abs().ok_or_else(|| Error::new("22003", "integer out of range"))?),
            Float(x) => Float(x.abs()),
            v => Float(as_f64(&v.cast(DataType::Float)?).abs()),
        },
        Func::Round => {
            let digits = match arg(1) {
                Null => 0,
                v => match v.cast(DataType::Int)? {
                    Int(d) => d,
                    _ => 0,
                },
            };
            match arg(0) {
                Int(i) if digits >= 0 => Int(i),
                v => {
                    let x = as_f64(&v.cast(DataType::Float)?);
                    let m = 10f64.powi(digits as i32);
                    Float((x * m).round() / m)
                }
            }
        }
        Func::Length => Int(text(&arg(0)).unwrap().chars().count() as i64),
        Func::Lower => Text(text(&arg(0)).unwrap().to_lowercase()),
        Func::Upper => Text(text(&arg(0)).unwrap().to_uppercase()),
        Func::Trim => Text(text(&arg(0)).unwrap().trim().into()),
        Func::LTrim => Text(text(&arg(0)).unwrap().trim_start().into()),
        Func::RTrim => Text(text(&arg(0)).unwrap().trim_end().into()),
        Func::Replace => match (text(&arg(0)), text(&arg(1)), text(&arg(2))) {
            (Some(s), Some(a), Some(b)) => Text(s.replace(&a, &b)),
            _ => Null,
        },
        Func::Substr => {
            let s: Vec<char> = text(&arg(0)).unwrap().chars().collect();
            let start = match arg(1).cast(DataType::Int)? {
                Int(i) => i,
                _ => return Ok(Null),
            };
            let len = match arg(2) {
                Null if args.len() < 3 => i64::MAX / 2,
                Null => return Ok(Null),
                v => match v.cast(DataType::Int)? {
                    Int(l) => l,
                    _ => 0,
                },
            };
            // 1-based; characters before position 1 count against the length (SQL standard).
            let from = start.max(1);
            let to = (start.saturating_add(len)).max(from);
            let (from, to) = ((from - 1) as usize, ((to - 1) as usize).min(s.len()));
            Text(if from >= s.len() || from >= to { String::new() } else { s[from..to].iter().collect() })
        }
    })
}

pub fn eval(e: &Expr, row: &[Value], env: &dyn Env) -> Result<Value> {
    use Value::*;
    Ok(match e {
        Expr::Const(v) => v.clone(),
        Expr::Col(i) => row.get(*i).cloned().unwrap_or(Null),
        Expr::Outer(d, i) => env.outer(*d, *i)?,
        Expr::Param(i) => env.param(*i)?,
        Expr::AggRef(_) => return err!("XX000", "aggregate outside an aggregation"),
        Expr::Neg(x) => match eval(x, row, env)? {
            Null => Null,
            Int(i) => Int(i.checked_neg().ok_or_else(|| Error::new("22003", "integer out of range"))?),
            Float(f) => Float(-f),
            v => arith(BinOp::Sub, Int(0), v)?,
        },
        Expr::Not(x) => match eval(x, row, env)?.truth()? {
            None => Null,
            Some(b) => Bool(!b),
        },
        Expr::Bin(BinOp::And, a, b) => {
            let l = eval(a, row, env)?.truth()?;
            if l == Some(false) {
                return Ok(Bool(false));
            }
            match (l, eval(b, row, env)?.truth()?) {
                (_, Some(false)) => Bool(false),
                (Some(true), Some(true)) => Bool(true),
                _ => Null,
            }
        }
        Expr::Bin(BinOp::Or, a, b) => {
            let l = eval(a, row, env)?.truth()?;
            if l == Some(true) {
                return Ok(Bool(true));
            }
            match (l, eval(b, row, env)?.truth()?) {
                (_, Some(true)) => Bool(true),
                (Some(false), Some(false)) => Bool(false),
                _ => Null,
            }
        }
        Expr::Bin(op, a, b) => {
            let (l, r) = (eval(a, row, env)?, eval(b, row, env)?);
            match op {
                BinOp::Is | BinOp::IsNot => {
                    let same = match (&l, &r) {
                        (Null, Null) => true,
                        (Null, _) | (_, Null) => false,
                        _ => l.compare(&r)? == Some(Ordering::Equal),
                    };
                    Bool(same == (*op == BinOp::Is))
                }
                BinOp::Eq | BinOp::NotEq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => match l.compare(&r)? {
                    None => Null,
                    Some(o) => Bool(match op {
                        BinOp::Eq => o == Ordering::Equal,
                        BinOp::NotEq => o != Ordering::Equal,
                        BinOp::Lt => o == Ordering::Less,
                        BinOp::LtEq => o != Ordering::Greater,
                        BinOp::Gt => o == Ordering::Greater,
                        _ => o != Ordering::Less,
                    }),
                },
                BinOp::Concat => match (text(&l), text(&r)) {
                    (Some(a), Some(b)) => Text(a + &b),
                    _ => Null,
                },
                _ => arith(*op, l, r)?,
            }
        }
        Expr::Func(f, args) => func(*f, args.iter().map(|a| eval(a, row, env)).collect::<Result<_>>()?)?,
        Expr::Case(operand, whens, else_) => {
            let op = operand.as_ref().map(|o| eval(o, row, env)).transpose()?;
            for (w, t) in whens {
                let hit = match &op {
                    Some(v) => v.compare(&eval(w, row, env)?)? == Some(Ordering::Equal),
                    None => eval(w, row, env)?.truth()? == Some(true),
                };
                if hit {
                    return eval(t, row, env);
                }
            }
            match else_ {
                Some(e) => eval(e, row, env)?,
                None => Null,
            }
        }
        Expr::Cast(x, t) => eval(x, row, env)?.cast(*t)?,
        Expr::InList(x, list, negated) => {
            let v = eval(x, row, env)?;
            if v.is_null() {
                return Ok(Null);
            }
            let mut saw_null = false;
            let mut found = false;
            for item in list {
                match v.compare(&eval(item, row, env)?)? {
                    Some(Ordering::Equal) => {
                        found = true;
                        break;
                    }
                    None => saw_null = true,
                    _ => {}
                }
            }
            if found {
                Bool(!negated)
            } else if saw_null {
                Null
            } else {
                Bool(*negated)
            }
        }
        Expr::Like(x, p, negated, ci) => match (text(&eval(x, row, env)?), text(&eval(p, row, env)?)) {
            (Some(s), Some(p)) => Bool(like(&s, &p, *ci) != *negated),
            _ => Null,
        },
        Expr::IsNull(x, negated) => Bool(eval(x, row, env)?.is_null() != *negated),
        Expr::Sub(sp, left) => {
            let l = left.as_ref().map(|l| eval(l, row, env)).transpose()?;
            env.subquery(sp, l, row)?
        }
    })
}

fn passes(pred: &Expr, row: &[Value], env: &dyn Env) -> Result<bool> {
    Ok(eval(pred, row, env)?.truth()? == Some(true))
}

// ---- executors -----------------------------------------------------------------------------

pub trait Exec {
    fn next(&mut self) -> Result<Option<Row>>;
}

struct Instrumented<'a> {
    inner: Box<dyn Exec + 'a>,
    ctx: &'a Ctx<'a>,
    id: usize,
}

impl Exec for Instrumented<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        let t = Instant::now();
        let r = self.inner.next();
        let rows = matches!(r, Ok(Some(_))) as u64;
        self.ctx.record(self.id, rows, 0, t.elapsed().as_nanos() as u64);
        r
    }
}

pub fn build<'a>(plan: &'a Plan, ctx: &'a Ctx<'a>) -> Result<Box<dyn Exec + 'a>> {
    let t = Instant::now();
    let inner: Box<dyn Exec + 'a> = match &plan.node {
        Node::Scan { table, access, filter, .. } => Box::new(Scan::new(ctx, table, access, filter.as_ref())?),
        Node::Values { rows } => Box::new(Values { ctx, rows, i: 0 }),
        Node::Filter { input, pred } => Box::new(Filter { input: build(input, ctx)?, pred, ctx }),
        Node::Project { input, exprs } => Box::new(Project { input: build(input, ctx)?, exprs, ctx }),
        Node::NestedLoop { left, right, kind, on } => Box::new(NestedLoop {
            left: build(left, ctx)?,
            right_plan: right,
            right: None,
            ctx,
            kind: *kind,
            on: on.as_ref(),
            current: None,
            pos: 0,
            matched: false,
            right_width: right.cols.len(),
        }),
        Node::HashJoin { left, right, kind, left_keys, right_keys, residual } => Box::new(HashJoin {
            left: build(left, ctx)?,
            right_plan: right,
            table: None,
            ctx,
            kind: *kind,
            left_keys,
            right_keys,
            residual: residual.as_ref(),
            pending: VecDeque::new(),
            right_width: right.cols.len(),
        }),
        Node::Aggregate { input, group, aggs } => Box::new(Aggregate { input: Some(build(input, ctx)?), group, aggs, ctx, out: VecDeque::new() }),
        Node::Sort { input, keys } => Box::new(Sort { input: Some(build(input, ctx)?), keys, ctx, out: VecDeque::new() }),
        Node::Limit { input, limit, offset } => {
            let n = |e: &Option<Expr>, what: &str| -> Result<Option<u64>> {
                match e.as_ref().map(|e| eval(e, &[], ctx)).transpose()? {
                    None | Some(Value::Null) => Ok(None),
                    Some(v) => match v.cast(DataType::Int)? {
                        Value::Int(i) if i >= 0 => Ok(Some(i as u64)),
                        _ => err!("2201W", "{what} must not be negative"),
                    },
                }
            };
            Box::new(Limit { input: build(input, ctx)?, limit: n(limit, "LIMIT")?, skip: n(offset, "OFFSET")?.unwrap_or(0), taken: 0 })
        }
        Node::Distinct { input } => Box::new(Distinct { input: build(input, ctx)?, seen: HashSet::new() }),
        Node::SetOp { op, all, left, right } => Box::new(SetOpExec::new(*op, *all, build(left, ctx)?, build(right, ctx)?)?),
        Node::Insert { .. } | Node::Update { .. } | Node::Delete { .. } => return err!("XX000", "a write is not a query"),
    };
    if ctx.stats.is_some() {
        ctx.record(plan.id, 0, 1, t.elapsed().as_nanos() as u64);
        return Ok(Box::new(Instrumented { inner, ctx, id: plan.id }));
    }
    Ok(inner)
}

/// Where a scan reads from.
enum Source {
    /// MVCC ranges of row keys under the table's prefix.
    Ranges(VecDeque<(Bound<Vec<u8>>, Bound<Vec<u8>>)>),
    /// Specific row keys.
    Keys(VecDeque<Vec<u8>>),
    /// Index ranges, each giving row keys.
    Index(VecDeque<(Vec<u8>, Bound<Vec<u8>>, Bound<Vec<u8>>)>),
}

pub struct Scan<'a> {
    ctx: &'a Ctx<'a>,
    prefix: Vec<u8>,
    source: Source,
    filter: Option<&'a Expr>,
    buf: VecDeque<(Vec<u8>, Row)>,
    /// Where the current range continues from.
    resume: Option<(Bound<Vec<u8>>, Bound<Vec<u8>>)>,
}

/// Encodes a lookup value for a column's key order, or None if it cannot be (then the scan
/// falls back to reading everything and letting the filter decide).
fn key_value(v: Value, ty: DataType) -> Option<Value> {
    match (&v, ty) {
        (Value::Null, _) => Some(Value::Null),
        (Value::Int(_), DataType::Int) | (Value::Float(_), DataType::Float) | (Value::Text(_), DataType::Text) | (Value::Bool(_), DataType::Bool) => Some(v),
        (Value::Int(i), DataType::Float) => Some(Value::Float(*i as f64)),
        (Value::Float(f), DataType::Int) if f.fract() == 0.0 && f.abs() < 9e18 => Some(Value::Int(*f as i64)),
        _ => None,
    }
}

/// A range bound for one key column. Float bounds on an integer column round inwards.
fn bound_for(b: &Option<(Expr, bool)>, ty: DataType, ctx: &Ctx, lower: bool) -> Result<Option<Bound<Vec<u8>>>> {
    let Some((e, inclusive)) = b else { return Ok(Some(Bound::Unbounded)) };
    let v = eval(e, &[], ctx)?;
    if v.is_null() {
        return Ok(None); // comparing with NULL matches nothing
    }
    let (v, inclusive) = match (&v, ty) {
        (Value::Float(f), DataType::Int) if f.fract() != 0.0 => (Value::Int(if lower { f.ceil() } else { f.floor() } as i64), true),
        _ => match key_value(v.clone(), ty) {
            Some(v) => (v, *inclusive),
            None => return Ok(Some(Bound::Unbounded)),
        },
    };
    let mut k = Vec::new();
    v.encode_key(&mut k);
    Ok(Some(match (lower, inclusive) {
        (true, true) => Bound::Included(k),
        // Keys may continue after the value (more columns, the row key): skip all of them.
        (true, false) => Bound::Included(prefix_end(&k).unwrap()),
        (false, true) => Bound::Excluded(prefix_end(&k).unwrap()),
        (false, false) => Bound::Excluded(k),
    }))
}

impl<'a> Scan<'a> {
    pub fn new(ctx: &'a Ctx<'a>, table: &'a Table, access: &'a Access, filter: Option<&'a Expr>) -> Result<Scan<'a>> {
        let prefix = row_prefix(table.id);
        let ty = |c: usize| table.columns[c].data_type;
        // Lower bound past NULL keys (NULL never satisfies a comparison).
        let not_null = || Bound::Included(vec![0x02]);
        let full = || Source::Ranges(VecDeque::from([(Bound::Unbounded, Bound::Unbounded)]));
        let source = match access {
            Access::Full => full(),
            Access::Keys(keys) => {
                let mut out = VecDeque::new();
                let mut fallback = false;
                'k: for tuple in keys {
                    let mut k = prefix.clone();
                    for (e, &c) in tuple.iter().zip(&table.primary_key) {
                        match key_value(eval(e, &[], ctx)?, ty(c)) {
                            Some(Value::Null) => continue 'k,
                            Some(v) => v.encode_key(&mut k),
                            None => {
                                fallback = true;
                                break 'k;
                            }
                        }
                    }
                    out.push_back(k);
                }
                if fallback { full() } else { Source::Keys(out) }
            }
            Access::KeyRange { lo, hi } => {
                let c = table.primary_key[0];
                match (bound_for(lo, ty(c), ctx, true)?, bound_for(hi, ty(c), ctx, false)?) {
                    (Some(l), Some(h)) => Source::Ranges(VecDeque::from([(l, h)])),
                    _ => Source::Keys(VecDeque::new()),
                }
            }
            Access::IndexEq { index, keys } => {
                let ix = &table.indexes[*index];
                let mut out = VecDeque::new();
                let mut fallback = false;
                'k: for tuple in keys {
                    let mut k = index_prefix(ix.id);
                    for (e, &c) in tuple.iter().zip(&ix.columns) {
                        match key_value(eval(e, &[], ctx)?, ty(c)) {
                            Some(Value::Null) => continue 'k,
                            Some(v) => v.encode_key(&mut k),
                            None => {
                                fallback = true;
                                break 'k;
                            }
                        }
                    }
                    let end = prefix_end(&k).unwrap();
                    out.push_back((Vec::new(), Bound::Included(k), Bound::Excluded(end)));
                }
                if fallback { full() } else { Source::Index(out) }
            }
            Access::IndexRange { index, lo, hi } => {
                let ix = &table.indexes[*index];
                let c = ix.columns[0];
                let p = index_prefix(ix.id);
                match (bound_for(lo, ty(c), ctx, true)?, bound_for(hi, ty(c), ctx, false)?) {
                    (Some(l), Some(h)) => {
                        let l = match l {
                            Bound::Unbounded => not_null(),
                            b => b,
                        };
                        Source::Index(VecDeque::from([(p, l, h)]))
                    }
                    _ => Source::Keys(VecDeque::new()),
                }
            }
        };
        Ok(Scan { ctx, prefix, source, filter, buf: VecDeque::new(), resume: None })
    }

    fn refill(&mut self) -> Result<bool> {
        let txn = self.ctx.txn;
        match &mut self.source {
            Source::Ranges(ranges) => loop {
                let (from, to) = match self.resume.take() {
                    Some(r) => r,
                    None => match ranges.pop_front() {
                        Some(r) => r,
                        None => return Ok(false),
                    },
                };
                let range = Range { prefix: &self.prefix, from: as_ref(&from), to: as_ref(&to) };
                let batch = txn.scan(&range, SCAN_BATCH)?;
                if batch.len() == SCAN_BATCH {
                    self.resume = Some((Bound::Excluded(batch.last().unwrap().0.clone()), to));
                }
                for (k, v) in batch {
                    let mut key = self.prefix.clone();
                    key.extend_from_slice(&k);
                    self.buf.push_back((key, decode_row(&v)?));
                }
                if !self.buf.is_empty() {
                    return Ok(true);
                }
            },
            Source::Keys(keys) => {
                while let Some(k) = keys.pop_front() {
                    if let Some(v) = txn.get(&k)? {
                        self.buf.push_back((k, decode_row(&v)?));
                        if self.buf.len() >= SCAN_BATCH {
                            break;
                        }
                    }
                }
                Ok(!self.buf.is_empty())
            }
            Source::Index(ranges) => {
                while let Some((p, from, to)) = ranges.pop_front() {
                    // A full-key range (p empty) or a range within the index prefix.
                    let (prefix, from, to) = if p.is_empty() {
                        (Vec::new(), from, to)
                    } else {
                        (p, from, to)
                    };
                    let range = Range { prefix: &prefix, from: as_ref(&from), to: as_ref(&to) };
                    for (_, row_suffix) in txn.scan(&range, usize::MAX)? {
                        let mut key = self.prefix.clone();
                        key.extend_from_slice(&row_suffix);
                        if let Some(v) = txn.get(&key)? {
                            self.buf.push_back((key, decode_row(&v)?));
                        }
                    }
                    if !self.buf.is_empty() {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
        }
    }

    /// The next row that passes the filter, with its storage key.
    pub fn next_keyed(&mut self) -> Result<Option<(Vec<u8>, Row)>> {
        loop {
            while let Some((k, row)) = self.buf.pop_front() {
                if let Some(f) = self.filter {
                    if !passes(f, &row, self.ctx)? {
                        continue;
                    }
                }
                return Ok(Some((k, row)));
            }
            if !self.refill()? {
                return Ok(None);
            }
        }
    }
}

fn as_ref(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
    match b {
        Bound::Included(k) => Bound::Included(k),
        Bound::Excluded(k) => Bound::Excluded(k),
        Bound::Unbounded => Bound::Unbounded,
    }
}

impl Exec for Scan<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        Ok(self.next_keyed()?.map(|(_, r)| r))
    }
}

struct Values<'a> {
    ctx: &'a Ctx<'a>,
    rows: &'a [Vec<Expr>],
    i: usize,
}

impl Exec for Values<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        let Some(r) = self.rows.get(self.i) else { return Ok(None) };
        self.i += 1;
        Ok(Some(r.iter().map(|e| eval(e, &[], self.ctx)).collect::<Result<_>>()?))
    }
}

struct Filter<'a> {
    input: Box<dyn Exec + 'a>,
    pred: &'a Expr,
    ctx: &'a Ctx<'a>,
}

impl Exec for Filter<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        while let Some(r) = self.input.next()? {
            if passes(self.pred, &r, self.ctx)? {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }
}

struct Project<'a> {
    input: Box<dyn Exec + 'a>,
    exprs: &'a [Expr],
    ctx: &'a Ctx<'a>,
}

impl Exec for Project<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        match self.input.next()? {
            None => Ok(None),
            Some(r) => Ok(Some(self.exprs.iter().map(|e| eval(e, &r, self.ctx)).collect::<Result<_>>()?)),
        }
    }
}

fn drain(ex: &mut dyn Exec) -> Result<Vec<Row>> {
    let mut out = Vec::new();
    while let Some(r) = ex.next()? {
        out.push(r);
    }
    Ok(out)
}

struct NestedLoop<'a> {
    left: Box<dyn Exec + 'a>,
    right_plan: &'a Plan,
    right: Option<Vec<Row>>,
    ctx: &'a Ctx<'a>,
    kind: JoinType,
    on: Option<&'a Expr>,
    current: Option<Row>,
    pos: usize,
    matched: bool,
    right_width: usize,
}

impl Exec for NestedLoop<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        if self.right.is_none() {
            let mut ex = build(self.right_plan, self.ctx)?;
            self.right = Some(drain(ex.as_mut())?);
        }
        loop {
            if self.current.is_none() {
                match self.left.next()? {
                    None => return Ok(None),
                    Some(r) => {
                        self.current = Some(r);
                        self.pos = 0;
                        self.matched = false;
                    }
                }
            }
            let right = self.right.as_ref().unwrap();
            let left = self.current.as_ref().unwrap();
            while self.pos < right.len() {
                let mut row = left.clone();
                row.extend_from_slice(&right[self.pos]);
                self.pos += 1;
                if let Some(on) = self.on {
                    if !passes(on, &row, self.ctx)? {
                        continue;
                    }
                }
                self.matched = true;
                return Ok(Some(row));
            }
            let left = self.current.take().unwrap();
            if self.kind == JoinType::Left && !self.matched {
                let mut row = left;
                row.extend(std::iter::repeat_n(Value::Null, self.right_width));
                return Ok(Some(row));
            }
        }
    }
}

struct HashJoin<'a> {
    left: Box<dyn Exec + 'a>,
    right_plan: &'a Plan,
    table: Option<HashMap<Vec<Value>, Vec<Row>>>,
    ctx: &'a Ctx<'a>,
    kind: JoinType,
    left_keys: &'a [Expr],
    right_keys: &'a [Expr],
    residual: Option<&'a Expr>,
    pending: VecDeque<Row>,
    right_width: usize,
}

fn key_of(exprs: &[Expr], row: &[Value], ctx: &Ctx) -> Result<Option<Vec<Value>>> {
    let mut k = Vec::with_capacity(exprs.len());
    for e in exprs {
        let v = eval(e, row, ctx)?;
        if v.is_null() {
            return Ok(None); // NULL never equals anything
        }
        k.push(v);
    }
    Ok(Some(k))
}

impl Exec for HashJoin<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        if self.table.is_none() {
            let mut ex = build(self.right_plan, self.ctx)?;
            let mut table: HashMap<Vec<Value>, Vec<Row>> = HashMap::new();
            while let Some(r) = ex.next()? {
                if let Some(k) = key_of(self.right_keys, &r, self.ctx)? {
                    table.entry(k).or_default().push(r);
                }
            }
            self.table = Some(table);
        }
        loop {
            if let Some(r) = self.pending.pop_front() {
                return Ok(Some(r));
            }
            let Some(left) = self.left.next()? else { return Ok(None) };
            let mut matched = false;
            if let Some(k) = key_of(self.left_keys, &left, self.ctx)? {
                if let Some(rows) = self.table.as_ref().unwrap().get(&k) {
                    for r in rows {
                        let mut row = left.clone();
                        row.extend_from_slice(r);
                        if let Some(p) = self.residual {
                            if !passes(p, &row, self.ctx)? {
                                continue;
                            }
                        }
                        matched = true;
                        self.pending.push_back(row);
                    }
                }
            }
            if !matched && self.kind == JoinType::Left {
                let mut row = left;
                row.extend(std::iter::repeat_n(Value::Null, self.right_width));
                return Ok(Some(row));
            }
        }
    }
}

#[derive(Clone)]
enum Acc {
    Count(i64),
    Sum { int: i64, float: f64, is_float: bool, any: bool },
    Avg { sum: f64, n: i64 },
    Min(Value),
    Max(Value),
}

impl Acc {
    fn new(f: AggFunc) -> Acc {
        match f {
            AggFunc::Count => Acc::Count(0),
            AggFunc::Sum => Acc::Sum { int: 0, float: 0.0, is_float: false, any: false },
            AggFunc::Avg => Acc::Avg { sum: 0.0, n: 0 },
            AggFunc::Min => Acc::Min(Value::Null),
            AggFunc::Max => Acc::Max(Value::Null),
        }
    }

    fn add(&mut self, v: Value) -> Result<()> {
        if v.is_null() {
            return Ok(());
        }
        match self {
            Acc::Count(n) => *n += 1,
            Acc::Sum { int, float, is_float, any } => {
                *any = true;
                match v {
                    Value::Int(i) if !*is_float => match int.checked_add(i) {
                        Some(s) => *int = s,
                        None => return err!("22003", "integer out of range"),
                    },
                    other => {
                        if !*is_float {
                            *is_float = true;
                            *float = *int as f64;
                        }
                        *float += as_f64(&other.cast(DataType::Float)?);
                    }
                }
            }
            Acc::Avg { sum, n } => {
                *sum += as_f64(&v.cast(DataType::Float)?);
                *n += 1;
            }
            Acc::Min(m) => {
                if m.is_null() || v.compare(m)? == Some(Ordering::Less) {
                    *m = v;
                }
            }
            Acc::Max(m) => {
                if m.is_null() || v.compare(m)? == Some(Ordering::Greater) {
                    *m = v;
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Value {
        match self {
            Acc::Count(n) => Value::Int(n),
            Acc::Sum { any: false, .. } => Value::Null,
            Acc::Sum { is_float: true, float, .. } => Value::Float(float),
            Acc::Sum { int, .. } => Value::Int(int),
            Acc::Avg { n: 0, .. } => Value::Null,
            Acc::Avg { sum, n } => Value::Float(sum / n as f64),
            Acc::Min(v) | Acc::Max(v) => v,
        }
    }
}

struct Aggregate<'a> {
    input: Option<Box<dyn Exec + 'a>>,
    group: &'a [Expr],
    aggs: &'a [Agg],
    ctx: &'a Ctx<'a>,
    out: VecDeque<Row>,
}

impl Exec for Aggregate<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        if let Some(mut input) = self.input.take() {
            let mut order: Vec<Vec<Value>> = Vec::new();
            let mut groups: HashMap<Vec<Value>, (Vec<Acc>, Vec<HashSet<Value>>)> = HashMap::new();
            let fresh = || (self.aggs.iter().map(|a| Acc::new(a.func)).collect::<Vec<_>>(), vec![HashSet::new(); self.aggs.len()]);
            while let Some(row) = input.next()? {
                let key: Vec<Value> = self.group.iter().map(|g| eval(g, &row, self.ctx)).collect::<Result<_>>()?;
                let entry = match groups.get_mut(&key) {
                    Some(e) => e,
                    None => {
                        order.push(key.clone());
                        groups.entry(key).or_insert_with(fresh)
                    }
                };
                for (i, a) in self.aggs.iter().enumerate() {
                    let v = match &a.arg {
                        None => Value::Bool(true), // COUNT(*) counts rows
                        Some(e) => eval(e, &row, self.ctx)?,
                    };
                    if a.distinct && !v.is_null() && !entry.1[i].insert(v.clone()) {
                        continue;
                    }
                    entry.0[i].add(v)?;
                }
            }
            if self.group.is_empty() && order.is_empty() {
                order.push(vec![]);
                groups.insert(vec![], fresh());
            }
            for key in order {
                let (accs, _) = groups.remove(&key).unwrap();
                let mut row = key;
                row.extend(accs.into_iter().map(Acc::finish));
                self.out.push_back(row);
            }
        }
        Ok(self.out.pop_front())
    }
}

/// Orders values for ORDER BY: NULLs where asked, otherwise by value.
pub fn order_cmp(a: &Value, b: &Value, desc: bool, nulls_first: bool) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => if nulls_first { Ordering::Less } else { Ordering::Greater },
        (false, true) => if nulls_first { Ordering::Greater } else { Ordering::Less },
        _ => {
            let o = a.sort_cmp(b);
            if desc { o.reverse() } else { o }
        }
    }
}

struct Sort<'a> {
    input: Option<Box<dyn Exec + 'a>>,
    keys: &'a [SortKey],
    ctx: &'a Ctx<'a>,
    out: VecDeque<Row>,
}

impl Exec for Sort<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        if let Some(mut input) = self.input.take() {
            let mut rows: Vec<(Vec<Value>, Row)> = Vec::new();
            while let Some(r) = input.next()? {
                let k = self.keys.iter().map(|k| eval(&k.expr, &r, self.ctx)).collect::<Result<_>>()?;
                rows.push((k, r));
            }
            rows.sort_by(|(a, _), (b, _)| {
                for (i, k) in self.keys.iter().enumerate() {
                    let o = order_cmp(&a[i], &b[i], k.desc, k.nulls_first);
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            });
            self.out = rows.into_iter().map(|(_, r)| r).collect();
        }
        Ok(self.out.pop_front())
    }
}

struct Limit<'a> {
    input: Box<dyn Exec + 'a>,
    limit: Option<u64>,
    skip: u64,
    taken: u64,
}

impl Exec for Limit<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        if self.limit.is_some_and(|l| self.taken >= l) {
            return Ok(None);
        }
        while self.skip > 0 {
            if self.input.next()?.is_none() {
                return Ok(None);
            }
            self.skip -= 1;
        }
        let r = self.input.next()?;
        if r.is_some() {
            self.taken += 1;
        }
        Ok(r)
    }
}

struct Distinct<'a> {
    input: Box<dyn Exec + 'a>,
    seen: HashSet<Row>,
}

impl Exec for Distinct<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        while let Some(r) = self.input.next()? {
            if self.seen.insert(r.clone()) {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }
}

struct SetOpExec<'a> {
    op: SetOp,
    all: bool,
    left: Box<dyn Exec + 'a>,
    right: Option<Box<dyn Exec + 'a>>,
    right_counts: HashMap<Row, usize>,
    seen: HashSet<Row>,
}

impl<'a> SetOpExec<'a> {
    fn new(op: SetOp, all: bool, left: Box<dyn Exec + 'a>, mut right: Box<dyn Exec + 'a>) -> Result<SetOpExec<'a>> {
        let mut right_counts = HashMap::new();
        let mut keep = Some(right);
        if op != SetOp::Union {
            right = keep.take().unwrap();
            while let Some(r) = right.next()? {
                *right_counts.entry(r).or_insert(0) += 1;
            }
        }
        Ok(SetOpExec { op, all, left, right: keep, right_counts, seen: HashSet::new() })
    }
}

impl Exec for SetOpExec<'_> {
    fn next(&mut self) -> Result<Option<Row>> {
        loop {
            let r = match self.left.next()? {
                Some(r) => r,
                None => match (self.op, self.right.as_mut()) {
                    (SetOp::Union, Some(right)) => match right.next()? {
                        Some(r) => r,
                        None => return Ok(None),
                    },
                    _ => return Ok(None),
                },
            };
            match self.op {
                SetOp::Union => {
                    if self.all || self.seen.insert(r.clone()) {
                        return Ok(Some(r));
                    }
                }
                SetOp::Intersect => {
                    let n = self.right_counts.get_mut(&r);
                    match n {
                        Some(n) if *n > 0 => {
                            if self.all {
                                *n -= 1;
                                return Ok(Some(r));
                            }
                            if self.seen.insert(r.clone()) {
                                return Ok(Some(r));
                            }
                        }
                        _ => {}
                    }
                }
                SetOp::Except => match self.right_counts.get_mut(&r) {
                    Some(n) if *n > 0 => {
                        if self.all {
                            *n -= 1;
                        }
                    }
                    _ => {
                        if self.all || self.seen.insert(r.clone()) {
                            return Ok(Some(r));
                        }
                    }
                },
            }
        }
    }
}

// ---- writes ----------------------------------------------------------------------------------

fn row_count_key(table: &Table) -> Vec<u8> {
    format!("rows:{}", table.id).into_bytes()
}

fn add_index_entries(ctx: &Ctx, table: &Table, row: &[Value], row_key: &[u8]) -> Result<()> {
    for ix in &table.indexes {
        let (k, v) = table.index_entry(ix, row, row_key);
        if ix.unique {
            if let Some(existing) = ctx.txn.get(&k)? {
                if existing != v {
                    let cols: Vec<&str> = ix.columns.iter().map(|&c| table.columns[c].name.as_str()).collect();
                    return err!("23505", "duplicate key value violates unique constraint \"{}\" ({})", ix.name, cols.join(", "));
                }
            }
        }
        ctx.txn.set(&k, &v)?;
    }
    Ok(())
}

fn remove_index_entries(ctx: &Ctx, table: &Table, row: &[Value], row_key: &[u8]) -> Result<()> {
    for ix in &table.indexes {
        let (k, _) = table.index_entry(ix, row, row_key);
        ctx.txn.delete(&k)?;
    }
    Ok(())
}

/// The suffix of a row's storage key after the table prefix.
fn suffix<'k>(table: &Table, key: &'k [u8]) -> &'k [u8] {
    &key[row_prefix(table.id).len()..]
}

pub fn insert(ctx: &Ctx, table: &Table, input: &Plan) -> Result<u64> {
    // Read everything first: an INSERT ... SELECT from the same table must not see its own rows.
    let rows = {
        let mut ex = build(input, ctx)?;
        drain(ex.as_mut())?
    };
    let prefix = row_prefix(table.id);
    let mut n = 0;
    for row in rows {
        let row = catalog::conform(table, row)?;
        let pk = match table.key_of(&row) {
            Some(k) => k,
            None => {
                let id = ctx.mvcc.bump(format!("rowid:{}", table.id).as_bytes(), 1)?;
                let mut k = Vec::new();
                Value::Int(id).encode_key(&mut k);
                k
            }
        };
        let mut key = prefix.clone();
        key.extend_from_slice(&pk);
        if ctx.txn.get(&key)?.is_some() {
            let cols: Vec<&str> = table.primary_key.iter().map(|&c| table.columns[c].name.as_str()).collect();
            return err!("23505", "duplicate key value violates unique constraint \"{}_pkey\" ({})", table.name, cols.join(", "));
        }
        ctx.txn.set(&key, &encode_row(&row))?;
        add_index_entries(ctx, table, &row, &pk)?;
        n += 1;
    }
    ctx.mvcc.bump(&row_count_key(table), n as i64)?;
    Ok(n)
}

fn scan_targets(ctx: &Ctx, plan: &Plan) -> Result<Vec<(Vec<u8>, Row)>> {
    // The input of an UPDATE or DELETE is a scan, perhaps under a filter the optimiser left.
    let t = Instant::now();
    let out = match &plan.node {
        Node::Scan { table, access, filter, .. } => {
            let mut scan = Scan::new(ctx, table, access, filter.as_ref())?;
            let mut out = Vec::new();
            while let Some(kr) = scan.next_keyed()? {
                out.push(kr);
            }
            out
        }
        Node::Filter { input, pred } => {
            let mut out = Vec::new();
            for (k, r) in scan_targets(ctx, input)? {
                if passes(pred, &r, ctx)? {
                    out.push((k, r));
                }
            }
            out
        }
        _ => return err!("XX000", "unexpected plan under a write"),
    };
    ctx.record(plan.id, out.len() as u64, 1, t.elapsed().as_nanos() as u64);
    Ok(out)
}

pub fn update(ctx: &Ctx, table: &Table, input: &Plan, set: &[(usize, Expr)]) -> Result<u64> {
    let targets = scan_targets(ctx, input)?;
    let prefix = row_prefix(table.id);
    let mut n = 0;
    for (key, old) in targets {
        let mut new = old.clone();
        for (c, e) in set {
            new[*c] = eval(e, &old, ctx)?;
        }
        let new = catalog::conform(table, new)?;
        let old_pk = suffix(table, &key).to_vec();
        let new_pk = table.key_of(&new).unwrap_or_else(|| old_pk.clone());
        remove_index_entries(ctx, table, &old, &old_pk)?;
        if new_pk != old_pk {
            let mut new_key = prefix.clone();
            new_key.extend_from_slice(&new_pk);
            if ctx.txn.get(&new_key)?.is_some() {
                return err!("23505", "duplicate key value violates unique constraint \"{}_pkey\"", table.name);
            }
            ctx.txn.delete(&key)?;
            ctx.txn.set(&new_key, &encode_row(&new))?;
        } else {
            ctx.txn.set(&key, &encode_row(&new))?;
        }
        add_index_entries(ctx, table, &new, &new_pk)?;
        n += 1;
    }
    Ok(n)
}

pub fn delete(ctx: &Ctx, table: &Table, input: &Plan) -> Result<u64> {
    let targets = scan_targets(ctx, input)?;
    let mut n = 0;
    for (key, row) in targets {
        let pk = suffix(table, &key).to_vec();
        remove_index_entries(ctx, table, &row, &pk)?;
        ctx.txn.delete(&key)?;
        n += 1;
    }
    ctx.mvcc.bump(&row_count_key(table), -(n as i64))?;
    Ok(n)
}

/// Fills a new index from the table's rows.
pub fn build_index(ctx: &Ctx, table: &Table, index: usize) -> Result<()> {
    let mut scan = Scan::new(ctx, table, &Access::Full, None)?;
    let ix = &table.indexes[index];
    let one = Table { indexes: vec![ix.clone()], ..table.clone() };
    while let Some((key, row)) = scan.next_keyed()? {
        add_index_entries(ctx, &one, &row, suffix(table, &key))?;
    }
    Ok(())
}

/// Deletes every row and index entry of a table.
pub fn truncate(ctx: &Ctx, table: &Table) -> Result<u64> {
    let mut scan = Scan::new(ctx, table, &Access::Full, None)?;
    let mut n = 0;
    while let Some((key, row)) = scan.next_keyed()? {
        remove_index_entries(ctx, table, &row, suffix(table, &key))?;
        ctx.txn.delete(&key)?;
        n += 1;
    }
    ctx.mvcc.bump(&row_count_key(table), -(n as i64))?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert!(like("hello", "h%o", false));
        assert!(like("hello", "%", false));
        assert!(like("hello", "_ello", false));
        assert!(!like("hello", "h_o", false));
        assert!(like("HeLLo", "hello", true));
        assert!(!like("HeLLo", "hello", false));
        assert!(like("a%b", "a\\%b", false));
        assert!(like("abcabc", "%bc", false));
    }
}
