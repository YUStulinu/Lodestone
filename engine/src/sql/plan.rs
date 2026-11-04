//! Query plans and the binder that builds them from the syntax tree: names resolved to column
//! positions, aggregates pulled out, subqueries bound (and marked correlated when they refer to
//! an enclosing query), and the FROM clause handed to the optimiser for join ordering.

use std::collections::HashSet;
use std::rc::Rc;

use super::ast::{self, BinOp, FromItem, JoinKind, SelectItem, SetExpr, SetOp, UnOp};
use super::catalog::{self, Table};
use super::optimize;
use super::types::{DataType, Error, Result, Value, err};
use crate::mvcc::{Mvcc, Transaction};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Func {
    Abs,
    Coalesce,
    NullIf,
    Length,
    Lower,
    Upper,
    Substr,
    Trim,
    LTrim,
    RTrim,
    Round,
    Replace,
    Least,
    Greatest,
    Version,
    Typeof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Agg {
    pub func: AggFunc,
    /// None for COUNT(*).
    pub arg: Option<Expr>,
    pub distinct: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SubKind {
    Scalar,
    Exists,
    In,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubPlan {
    pub plan: Plan,
    pub kind: SubKind,
    pub negated: bool,
    /// Refers to an enclosing query's row, so it must be run again for each row.
    pub correlated: bool,
    pub id: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Const(Value),
    Col(usize),
    /// A column of an enclosing query's row: (how many subquery levels up, column).
    Outer(usize, usize),
    Param(usize),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Func(Func, Vec<Expr>),
    Case(Option<Box<Expr>>, Vec<(Expr, Expr)>, Option<Box<Expr>>),
    Cast(Box<Expr>, DataType),
    InList(Box<Expr>, Vec<Expr>, bool),
    Like(Box<Expr>, Box<Expr>, bool, bool),
    IsNull(Box<Expr>, bool),
    /// A subquery; for IN, the left operand.
    Sub(Rc<SubPlan>, Option<Box<Expr>>),
    /// Binder-internal: the k-th aggregate of the query being bound.
    AggRef(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Col {
    pub table: Option<String>,
    pub name: String,
    pub ty: Option<DataType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    pub expr: Expr,
    pub desc: bool,
    pub nulls_first: bool,
}

/// How a scan finds its rows.
#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    Full,
    /// Primary key lookups, one tuple of values each.
    Keys(Vec<Vec<Expr>>),
    /// A range of a single-column primary key: (value, inclusive).
    KeyRange {
        lo: Option<(Expr, bool)>,
        hi: Option<(Expr, bool)>,
    },
    /// Index lookups on a prefix of the index's columns.
    IndexEq {
        index: usize,
        keys: Vec<Vec<Expr>>,
    },
    /// A range on an index's first column.
    IndexRange {
        index: usize,
        lo: Option<(Expr, bool)>,
        hi: Option<(Expr, bool)>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// `rows`: the table's estimated size.
    Scan {
        table: Rc<Table>,
        alias: String,
        access: Access,
        filter: Option<Expr>,
        rows: f64,
    },
    Values {
        rows: Vec<Vec<Expr>>,
    },
    Filter {
        input: Box<Plan>,
        pred: Expr,
    },
    Project {
        input: Box<Plan>,
        exprs: Vec<Expr>,
    },
    NestedLoop {
        left: Box<Plan>,
        right: Box<Plan>,
        kind: JoinType,
        on: Option<Expr>,
    },
    HashJoin {
        left: Box<Plan>,
        right: Box<Plan>,
        kind: JoinType,
        left_keys: Vec<Expr>,
        right_keys: Vec<Expr>,
        residual: Option<Expr>,
    },
    Aggregate {
        input: Box<Plan>,
        group: Vec<Expr>,
        aggs: Vec<Agg>,
    },
    Sort {
        input: Box<Plan>,
        keys: Vec<SortKey>,
    },
    Limit {
        input: Box<Plan>,
        limit: Option<Expr>,
        offset: Option<Expr>,
    },
    Distinct {
        input: Box<Plan>,
    },
    SetOp {
        op: SetOp,
        all: bool,
        left: Box<Plan>,
        right: Box<Plan>,
    },
    Insert {
        table: Rc<Table>,
        input: Box<Plan>,
    },
    Update {
        table: Rc<Table>,
        input: Box<Plan>,
        set: Vec<(usize, Expr)>,
    },
    Delete {
        table: Rc<Table>,
        input: Box<Plan>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub node: Node,
    pub cols: Vec<Col>,
    /// Estimated rows out, and estimated cost (abstract units: about one row touched each).
    pub est: f64,
    pub cost: f64,
    /// Position in a pre-order walk, for EXPLAIN ANALYZE.
    pub id: usize,
}

impl Plan {
    pub fn new(node: Node, cols: Vec<Col>, est: f64, cost: f64) -> Plan {
        Plan { node, cols, est, cost, id: 0 }
    }

    pub fn children(&self) -> Vec<&Plan> {
        match &self.node {
            Node::Scan { .. } | Node::Values { .. } => vec![],
            Node::Filter { input, .. }
            | Node::Project { input, .. }
            | Node::Aggregate { input, .. }
            | Node::Sort { input, .. }
            | Node::Limit { input, .. }
            | Node::Distinct { input }
            | Node::Insert { input, .. }
            | Node::Update { input, .. }
            | Node::Delete { input, .. } => vec![input],
            Node::NestedLoop { left, right, .. } | Node::HashJoin { left, right, .. } | Node::SetOp { left, right, .. } => {
                vec![left, right]
            }
        }
    }

    fn children_mut(&mut self) -> Vec<&mut Plan> {
        match &mut self.node {
            Node::Scan { .. } | Node::Values { .. } => vec![],
            Node::Filter { input, .. }
            | Node::Project { input, .. }
            | Node::Aggregate { input, .. }
            | Node::Sort { input, .. }
            | Node::Limit { input, .. }
            | Node::Distinct { input }
            | Node::Insert { input, .. }
            | Node::Update { input, .. }
            | Node::Delete { input, .. } => vec![input],
            Node::NestedLoop { left, right, .. } | Node::HashJoin { left, right, .. } | Node::SetOp { left, right, .. } => {
                vec![left, right]
            }
        }
    }

    /// Numbers the nodes in pre-order, subquery plans included.
    pub fn number(&mut self) {
        fn walk(p: &mut Plan, next: &mut usize) {
            p.id = *next;
            *next += 1;
            for e in p.exprs_mut() {
                subs(e, next);
            }
            for c in p.children_mut() {
                walk(c, next);
            }
        }
        fn subs(e: &mut Expr, next: &mut usize) {
            if let Expr::Sub(sp, left) = e {
                if let Some(l) = left {
                    subs(l, next);
                }
                walk(&mut Rc::make_mut(sp).plan, next);
                return;
            }
            e.for_each_child_mut(&mut |c| subs(c, next));
        }
        let mut n = 0;
        walk(self, &mut n);
    }

    /// Every expression held by this node (not its children).
    #[allow(clippy::needless_lifetimes)]
    pub fn exprs_mut(&mut self) -> Vec<&mut Expr> {
        let mut out: Vec<&mut Expr> = Vec::new();
        match &mut self.node {
            Node::Scan { access, filter, .. } => {
                match access {
                    Access::Full => {}
                    Access::Keys(keys) | Access::IndexEq { keys, .. } => out.extend(keys.iter_mut().flatten()),
                    Access::KeyRange { lo, hi } | Access::IndexRange { lo, hi, .. } => {
                        out.extend(opt(lo));
                        out.extend(opt(hi));
                    }
                }
                out.extend(filter.as_mut());
            }
            Node::Values { rows } => out.extend(rows.iter_mut().flatten()),
            Node::Filter { pred, .. } => out.push(pred),
            Node::Project { exprs, .. } => out.extend(exprs.iter_mut()),
            Node::NestedLoop { on, .. } => out.extend(on.as_mut()),
            Node::HashJoin { left_keys, right_keys, residual, .. } => {
                out.extend(left_keys.iter_mut());
                out.extend(right_keys.iter_mut());
                out.extend(residual.as_mut());
            }
            Node::Aggregate { group, aggs, .. } => {
                out.extend(group.iter_mut());
                out.extend(aggs.iter_mut().filter_map(|a| a.arg.as_mut()));
            }
            Node::Sort { keys, .. } => out.extend(keys.iter_mut().map(|k| &mut k.expr)),
            Node::Limit { limit, offset, .. } => {
                out.extend(limit.as_mut());
                out.extend(offset.as_mut());
            }
            Node::Update { set, .. } => out.extend(set.iter_mut().map(|s| &mut s.1)),
            Node::Distinct { .. } | Node::SetOp { .. } | Node::Insert { .. } | Node::Delete { .. } => {}
        }
        out
    }

    /// Rewrites the references this plan (at subquery depth `depth` below the query whose
    /// columns are being moved) makes to that query's columns.
    pub fn remap_outer(&mut self, depth: usize, f: &dyn Fn(usize) -> usize) {
        for e in self.exprs_mut() {
            e.remap(depth, f);
        }
        for c in self.children_mut() {
            c.remap_outer(depth, f);
        }
    }
}

fn opt(o: &mut Option<(Expr, bool)>) -> Option<&mut Expr> {
    o.as_mut().map(|x| &mut x.0)
}

impl Expr {
    pub fn and(a: Expr, b: Expr) -> Expr {
        Expr::Bin(BinOp::And, Box::new(a), Box::new(b))
    }

    /// Rewrites column references with `f`: plain columns when `depth` is 0, else outer
    /// references reaching exactly `depth` levels up (through subqueries).
    pub fn remap(&mut self, depth: usize, f: &dyn Fn(usize) -> usize) {
        match self {
            Expr::Col(i) if depth == 0 => *i = f(*i),
            Expr::Outer(d, i) if *d == depth && depth > 0 => *i = f(*i),
            Expr::Sub(sp, left) => {
                if let Some(l) = left {
                    l.remap(depth, f);
                }
                let mut s = (**sp).clone();
                s.plan.remap_outer(depth + 1, f);
                *sp = Rc::new(s);
            }
            _ => self.for_each_child_mut(&mut |c| c.remap(depth, f)),
        }
    }

    pub fn for_each_child_mut(&mut self, f: &mut dyn FnMut(&mut Expr)) {
        match self {
            Expr::Const(_) | Expr::Col(_) | Expr::Outer(..) | Expr::Param(_) | Expr::AggRef(_) => {}
            Expr::Neg(e) | Expr::Not(e) | Expr::Cast(e, _) | Expr::IsNull(e, _) => f(e),
            Expr::Bin(_, a, b) | Expr::Like(a, b, ..) => {
                f(a);
                f(b);
            }
            Expr::Func(_, args) => args.iter_mut().for_each(f),
            Expr::Case(op, whens, else_) => {
                if let Some(o) = op {
                    f(o);
                }
                for (w, t) in whens {
                    f(w);
                    f(t);
                }
                if let Some(e) = else_ {
                    f(e);
                }
            }
            Expr::InList(e, list, _) => {
                f(e);
                list.iter_mut().for_each(f);
            }
            Expr::Sub(_, left) => {
                if let Some(l) = left {
                    f(l);
                }
            }
        }
    }

    pub fn children(&self) -> Vec<&Expr> {
        match self {
            Expr::Const(_) | Expr::Col(_) | Expr::Outer(..) | Expr::Param(_) | Expr::AggRef(_) => vec![],
            Expr::Neg(e) | Expr::Not(e) | Expr::Cast(e, _) | Expr::IsNull(e, _) => vec![e],
            Expr::Bin(_, a, b) | Expr::Like(a, b, ..) => vec![a, b],
            Expr::Func(_, args) => args.iter().collect(),
            Expr::Case(op, whens, else_) => {
                let mut v: Vec<&Expr> = op.iter().map(|o| &**o).collect();
                for (w, t) in whens {
                    v.push(w);
                    v.push(t);
                }
                v.extend(else_.iter().map(|e| &**e));
                v
            }
            Expr::InList(e, list, _) => std::iter::once(&**e).chain(list.iter()).collect(),
            Expr::Sub(_, left) => left.iter().map(|l| &**l).collect(),
        }
    }

    pub fn for_each_child(&self, f: &mut dyn FnMut(&Expr)) {
        for c in self.children() {
            f(c);
        }
    }

    /// Columns of the current row this expression reads, including through correlated subqueries.
    pub fn columns(&self) -> HashSet<usize> {
        let mut out = HashSet::new();
        self.collect_columns(0, &mut out);
        out
    }

    fn collect_columns(&self, depth: usize, out: &mut HashSet<usize>) {
        match self {
            Expr::Col(i) if depth == 0 => {
                out.insert(*i);
            }
            Expr::Outer(d, i) if *d == depth && depth > 0 => {
                out.insert(*i);
            }
            Expr::Sub(sp, left) => {
                if let Some(l) = left {
                    l.collect_columns(depth, out);
                }
                plan_columns(&sp.plan, depth + 1, out);
            }
            _ => self.for_each_child(&mut |c| c.collect_columns(depth, out)),
        }
    }

    pub fn has_subquery(&self) -> bool {
        let mut found = matches!(self, Expr::Sub(..));
        if !found {
            self.for_each_child(&mut |c| found |= c.has_subquery());
        }
        found
    }

    /// True when the value does not depend on the current row (it may depend on parameters and
    /// enclosing rows, which are fixed while a scan runs).
    pub fn is_row_independent(&self) -> bool {
        match self {
            Expr::Col(_) | Expr::AggRef(_) | Expr::Sub(..) => false,
            _ => {
                let mut ok = true;
                self.for_each_child(&mut |c| ok &= c.is_row_independent());
                ok
            }
        }
    }

    /// The type this expression evaluates to, if known (NULL has none).
    pub fn data_type(&self, cols: &[Col]) -> Option<DataType> {
        use DataType::*;
        match self {
            Expr::Const(v) => v.data_type(),
            Expr::Col(i) => cols.get(*i).and_then(|c| c.ty),
            Expr::Outer(..) | Expr::Param(_) | Expr::AggRef(_) => None,
            Expr::Neg(e) => e.data_type(cols),
            Expr::Not(_) | Expr::InList(..) | Expr::Like(..) | Expr::IsNull(..) => Some(Bool),
            Expr::Bin(op, a, b) => match op {
                BinOp::Concat => Some(Text),
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => match (a.data_type(cols), b.data_type(cols)) {
                    (Some(Float), _) | (_, Some(Float)) => Some(Float),
                    (Some(Int), Some(Int)) => Some(Int),
                    (Some(Int), None) | (None, Some(Int)) => Some(Int),
                    (x, None) | (None, x) => x,
                    _ => Some(Float),
                },
                _ => Some(Bool),
            },
            Expr::Func(f, args) => match f {
                Func::Abs | Func::Round => args.first().and_then(|a| a.data_type(cols)),
                Func::Length => Some(Int),
                Func::Coalesce | Func::NullIf | Func::Least | Func::Greatest => {
                    let tys: Vec<_> = args.iter().filter_map(|a| a.data_type(cols)).collect();
                    if tys.contains(&Float) && tys.iter().all(|t| *t == Int || *t == Float) { Some(Float) } else { tys.first().copied() }
                }
                _ => Some(Text),
            },
            Expr::Case(_, whens, else_) => {
                let tys: Vec<_> = whens.iter().map(|(_, t)| t).chain(else_.as_deref()).filter_map(|e| e.data_type(cols)).collect();
                if tys.contains(&Float) && tys.iter().all(|t| *t == Int || *t == Float) { Some(Float) } else { tys.first().copied() }
            }
            Expr::Cast(_, t) => Some(*t),
            Expr::Sub(sp, _) => match sp.kind {
                SubKind::Scalar => sp.plan.cols.first().and_then(|c| c.ty),
                _ => Some(Bool),
            },
        }
    }
}

fn plan_columns(plan: &Plan, depth: usize, out: &mut HashSet<usize>) {
    let mut p = plan.clone();
    for e in p.exprs_mut() {
        e.collect_columns(depth, out);
    }
    for c in plan.children() {
        plan_columns(c, depth, out);
    }
}

/// Splits AND chains.
pub fn conjuncts(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::Bin(BinOp::And, a, b) => {
            conjuncts(*a, out);
            conjuncts(*b, out);
        }
        e => out.push(e),
    }
}

pub fn and_all(es: Vec<Expr>) -> Option<Expr> {
    es.into_iter().reduce(Expr::and)
}

/// One level of query nesting being bound.
struct Frame {
    scope: Vec<Col>,
    /// Whether this frame's plan runs once per row of the frame below (an expression subquery),
    /// as opposed to inline (a derived table).
    pushes_row: bool,
    correlated: bool,
}

pub struct Binder<'a> {
    pub txn: &'a Transaction,
    pub mvcc: &'a Mvcc,
    frames: Vec<Frame>,
    next_sub: usize,
    view_depth: usize,
}

/// What an aggregate-aware expression binding collects.
struct AggCtx {
    aggs: Vec<Agg>,
    allowed: bool,
}

const AGGREGATES: [&str; 5] = ["count", "sum", "avg", "min", "max"];

impl<'a> Binder<'a> {
    pub fn new(txn: &'a Transaction, mvcc: &'a Mvcc) -> Binder<'a> {
        Binder { txn, mvcc, frames: Vec::new(), next_sub: 0, view_depth: 0 }
    }

    pub fn rows_estimate(&self, table: &Table) -> f64 {
        (self.mvcc.counter(format!("rows:{}", table.id).as_bytes()).unwrap_or(0) as f64).max(1.0)
    }

    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("a frame")
    }

    // ---- queries -------------------------------------------------------------------------

    pub fn query(&mut self, q: &ast::Query, pushes_row: bool) -> Result<Plan> {
        let mut plan = match &q.body {
            SetExpr::Select(sel) => return self.select(sel, q, pushes_row),
            body => self.set_expr(body, pushes_row)?,
        };
        // ORDER BY on a set operation or VALUES: output names and ordinals.
        if !q.order_by.is_empty() {
            let mut keys = Vec::new();
            for item in &q.order_by {
                let i = match &item.expr {
                    ast::Expr::Literal(Value::Int(n)) if *n >= 1 && (*n as usize) <= plan.cols.len() => *n as usize - 1,
                    ast::Expr::Column(None, name) => match plan.cols.iter().position(|c| &c.name == name) {
                        Some(i) => i,
                        None => return err!("42703", "column \"{name}\" does not exist"),
                    },
                    _ => return err!("0A000", "ORDER BY on a set operation must name an output column or position"),
                };
                keys.push(SortKey { expr: Expr::Col(i), desc: item.desc, nulls_first: item.nulls_first.unwrap_or(item.desc) });
            }
            plan = optimize::sort(plan, keys);
        }
        self.limit(plan, q)
    }

    fn limit(&mut self, plan: Plan, q: &ast::Query) -> Result<Plan> {
        if q.limit.is_none() && q.offset.is_none() {
            return Ok(plan);
        }
        let mut agg = AggCtx { aggs: vec![], allowed: false };
        let limit = q.limit.as_ref().map(|e| self.expr(e, &mut agg)).transpose()?;
        let offset = q.offset.as_ref().map(|e| self.expr(e, &mut agg)).transpose()?;
        let est = match &limit {
            Some(Expr::Const(Value::Int(n))) => plan.est.min(*n as f64),
            _ => plan.est,
        };
        let cols = plan.cols.clone();
        let cost = plan.cost;
        Ok(Plan::new(Node::Limit { input: Box::new(plan), limit, offset }, cols, est, cost))
    }

    fn set_expr(&mut self, body: &SetExpr, pushes_row: bool) -> Result<Plan> {
        match body {
            SetExpr::Select(sel) => {
                let q = ast::Query { body: SetExpr::Select(sel.clone()), order_by: vec![], limit: None, offset: None };
                self.select(sel, &q, pushes_row)
            }
            SetExpr::Query(q) => self.query(q, pushes_row),
            SetExpr::Values(rows) => {
                self.frames.push(Frame { scope: vec![], pushes_row, correlated: false });
                let res = self.values(rows);
                self.frames.pop();
                res
            }
            SetExpr::SetOp { op, all, left, right } => {
                let l = self.set_expr(left, pushes_row)?;
                let r = self.set_expr(right, pushes_row)?;
                if l.cols.len() != r.cols.len() {
                    return err!("42601", "each {} query must have the same number of columns", format!("{op:?}").to_uppercase());
                }
                let cols = l.cols.iter().zip(&r.cols).map(|(a, b)| Col { table: None, name: a.name.clone(), ty: a.ty.or(b.ty) }).collect();
                let est = match op {
                    SetOp::Union => l.est + r.est,
                    SetOp::Intersect => l.est.min(r.est),
                    SetOp::Except => l.est,
                };
                let cost = l.cost + r.cost + l.est + r.est;
                Ok(Plan::new(Node::SetOp { op: *op, all: *all, left: Box::new(l), right: Box::new(r) }, cols, est, cost))
            }
        }
    }

    fn values(&mut self, rows: &[Vec<ast::Expr>]) -> Result<Plan> {
        let width = rows[0].len();
        let mut agg = AggCtx { aggs: vec![], allowed: false };
        let mut bound = Vec::new();
        for r in rows {
            if r.len() != width {
                return err!("42601", "VALUES lists must all be the same length");
            }
            bound.push(r.iter().map(|e| self.expr(e, &mut agg)).collect::<Result<Vec<_>>>()?);
        }
        let cols = (0..width)
            .map(|i| Col { table: None, name: format!("column{}", i + 1), ty: bound.iter().find_map(|r| r[i].data_type(&[])) })
            .collect();
        let n = bound.len() as f64;
        Ok(Plan::new(Node::Values { rows: bound }, cols, n, n))
    }

    fn select(&mut self, sel: &ast::Select, q: &ast::Query, pushes_row: bool) -> Result<Plan> {
        self.frames.push(Frame { scope: vec![], pushes_row, correlated: false });
        let res = self.select_inner(sel, q);
        self.frames.pop();
        res
    }

    fn select_inner(&mut self, sel: &ast::Select, q: &ast::Query) -> Result<Plan> {
        // FROM: relations and join conditions, then WHERE, then join ordering.
        let (rels, mut preds) = if sel.from.is_empty() {
            let one = Plan::new(Node::Values { rows: vec![vec![]] }, vec![], 1.0, 1.0);
            (vec![one], vec![])
        } else {
            let mut rels = Vec::new();
            let mut ons = Vec::new();
            for item in &sel.from {
                self.flatten(item, &mut rels, &mut ons)?;
            }
            self.frame().scope = rels.iter().flat_map(|r: &Plan| r.cols.clone()).collect();
            let mut preds = Vec::new();
            for on in ons {
                let mut agg = AggCtx { aggs: vec![], allowed: false };
                conjuncts(self.expr(&on, &mut agg)?, &mut preds);
            }
            (rels, preds)
        };
        if let Some(w) = &sel.filter {
            let mut agg = AggCtx { aggs: vec![], allowed: false };
            let e = self.expr(w, &mut agg)?;
            conjuncts(e, &mut preds);
        }
        let scope = self.frame().scope.clone();
        let mut plan = optimize::join(rels, preds)?;
        debug_assert_eq!(plan.cols.len(), scope.len());

        // SELECT list (expanding *), with aggregates collected.
        let mut agg = AggCtx { aggs: vec![], allowed: true };
        let mut items: Vec<(Expr, String)> = Vec::new();
        for item in &sel.items {
            match item {
                SelectItem::Wildcard => {
                    if scope.is_empty() {
                        return err!("42601", "SELECT * with no tables specified is not valid");
                    }
                    items.extend(scope.iter().enumerate().map(|(i, c)| (Expr::Col(i), c.name.clone())));
                }
                SelectItem::QualifiedWildcard(t) => {
                    let before = items.len();
                    items.extend(
                        scope
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| c.table.as_deref() == Some(t))
                            .map(|(i, c)| (Expr::Col(i), c.name.clone())),
                    );
                    if items.len() == before {
                        return err!("42P01", "missing FROM-clause entry for table \"{t}\"");
                    }
                }
                SelectItem::Expr(e, alias) => {
                    let b = self.expr(e, &mut agg)?;
                    let name = alias.clone().unwrap_or_else(|| default_name(e));
                    items.push((b, name));
                }
            }
        }
        let mut group = Vec::new();
        for g in &sel.group_by {
            // GROUP BY may name an output column or position.
            let e = match g {
                ast::Expr::Literal(Value::Int(n)) if *n >= 1 && (*n as usize) <= items.len() => items[*n as usize - 1].0.clone(),
                ast::Expr::Column(None, name) if self.resolve(None, name).is_err() => match items.iter().find(|(_, n)| n == name) {
                    Some((e, _)) => e.clone(),
                    None => self.expr(g, &mut AggCtx { aggs: vec![], allowed: false })?,
                },
                _ => self.expr(g, &mut AggCtx { aggs: vec![], allowed: false })?,
            };
            if contains_agg(&e) {
                return err!("42803", "aggregate functions are not allowed in GROUP BY");
            }
            group.push(e);
        }
        let having = match &sel.having {
            Some(h) => Some(self.expr(h, &mut agg)?),
            None => None,
        };

        // ORDER BY: output names and positions, else expressions over the input.
        let mut order: Vec<(usize, bool, bool)> = Vec::new(); // (column of the projected row, desc, nulls first)
        let mut hidden: Vec<Expr> = Vec::new();
        for item in &q.order_by {
            let nulls_first = item.nulls_first.unwrap_or(item.desc);
            let target = match &item.expr {
                ast::Expr::Literal(Value::Int(n)) => {
                    if *n < 1 || *n as usize > items.len() {
                        return err!("42P10", "ORDER BY position {n} is not in select list");
                    }
                    *n as usize - 1
                }
                ast::Expr::Column(None, name) if items.iter().filter(|(_, n)| n == name).count() == 1 => {
                    items.iter().position(|(_, n)| n == name).unwrap()
                }
                e => {
                    let b = self.expr(e, &mut agg)?;
                    match items.iter().position(|(x, _)| *x == b) {
                        Some(i) => i,
                        None => {
                            if sel.distinct {
                                return err!("42P10", "for SELECT DISTINCT, ORDER BY expressions must appear in select list");
                            }
                            hidden.push(b);
                            items.len() + hidden.len() - 1
                        }
                    }
                }
            };
            order.push((target, item.desc, nulls_first));
        }

        let aggregated = !agg.aggs.is_empty() || !group.is_empty() || having.is_some();
        let mut outputs: Vec<Expr> = items.iter().map(|(e, _)| e.clone()).chain(hidden).collect();
        let mut having = having;
        if aggregated {
            let aggs = std::mem::take(&mut agg.aggs);
            let g = group.len();
            let mut cols: Vec<Col> = group
                .iter()
                .map(|e| Col {
                    table: match e {
                        Expr::Col(i) => scope[*i].table.clone(),
                        _ => None,
                    },
                    name: match e {
                        Expr::Col(i) => scope[*i].name.clone(),
                        _ => "?column?".into(),
                    },
                    ty: e.data_type(&scope),
                })
                .collect();
            for a in &aggs {
                let ty = match (a.func, a.arg.as_ref().and_then(|x| x.data_type(&scope))) {
                    (AggFunc::Count, _) => Some(DataType::Int),
                    (AggFunc::Avg, _) => Some(DataType::Float),
                    (_, t) => t,
                };
                cols.push(Col { table: None, name: format!("{:?}", a.func).to_lowercase(), ty });
            }
            let est = if group.is_empty() { 1.0 } else { (plan.est * 0.1).max(1.0) };
            let cost = plan.cost + plan.est;
            plan = Plan::new(Node::Aggregate { input: Box::new(plan), group: group.clone(), aggs }, cols, est, cost);
            // Rewrite outputs over the aggregate's row: group expressions, then aggregates.
            let rewrite = |e: &mut Expr| -> Result<()> { over_groups(e, &group, g, &scope) };
            for e in outputs.iter_mut() {
                rewrite(e)?;
            }
            if let Some(h) = having.as_mut() {
                rewrite(h)?;
            }
        }
        let input_cols = plan.cols.clone();
        if let Some(h) = having {
            plan = optimize::filter(plan, h);
        }
        let mut out_cols: Vec<Col> = items
            .iter()
            .zip(&outputs)
            .map(|((e, name), b)| Col {
                table: match e {
                    Expr::Col(i) if !aggregated => scope[*i].table.clone(),
                    _ => None,
                },
                name: name.clone(),
                ty: b.data_type(&input_cols),
            })
            .collect();
        for h in &outputs[items.len()..] {
            out_cols.push(Col { table: None, name: "?order?".into(), ty: h.data_type(&input_cols) });
        }
        let visible = items.len();
        plan = optimize::project(plan, outputs, out_cols);
        if sel.distinct {
            let cols = plan.cols.clone();
            let (est, cost) = (plan.est, plan.cost + plan.est);
            plan = Plan::new(Node::Distinct { input: Box::new(plan) }, cols, est, cost);
        }
        if !order.is_empty() {
            let keys = order.iter().map(|&(i, desc, nf)| SortKey { expr: Expr::Col(i), desc, nulls_first: nf }).collect();
            plan = optimize::sort(plan, keys);
        }
        plan = self.limit(plan, q)?;
        if plan.cols.len() > visible {
            let exprs = (0..visible).map(Expr::Col).collect();
            let cols = plan.cols[..visible].to_vec();
            plan = optimize::project(plan, exprs, cols);
        }
        Ok(plan)
    }

    /// Adds a FROM item's relations: inner and cross joins are flattened (their ON conditions
    /// join the WHERE conjuncts for the optimiser); outer joins become one relation each.
    fn flatten(&mut self, item: &FromItem, rels: &mut Vec<Plan>, ons: &mut Vec<ast::Expr>) -> Result<()> {
        match item {
            FromItem::Table { .. } | FromItem::Subquery { .. } => rels.push(self.relation(item)?),
            FromItem::Join { left, right, kind: JoinKind::Inner | JoinKind::Cross, on, using } => {
                let first = rels.len();
                self.flatten(left, rels, ons)?;
                let mid = rels.len();
                self.flatten(right, rels, ons)?;
                ons.extend(on.clone());
                if !using.is_empty() {
                    let lcols: Vec<Col> = rels[first..mid].iter().flat_map(|r| r.cols.clone()).collect();
                    let rcols: Vec<Col> = rels[mid..].iter().flat_map(|r| r.cols.clone()).collect();
                    ons.extend(using_conditions(using, &lcols, &rcols)?);
                }
            }
            FromItem::Join { left, right, kind, on, using } => {
                // An outer join: plan both sides, then the join itself.
                let (l_item, r_item) = if *kind == JoinKind::Right { (right, left) } else { (left, right) };
                let l = self.side(l_item)?;
                let r = self.side(r_item)?;
                let (lw, rw) = (l.cols.len(), r.cols.len());
                let saved = std::mem::take(&mut self.frame().scope);
                self.frame().scope = l.cols.iter().chain(&r.cols).cloned().collect();
                let mut preds = Vec::new();
                let res = (|| -> Result<()> {
                    let mut agg = AggCtx { aggs: vec![], allowed: false };
                    if let Some(on) = on {
                        conjuncts(self.expr(on, &mut agg)?, &mut preds);
                    }
                    if !using.is_empty() {
                        for c in using_conditions(using, &l.cols, &r.cols)? {
                            conjuncts(self.expr(&c, &mut agg)?, &mut preds);
                        }
                    }
                    Ok(())
                })();
                self.frame().scope = saved;
                res?;
                let mut plan = optimize::left_join(l, r, preds);
                if *kind == JoinKind::Right {
                    // Put the columns back in the order they were written.
                    let exprs: Vec<Expr> = (lw..lw + rw).chain(0..lw).map(Expr::Col).collect();
                    let cols = exprs
                        .iter()
                        .map(|e| match e {
                            Expr::Col(i) => plan.cols[*i].clone(),
                            _ => unreachable!(),
                        })
                        .collect();
                    plan = optimize::project(plan, exprs, cols);
                }
                rels.push(plan);
            }
        }
        Ok(())
    }

    /// One side of an outer join, planned on its own.
    fn side(&mut self, item: &FromItem) -> Result<Plan> {
        let mut rels = Vec::new();
        let mut ons = Vec::new();
        self.flatten(item, &mut rels, &mut ons)?;
        if rels.len() == 1 && ons.is_empty() {
            return Ok(rels.pop().unwrap());
        }
        let saved = std::mem::take(&mut self.frame().scope);
        self.frame().scope = rels.iter().flat_map(|r| r.cols.clone()).collect();
        let mut preds = Vec::new();
        let res = (|| -> Result<()> {
            for on in ons {
                conjuncts(self.expr(&on, &mut AggCtx { aggs: vec![], allowed: false })?, &mut preds);
            }
            Ok(())
        })();
        self.frame().scope = saved;
        res?;
        optimize::join(rels, preds)
    }

    fn relation(&mut self, item: &FromItem) -> Result<Plan> {
        match item {
            FromItem::Table { name, alias } => {
                if let Some(t) = catalog::get_table(self.txn, name)? {
                    let alias = alias.clone().unwrap_or_else(|| name.clone());
                    let est = self.rows_estimate(&t);
                    let cols =
                        t.columns.iter().map(|c| Col { table: Some(alias.clone()), name: c.name.clone(), ty: Some(c.data_type) }).collect();
                    let node = Node::Scan { table: Rc::new(t), alias, access: Access::Full, filter: None, rows: est };
                    return Ok(Plan::new(node, cols, est, est));
                }
                if let Some(v) = catalog::get_view(self.txn, name)? {
                    if self.view_depth > 16 {
                        return err!("42P17", "infinite recursion detected in view \"{name}\"");
                    }
                    let stmt = super::parser::parse(&v.sql)?;
                    let Some(ast::Statement::Select(q)) = stmt.into_iter().next() else {
                        return err!("XX001", "view \"{name}\" is not a query");
                    };
                    self.view_depth += 1;
                    let res = self.query(&q, false);
                    self.view_depth -= 1;
                    let mut plan = res?;
                    let alias = alias.clone().unwrap_or_else(|| name.clone());
                    for (c, n) in plan.cols.iter_mut().zip(&v.columns) {
                        c.table = Some(alias.clone());
                        c.name = n.clone();
                    }
                    return Ok(plan);
                }
                err!("42P01", "relation \"{name}\" does not exist")
            }
            FromItem::Subquery { query, alias } => {
                let mut plan = self.query(query, false)?;
                for c in plan.cols.iter_mut() {
                    c.table = alias.clone();
                }
                Ok(plan)
            }
            FromItem::Join { .. } => unreachable!(),
        }
    }

    // ---- expressions ---------------------------------------------------------------------

    /// Finds a column by name in the frames, innermost first.
    fn resolve(&mut self, table: Option<&str>, name: &str) -> Result<Expr> {
        let n = self.frames.len();
        for a in (0..n).rev() {
            let scope = &self.frames[a].scope;
            let hits: Vec<usize> = scope
                .iter()
                .enumerate()
                .filter(|(_, c)| c.name == name && table.is_none_or(|t| c.table.as_deref() == Some(t)))
                .map(|(i, _)| i)
                .collect();
            match hits.len() {
                0 => continue,
                1 => {
                    if a == n - 1 {
                        return Ok(Expr::Col(hits[0]));
                    }
                    let depth = self.frames[a + 1..].iter().filter(|f| f.pushes_row).count();
                    for f in &mut self.frames[a + 1..] {
                        f.correlated = true;
                    }
                    return Ok(if depth == 0 { Expr::Col(hits[0]) } else { Expr::Outer(depth, hits[0]) });
                }
                _ => return err!("42702", "column reference \"{name}\" is ambiguous"),
            }
        }
        match table {
            Some(t) if !self.frames.iter().any(|f| f.scope.iter().any(|c| c.table.as_deref() == Some(t))) => {
                err!("42P01", "missing FROM-clause entry for table \"{t}\"")
            }
            Some(t) => err!("42703", "column {t}.{name} does not exist"),
            None => err!("42703", "column \"{name}\" does not exist"),
        }
    }

    fn subquery(&mut self, q: &ast::Query, kind: SubKind, negated: bool, left: Option<Expr>) -> Result<Expr> {
        let depth_before = self.frames.len();
        let plan = self.query(q, true)?;
        debug_assert_eq!(self.frames.len(), depth_before);
        let correlated = plan_is_correlated(&plan);
        if kind != SubKind::Exists && plan.cols.len() != 1 {
            return err!("42601", "subquery must return only one column");
        }
        let id = self.next_sub;
        self.next_sub += 1;
        Ok(Expr::Sub(Rc::new(SubPlan { plan, kind, negated, correlated, id }), left.map(Box::new)))
    }

    fn expr(&mut self, e: &ast::Expr, agg: &mut AggCtx) -> Result<Expr> {
        use ast::Expr as A;
        Ok(match e {
            A::Literal(v) => Expr::Const(v.clone()),
            A::Column(t, n) => self.resolve(t.as_deref(), n)?,
            A::Param(n) => Expr::Param(*n),
            A::Unary(UnOp::Neg, e) => Expr::Neg(Box::new(self.expr(e, agg)?)),
            A::Unary(UnOp::Plus, e) => self.expr(e, agg)?,
            A::Unary(UnOp::Not, e) => Expr::Not(Box::new(self.expr(e, agg)?)),
            A::Binary(op, a, b) => Expr::Bin(*op, Box::new(self.expr(a, agg)?), Box::new(self.expr(b, agg)?)),
            A::Function { name, args, distinct, star } => {
                if AGGREGATES.contains(&name.as_str()) && (*star || args.len() == 1) {
                    if !agg.allowed {
                        return err!("42803", "aggregate functions are not allowed here");
                    }
                    let func = match name.as_str() {
                        "count" => AggFunc::Count,
                        "sum" => AggFunc::Sum,
                        "avg" => AggFunc::Avg,
                        "min" => AggFunc::Min,
                        _ => AggFunc::Max,
                    };
                    if *star && func != AggFunc::Count {
                        return err!("42601", "{name}(*) is not valid");
                    }
                    let arg = if *star {
                        None
                    } else {
                        // No nested aggregates.
                        let mut inner = AggCtx { aggs: vec![], allowed: false };
                        Some(self.expr(&args[0], &mut inner)?)
                    };
                    let a = Agg { func, arg, distinct: *distinct };
                    let k = match agg.aggs.iter().position(|x| *x == a) {
                        Some(k) => k,
                        None => {
                            agg.aggs.push(a);
                            agg.aggs.len() - 1
                        }
                    };
                    return Ok(Expr::AggRef(k));
                }
                let bound = args.iter().map(|a| self.expr(a, agg)).collect::<Result<Vec<_>>>()?;
                let (func, arity) = match name.as_str() {
                    "abs" => (Func::Abs, 1..=1),
                    "coalesce" | "ifnull" => (Func::Coalesce, 1..=usize::MAX),
                    "nullif" => (Func::NullIf, 2..=2),
                    "length" | "char_length" | "character_length" => (Func::Length, 1..=1),
                    "lower" => (Func::Lower, 1..=1),
                    "upper" => (Func::Upper, 1..=1),
                    "substr" | "substring" => (Func::Substr, 2..=3),
                    "trim" | "btrim" => (Func::Trim, 1..=1),
                    "ltrim" => (Func::LTrim, 1..=1),
                    "rtrim" => (Func::RTrim, 1..=1),
                    "round" => (Func::Round, 1..=2),
                    "replace" => (Func::Replace, 3..=3),
                    "least" | "min" => (Func::Least, 2..=usize::MAX),
                    "greatest" | "max" => (Func::Greatest, 2..=usize::MAX),
                    "version" => (Func::Version, 0..=0),
                    "typeof" | "pg_typeof" => (Func::Typeof, 1..=1),
                    _ => return err!("42883", "function {name} does not exist"),
                };
                if !arity.contains(&bound.len()) || *star || *distinct {
                    return err!("42883", "function {name} called with the wrong arguments");
                }
                Expr::Func(func, bound)
            }
            A::Case { operand, whens, else_ } => Expr::Case(
                operand.as_ref().map(|o| self.expr(o, agg).map(Box::new)).transpose()?,
                whens.iter().map(|(w, t)| Ok((self.expr(w, agg)?, self.expr(t, agg)?))).collect::<Result<_>>()?,
                else_.as_ref().map(|e| self.expr(e, agg).map(Box::new)).transpose()?,
            ),
            A::Cast(e, t) => Expr::Cast(Box::new(self.expr(e, agg)?), *t),
            A::InList { expr, list, negated } => {
                let e = self.expr(expr, agg)?;
                if list.is_empty() {
                    // x IN () is false (SQLite); NOT IN () true.
                    return Ok(Expr::Const(Value::Bool(*negated)));
                }
                Expr::InList(Box::new(e), list.iter().map(|x| self.expr(x, agg)).collect::<Result<_>>()?, *negated)
            }
            A::InSubquery { expr, query, negated } => {
                let left = self.expr(expr, agg)?;
                self.subquery(query, SubKind::In, *negated, Some(left))?
            }
            A::Exists { query, negated } => self.subquery(query, SubKind::Exists, *negated, None)?,
            A::Subquery(q) => self.subquery(q, SubKind::Scalar, false, None)?,
            A::Between { expr, low, high, negated } => {
                let x = self.expr(expr, agg)?;
                let lo = Expr::Bin(BinOp::GtEq, Box::new(x.clone()), Box::new(self.expr(low, agg)?));
                let hi = Expr::Bin(BinOp::LtEq, Box::new(x), Box::new(self.expr(high, agg)?));
                let both = Expr::and(lo, hi);
                if *negated { Expr::Not(Box::new(both)) } else { both }
            }
            A::Like { expr, pattern, negated, case_insensitive } => {
                Expr::Like(Box::new(self.expr(expr, agg)?), Box::new(self.expr(pattern, agg)?), *negated, *case_insensitive)
            }
            A::IsNull { expr, negated } => Expr::IsNull(Box::new(self.expr(expr, agg)?), *negated),
        })
    }

    // ---- statements ----------------------------------------------------------------------

    pub fn insert(&mut self, table: &str, columns: &Option<Vec<String>>, source: &ast::InsertSource) -> Result<Plan> {
        let t = catalog::table(self.txn, table)?;
        let targets: Vec<usize> = match columns {
            None => (0..t.columns.len()).collect(),
            Some(names) => {
                let mut seen = HashSet::new();
                names
                    .iter()
                    .map(|n| {
                        let i = t
                            .column(n)
                            .ok_or_else(|| Error::new("42703", format!("column \"{n}\" of relation \"{table}\" does not exist")))?;
                        if !seen.insert(i) {
                            return err!("42701", "column \"{n}\" specified more than once");
                        }
                        Ok(i)
                    })
                    .collect::<Result<_>>()?
            }
        };
        let input = match source {
            ast::InsertSource::Values(rows) => {
                self.frames.push(Frame { scope: vec![], pushes_row: false, correlated: false });
                let r = self.values(rows);
                self.frames.pop();
                r?
            }
            ast::InsertSource::Query(q) => self.query(q, false)?,
            ast::InsertSource::Default => Plan::new(Node::Values { rows: vec![vec![]] }, vec![], 1.0, 1.0),
        };
        if !matches!(source, ast::InsertSource::Default) && input.cols.len() != targets.len() {
            return err!(
                "42601",
                "INSERT has {} expressions than target columns",
                if input.cols.len() > targets.len() { "more" } else { "fewer" }
            );
        }
        // Map the input to the table's full row, defaults filling the gaps.
        let exprs: Vec<Expr> = (0..t.columns.len())
            .map(|c| match targets.iter().position(|&x| x == c) {
                Some(j) if !matches!(source, ast::InsertSource::Default) => Expr::Col(j),
                _ => Expr::Const(t.columns[c].default.clone().unwrap_or(Value::Null)),
            })
            .collect();
        let cols = t.columns.iter().map(|c| Col { table: Some(t.name.clone()), name: c.name.clone(), ty: Some(c.data_type) }).collect();
        let input = optimize::project(input, exprs, cols);
        let (est, cost) = (input.est, input.cost + input.est);
        Ok(Plan::new(Node::Insert { table: Rc::new(t), input: Box::new(input) }, vec![], est, cost))
    }

    /// The rows of a table an UPDATE or DELETE touches: a scan with the WHERE pushed into it.
    fn target_scan(&mut self, table: &str, alias: &Option<String>, filter: &Option<ast::Expr>) -> Result<(Table, Plan)> {
        let t = catalog::table(self.txn, table)?;
        let item = FromItem::Table { name: table.into(), alias: alias.clone() };
        let rel = self.relation(&item)?;
        self.frame().scope = rel.cols.clone();
        let mut preds = Vec::new();
        if let Some(f) = filter {
            conjuncts(self.expr(f, &mut AggCtx { aggs: vec![], allowed: false })?, &mut preds);
        }
        let plan = optimize::join(vec![rel], preds)?;
        Ok((t, plan))
    }

    pub fn update(&mut self, table: &str, alias: &Option<String>, set: &[(String, ast::Expr)], filter: &Option<ast::Expr>) -> Result<Plan> {
        self.frames.push(Frame { scope: vec![], pushes_row: false, correlated: false });
        let res = (|| {
            let (t, scan) = self.target_scan(table, alias, filter)?;
            let mut assignments = Vec::new();
            for (col, e) in set {
                let i =
                    t.column(col).ok_or_else(|| Error::new("42703", format!("column \"{col}\" of relation \"{table}\" does not exist")))?;
                assignments.push((i, self.expr(e, &mut AggCtx { aggs: vec![], allowed: false })?));
            }
            let (est, cost) = (scan.est, scan.cost + scan.est);
            Ok(Plan::new(Node::Update { table: Rc::new(t), input: Box::new(scan), set: assignments }, vec![], est, cost))
        })();
        self.frames.pop();
        res
    }

    pub fn delete(&mut self, table: &str, alias: &Option<String>, filter: &Option<ast::Expr>) -> Result<Plan> {
        self.frames.push(Frame { scope: vec![], pushes_row: false, correlated: false });
        let res = (|| {
            let (t, scan) = self.target_scan(table, alias, filter)?;
            let (est, cost) = (scan.est, scan.cost + scan.est);
            Ok(Plan::new(Node::Delete { table: Rc::new(t), input: Box::new(scan) }, vec![], est, cost))
        })();
        self.frames.pop();
        res
    }

    /// Binds an expression with no row (DEFAULT values, LIMIT).
    pub fn constant(&mut self, e: &ast::Expr) -> Result<Expr> {
        self.frames.push(Frame { scope: vec![], pushes_row: false, correlated: false });
        let r = self.expr(e, &mut AggCtx { aggs: vec![], allowed: false });
        self.frames.pop();
        r
    }
}

fn plan_is_correlated(plan: &Plan) -> bool {
    let mut found = false;
    let mut p = plan.clone();
    fn walk(p: &mut Plan, depth: usize, found: &mut bool) {
        for e in p.exprs_mut() {
            e.visit_outer(depth, found);
        }
        for c in p.children_mut() {
            walk(c, depth, found);
        }
    }
    walk(&mut p, 1, &mut found);
    found
}

impl Expr {
    /// Sets `found` if this expression reaches out of the subquery it sits in at `depth`.
    fn visit_outer(&self, depth: usize, found: &mut bool) {
        match self {
            Expr::Outer(d, _) if *d >= depth => *found = true,
            Expr::Sub(sp, left) => {
                if let Some(l) = left {
                    l.visit_outer(depth, found);
                }
                let mut p = sp.plan.clone();
                fn walk(p: &mut Plan, depth: usize, found: &mut bool) {
                    for e in p.exprs_mut() {
                        e.visit_outer(depth, found);
                    }
                    for c in p.children_mut() {
                        walk(c, depth, found);
                    }
                }
                walk(&mut p, depth + 1, found);
            }
            _ => self.for_each_child(&mut |c| c.visit_outer(depth, found)),
        }
    }
}

fn contains_agg(e: &Expr) -> bool {
    let mut found = matches!(e, Expr::AggRef(_));
    if !found {
        e.for_each_child(&mut |c| found |= contains_agg(c));
    }
    found
}

/// Rewrites an expression bound over the FROM row to run over an aggregate's output row
/// (`g` group expressions, then the aggregates).
fn over_groups(e: &mut Expr, group: &[Expr], g: usize, scope: &[Col]) -> Result<()> {
    if let Some(j) = group.iter().position(|x| x == e) {
        *e = Expr::Col(j);
        return Ok(());
    }
    match e {
        Expr::AggRef(k) => *e = Expr::Col(g + *k),
        Expr::Col(i) => {
            let c = &scope[*i];
            return err!(
                "42803",
                "column \"{}{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                c.table.as_ref().map(|t| format!("{t}.")).unwrap_or_default(),
                c.name
            );
        }
        Expr::Sub(sp, left) => {
            if let Some(l) = left {
                over_groups(l, group, g, scope)?;
            }
            // Outer references from the subquery to this query's row move too.
            let mut bad = None;
            let map = |i: usize| match group.iter().position(|x| *x == Expr::Col(i)) {
                Some(j) => j,
                None => {
                    bad.get_or_insert(i);
                    usize::MAX
                }
            };
            let mut s = (**sp).clone();
            {
                let cell = std::cell::RefCell::new(map);
                let f = |i: usize| (cell.borrow_mut())(i);
                s.plan.remap_outer(1, &f);
            }
            if let Some(i) = bad {
                return err!(
                    "42803",
                    "column \"{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                    scope[i].name
                );
            }
            *sp = Rc::new(s);
        }
        _ => {
            let mut res = Ok(());
            e.for_each_child_mut(&mut |c| {
                if res.is_ok() {
                    res = over_groups(c, group, g, scope);
                }
            });
            res?;
        }
    }
    Ok(())
}

fn using_conditions(using: &[String], left: &[Col], right: &[Col]) -> Result<Vec<ast::Expr>> {
    using
        .iter()
        .map(|c| {
            let l = left
                .iter()
                .find(|x| &x.name == c)
                .ok_or_else(|| Error::new("42703", format!("column \"{c}\" specified in USING clause does not exist in left table")))?;
            let r = right
                .iter()
                .find(|x| &x.name == c)
                .ok_or_else(|| Error::new("42703", format!("column \"{c}\" specified in USING clause does not exist in right table")))?;
            Ok(ast::Expr::Binary(
                BinOp::Eq,
                Box::new(ast::Expr::Column(l.table.clone(), c.clone())),
                Box::new(ast::Expr::Column(r.table.clone(), c.clone())),
            ))
        })
        .collect()
}

/// PostgreSQL's names for unnamed output columns.
fn default_name(e: &ast::Expr) -> String {
    match e {
        ast::Expr::Column(_, n) => n.clone(),
        ast::Expr::Function { name, .. } => name.clone(),
        ast::Expr::Cast(inner, _) => default_name(inner),
        ast::Expr::Case { .. } => "case".into(),
        ast::Expr::Exists { .. } => "exists".into(),
        _ => "?column?".into(),
    }
}
