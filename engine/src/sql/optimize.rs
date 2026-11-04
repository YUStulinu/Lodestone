//! The optimiser: constant folding, pushing predicates down to the tables they read, choosing
//! how each table is read (primary key or index lookups and ranges, or a full scan), and
//! ordering inner joins greedily -- smallest estimated input first, then whichever relation
//! joins to what is already joined by an equality, as hash joins built on the smaller side.
//!
//! Estimates come from per-table row counters and fixed selectivities (equality 10%, range
//! 30%, ...); there are no histograms.

use std::collections::{BTreeSet, HashSet};

use super::ast::BinOp;
use super::catalog::Table;
use super::exec;
use super::plan::*;
use super::types::{DataType, Result, Value};

/// Folds the parts of an expression that depend on nothing.
pub fn fold(e: Expr) -> Expr {
    let mut e = e;
    e.for_each_child_mut(&mut |c| {
        let taken = std::mem::replace(c, Expr::Const(Value::Null));
        *c = fold(taken);
    });
    let foldable = !matches!(e, Expr::Const(_)) && is_pure_const(&e);
    if foldable {
        if let Ok(v) = exec::eval_const(&e) {
            return Expr::Const(v);
        }
    }
    // TRUE AND x = x; FALSE AND x = FALSE (x has no side effects).
    if let Expr::Bin(BinOp::And, a, b) = &e {
        match (&**a, &**b) {
            (Expr::Const(Value::Bool(true)), _) => return (**b).clone(),
            (_, Expr::Const(Value::Bool(true))) => return (**a).clone(),
            _ => {}
        }
    }
    e
}

fn is_pure_const(e: &Expr) -> bool {
    match e {
        Expr::Const(_) => true,
        Expr::Col(_) | Expr::Outer(..) | Expr::Param(_) | Expr::Sub(..) | Expr::AggRef(_) => false,
        Expr::Func(Func::Version, _) => false,
        _ => e.children().into_iter().all(is_pure_const),
    }
}

/// A guess at the fraction of rows a predicate keeps.
pub fn selectivity(e: &Expr) -> f64 {
    match e {
        Expr::Const(Value::Bool(true)) => 1.0,
        Expr::Const(Value::Bool(false)) | Expr::Const(Value::Null) => 0.0,
        Expr::Bin(BinOp::And, a, b) => selectivity(a) * selectivity(b),
        Expr::Bin(BinOp::Or, a, b) => {
            let (x, y) = (selectivity(a), selectivity(b));
            x + y - x * y
        }
        Expr::Not(a) => 1.0 - selectivity(a),
        Expr::Bin(BinOp::Eq | BinOp::Is, ..) => 0.1,
        Expr::Bin(BinOp::NotEq | BinOp::IsNot, ..) => 0.9,
        Expr::Bin(BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq, ..) => 0.3,
        Expr::InList(_, list, false) => (0.1 * list.len() as f64).min(0.5),
        Expr::Like(..) => 0.25,
        Expr::IsNull(_, false) => 0.1,
        Expr::IsNull(_, true) => 0.9,
        _ => 0.5,
    }
}

pub fn filter(plan: Plan, pred: Expr) -> Plan {
    let pred = fold(pred);
    if pred == Expr::Const(Value::Bool(true)) {
        return plan;
    }
    if let Node::Scan { .. } = plan.node {
        return push_into_scan(plan, pred);
    }
    let cols = plan.cols.clone();
    let est = plan.est * selectivity(&pred);
    let cost = plan.cost + plan.est;
    Plan::new(Node::Filter { input: Box::new(plan), pred }, cols, est, cost)
}

pub fn project(plan: Plan, exprs: Vec<Expr>, cols: Vec<Col>) -> Plan {
    let identity = exprs.len() == plan.cols.len() && exprs.iter().enumerate().all(|(i, e)| *e == Expr::Col(i));
    if identity {
        let mut plan = plan;
        plan.cols = cols;
        return plan;
    }
    let exprs = exprs.into_iter().map(fold).collect();
    let (est, cost) = (plan.est, plan.cost + plan.est * 0.1);
    Plan::new(Node::Project { input: Box::new(plan), exprs }, cols, est, cost)
}

pub fn sort(plan: Plan, keys: Vec<SortKey>) -> Plan {
    let n = plan.est.max(1.0);
    let cols = plan.cols.clone();
    let (est, cost) = (plan.est, plan.cost + n * n.log2().max(1.0));
    Plan::new(Node::Sort { input: Box::new(plan), keys }, cols, est, cost)
}

/// Can a value of this (static) type be looked up in a column of that type's key order?
fn key_compatible(col: DataType, value: Option<DataType>) -> bool {
    match (col, value) {
        (_, None) => true,
        (DataType::Int | DataType::Float, Some(DataType::Int | DataType::Float)) => true,
        (a, Some(b)) => a == b,
    }
}

/// `col <op> constant` in either order, normalised to the column on the left.
fn column_compare(e: &Expr) -> Option<(usize, BinOp, &Expr)> {
    let Expr::Bin(op, a, b) = e else { return None };
    let flip = |op: BinOp| match op {
        BinOp::Lt => BinOp::Gt,
        BinOp::LtEq => BinOp::GtEq,
        BinOp::Gt => BinOp::Lt,
        BinOp::GtEq => BinOp::LtEq,
        o => o,
    };
    if !matches!(op, BinOp::Eq | BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq) {
        return None;
    }
    match (&**a, &**b) {
        (Expr::Col(i), c) if c.is_row_independent() => Some((*i, *op, c)),
        (c, Expr::Col(i)) if c.is_row_independent() => Some((*i, flip(*op), c)),
        _ => None,
    }
}

type Bound2 = (Option<(Expr, bool)>, Option<(Expr, bool)>);

fn range_on(conj: &[Expr], col: usize, ty: DataType) -> Bound2 {
    let (mut lo, mut hi) = (None, None);
    for c in conj {
        if let Some((i, op, v)) = column_compare(c) {
            if i != col || !key_compatible(ty, v.data_type(&[])) {
                continue;
            }
            match op {
                BinOp::Gt | BinOp::GtEq if lo.is_none() => lo = Some((v.clone(), op == BinOp::GtEq)),
                BinOp::Lt | BinOp::LtEq if hi.is_none() => hi = Some((v.clone(), op == BinOp::LtEq)),
                _ => {}
            }
        }
    }
    (lo, hi)
}

/// Equality lookups on a column: `col = c` gives one value, `col IN (c1, c2, ...)` several.
fn eq_values(conj: &[Expr], col: usize, ty: DataType) -> Option<Vec<Expr>> {
    for c in conj {
        if let Some((i, BinOp::Eq, v)) = column_compare(c) {
            if i == col && key_compatible(ty, v.data_type(&[])) {
                return Some(vec![v.clone()]);
            }
        }
        if let Expr::InList(x, list, false) = c {
            if **x == Expr::Col(col) && list.iter().all(|v| v.is_row_independent() && key_compatible(ty, v.data_type(&[]))) {
                return Some(list.clone());
            }
        }
    }
    None
}

/// Picks how a scan reads its table, given its filter.
fn choose_access(table: &Table, filter: &Option<Expr>, n: f64) -> (Access, f64, f64) {
    let mut conj = Vec::new();
    if let Some(f) = filter {
        conjuncts(f.clone(), &mut conj);
    }
    let ty = |c: usize| table.columns[c].data_type;
    let log = n.max(2.0).log2();
    let mut best = (Access::Full, n, n);
    let mut consider = |access: Access, rows: f64, cost: f64| {
        if cost < best.2 {
            best = (access, rows, cost);
        }
    };
    // Primary key: every column equal, or a range on a single-column key.
    if !table.primary_key.is_empty() {
        let vals: Option<Vec<Vec<Expr>>> = table.primary_key.iter().map(|&c| eq_values(&conj, c, ty(c))).collect();
        if let Some(vals) = vals {
            // The cross product of each column's values.
            let mut keys: Vec<Vec<Expr>> = vec![vec![]];
            for v in vals {
                keys = keys.iter().flat_map(|k| v.iter().map(move |x| [k.clone(), vec![x.clone()]].concat())).collect();
            }
            if keys.len() <= 1000 {
                let k = keys.len() as f64;
                consider(Access::Keys(keys), k, k * log);
            }
        } else if table.primary_key.len() == 1 {
            let (lo, hi) = range_on(&conj, table.primary_key[0], ty(table.primary_key[0]));
            if lo.is_some() || hi.is_some() {
                let frac = if lo.is_some() && hi.is_some() { 0.1 } else { 0.3 };
                consider(Access::KeyRange { lo, hi }, n * frac, n * frac + log);
            }
        }
    }
    for (ix, index) in table.indexes.iter().enumerate() {
        // Equality on a prefix of the index's columns.
        let mut prefix: Vec<Vec<Expr>> = Vec::new();
        for &c in &index.columns {
            match eq_values(&conj, c, ty(c)) {
                Some(v) if prefix.is_empty() || v.len() == 1 => prefix.push(v),
                _ => break,
            }
        }
        if !prefix.is_empty() {
            let mut keys: Vec<Vec<Expr>> = vec![vec![]];
            for v in &prefix {
                keys = keys.iter().flat_map(|k| v.iter().map(move |x| [k.clone(), vec![x.clone()]].concat())).collect();
            }
            let full = prefix.len() == index.columns.len();
            let per_key = if index.unique && full { 1.0 } else { (n * 0.1f64.powi(prefix.len() as i32)).max(1.0) };
            let k = keys.len() as f64;
            consider(Access::IndexEq { index: ix, keys }, k * per_key, k * (log + per_key * 2.0));
            continue;
        }
        let first = index.columns[0];
        let (lo, hi) = range_on(&conj, first, ty(first));
        if lo.is_some() || hi.is_some() {
            let frac = if lo.is_some() && hi.is_some() { 0.1 } else { 0.3 };
            consider(Access::IndexRange { index: ix, lo, hi }, n * frac, n * frac * 2.0 + log);
        }
    }
    best
}

fn push_into_scan(mut plan: Plan, pred: Expr) -> Plan {
    let Node::Scan { table, filter, access, rows, .. } = &mut plan.node else { unreachable!() };
    *filter = Some(match filter.take() {
        Some(f) => Expr::and(f, pred),
        None => pred,
    });
    let n = *rows;
    let (a, found, cost) = choose_access(table, filter, n);
    *access = a;
    let sel = filter.as_ref().map_or(1.0, selectivity);
    plan.est = (n * sel).min(found).max(if sel > 0.0 { 1.0 } else { 0.0 });
    plan.cost = cost;
    plan
}

/// Relations of the FROM clause: which relation each column of the combined row belongs to.
struct Layout {
    offsets: Vec<usize>,
    widths: Vec<usize>,
}

impl Layout {
    fn rel_of(&self, col: usize) -> usize {
        (0..self.offsets.len()).find(|&r| self.offsets[r] <= col && col < self.offsets[r] + self.widths[r]).expect("column in a relation")
    }
}

struct Pred {
    expr: Expr,
    rels: BTreeSet<usize>,
}

/// Plans the inner joins of a FROM clause. The result's columns are the relations' columns in
/// FROM order, whatever order the joins run in.
pub fn join(rels: Vec<Plan>, preds: Vec<Expr>) -> Result<Plan> {
    let widths: Vec<usize> = rels.iter().map(|r| r.cols.len()).collect();
    let offsets: Vec<usize> = widths.iter().scan(0, |acc, w| {
        let o = *acc;
        *acc += w;
        Some(o)
    }).collect();
    let layout = Layout { offsets: offsets.clone(), widths: widths.clone() };
    let mut rels: Vec<Option<Plan>> = rels.into_iter().map(Some).collect();
    let mut multi: Vec<Pred> = Vec::new();
    for p in preds {
        let p = fold(p);
        if p == Expr::Const(Value::Bool(true)) {
            continue;
        }
        let set: BTreeSet<usize> = p.columns().into_iter().map(|c| layout.rel_of(c)).collect();
        if set.len() <= 1 {
            let r = set.into_iter().next().unwrap_or(0);
            let off = offsets[r];
            let mut local = p;
            local.remap(0, &|i| i - off);
            let plan = rels[r].take().unwrap();
            rels[r] = Some(filter(plan, local));
        } else {
            multi.push(Pred { expr: p, rels: set });
        }
    }
    let n = rels.len();
    if n == 1 {
        return Ok(rels.pop().unwrap().unwrap());
    }

    // Greedy: start from the smallest, add the cheapest relation that joins by equality.
    let mut joined: Vec<usize> = Vec::new(); // relations in row order
    let start = (0..n).min_by(|&a, &b| rels[a].as_ref().unwrap().est.total_cmp(&rels[b].as_ref().unwrap().est)).unwrap();
    let mut current = rels[start].take().unwrap();
    joined.push(start);
    let position = |joined: &[usize], col: usize| -> usize {
        let r = layout.rel_of(col);
        let mut pos = 0;
        for &j in joined {
            if j == r {
                return pos + (col - offsets[r]);
            }
            pos += widths[j];
        }
        unreachable!("column of an unjoined relation")
    };
    while joined.len() < n {
        let inside: BTreeSet<usize> = joined.iter().copied().collect();
        let connects = |r: usize| {
            multi.iter().any(|p| p.rels.contains(&r) && p.rels.iter().all(|x| *x == r || inside.contains(x)) && equi_sides(&p.expr, &layout, &inside, r).is_some())
        };
        let candidates: Vec<usize> = (0..n).filter(|&r| rels[r].is_some()).collect();
        let next = candidates
            .iter()
            .copied()
            .filter(|&r| connects(r))
            .min_by(|&a, &b| rels[a].as_ref().unwrap().est.total_cmp(&rels[b].as_ref().unwrap().est))
            .or_else(|| candidates.iter().copied().min_by(|&a, &b| rels[a].as_ref().unwrap().est.total_cmp(&rels[b].as_ref().unwrap().est)))
            .unwrap();
        let right = rels[next].take().unwrap();
        // Predicates now fully available.
        let (ready, rest): (Vec<Pred>, Vec<Pred>) = multi.into_iter().partition(|p| p.rels.iter().all(|x| *x == next || inside.contains(x)));
        multi = rest;
        let mut new_joined = joined.clone();
        new_joined.push(next);
        let (mut lkeys, mut rkeys, mut residual) = (Vec::new(), Vec::new(), Vec::new());
        for p in ready {
            match equi_sides(&p.expr, &layout, &inside, next) {
                Some((mut l, mut r)) => {
                    l.remap(0, &|c| position(&joined, c));
                    let off = offsets[next];
                    r.remap(0, &|c| c - off);
                    lkeys.push(l);
                    rkeys.push(r);
                }
                None => {
                    let mut e = p.expr;
                    e.remap(0, &|c| position(&new_joined, c));
                    residual.push(e);
                }
            }
        }
        current = make_join(current, right, JoinType::Inner, lkeys, rkeys, and_all(residual), &mut new_joined);
        joined = new_joined;
    }
    // Back to FROM order.
    let exprs: Vec<Expr> = (0..offsets.last().unwrap() + widths.last().unwrap()).map(|c| Expr::Col(position(&joined, c))).collect();
    let cols: Vec<Col> = exprs.iter().map(|e| match e { Expr::Col(i) => current.cols[*i].clone(), _ => unreachable!() }).collect();
    Ok(project(current, exprs, cols))
}

/// For `a = b` with one side over the joined relations and the other over `r`: (joined side, r side).
fn equi_sides(e: &Expr, layout: &Layout, inside: &BTreeSet<usize>, r: usize) -> Option<(Expr, Expr)> {
    let Expr::Bin(BinOp::Eq, a, b) = e else { return None };
    if a.has_subquery() || b.has_subquery() {
        return None;
    }
    let side = |x: &Expr| -> Option<bool> {
        let rels: HashSet<usize> = x.columns().into_iter().map(|c| layout.rel_of(c)).collect();
        if rels.is_empty() {
            None
        } else if rels.iter().all(|q| inside.contains(q)) {
            Some(true)
        } else if rels.iter().all(|q| *q == r) {
            Some(false)
        } else {
            None
        }
    };
    match (side(a)?, side(b)?) {
        (true, false) => Some(((**a).clone(), (**b).clone())),
        (false, true) => Some(((**b).clone(), (**a).clone())),
        _ => None,
    }
}

/// Joins `left` (row order `order` minus its last entry) with `right` (the last entry). A
/// hash join builds on the smaller input, which may put the right relation first; `order` is
/// updated to the row order produced.
fn make_join(left: Plan, right: Plan, kind: JoinType, lkeys: Vec<Expr>, rkeys: Vec<Expr>, residual: Option<Expr>, order: &mut Vec<usize>) -> Plan {
    let lw = left.cols.len();
    let rw = right.cols.len();
    let est_join = if lkeys.is_empty() { left.est * right.est } else { left.est.max(right.est) };
    let est = est_join * residual.as_ref().map_or(1.0, selectivity);
    if lkeys.is_empty() {
        let cols: Vec<Col> = left.cols.iter().chain(&right.cols).cloned().collect();
        let cost = left.cost + right.cost + left.est * right.est;
        return Plan::new(Node::NestedLoop { left: Box::new(left), right: Box::new(right), kind, on: residual }, cols, est, cost);
    }
    if kind == JoinType::Inner && right.est > left.est {
        // Build on the smaller side: swap, and the new relation comes first in the row.
        let last = order.pop().unwrap();
        order.insert(0, last);
        let mut residual = residual;
        if let Some(r) = residual.as_mut() {
            // Residual was written for [left, right]; now it is [right, left].
            r.remap(0, &|c| if c < lw { c + rw } else { c - lw });
        }
        let cols: Vec<Col> = right.cols.iter().chain(&left.cols).cloned().collect();
        let cost = left.cost + right.cost + left.est + right.est;
        return Plan::new(
            Node::HashJoin { left: Box::new(right), right: Box::new(left), kind, left_keys: rkeys, right_keys: lkeys, residual },
            cols,
            est,
            cost,
        );
    }
    let cols: Vec<Col> = left.cols.iter().chain(&right.cols).cloned().collect();
    let cost = left.cost + right.cost + left.est + right.est;
    Plan::new(Node::HashJoin { left: Box::new(left), right: Box::new(right), kind, left_keys: lkeys, right_keys: rkeys, residual }, cols, est, cost)
}

/// A LEFT JOIN of two planned sides; `preds` are over [left columns, right columns].
pub fn left_join(left: Plan, right: Plan, preds: Vec<Expr>) -> Plan {
    let lw = left.cols.len();
    let mut right = right;
    let (mut lkeys, mut rkeys, mut residual) = (Vec::new(), Vec::new(), Vec::new());
    for p in preds.into_iter().map(fold) {
        let cols = p.columns();
        if !cols.is_empty() && cols.iter().all(|&c| c >= lw) && !p.has_subquery() {
            // Only the right side: filter it before joining (rows it removes become NULLs).
            let mut q = p;
            q.remap(0, &|c| c - lw);
            right = filter(right, q);
            continue;
        }
        if let Expr::Bin(BinOp::Eq, a, b) = &p {
            let (ca, cb) = (a.columns(), b.columns());
            let left_only = |s: &HashSet<usize>| !s.is_empty() && s.iter().all(|&c| c < lw);
            let right_only = |s: &HashSet<usize>| !s.is_empty() && s.iter().all(|&c| c >= lw);
            if !a.has_subquery() && !b.has_subquery() {
                let pair = if left_only(&ca) && right_only(&cb) {
                    Some(((**a).clone(), (**b).clone()))
                } else if left_only(&cb) && right_only(&ca) {
                    Some(((**b).clone(), (**a).clone()))
                } else {
                    None
                };
                if let Some((l, mut r)) = pair {
                    r.remap(0, &|c| c - lw);
                    lkeys.push(l);
                    rkeys.push(r);
                    continue;
                }
            }
        }
        residual.push(p);
    }
    let est_inner = if lkeys.is_empty() { left.est * right.est } else { left.est.max(right.est) };
    let est = est_inner.max(left.est);
    let cols: Vec<Col> = left.cols.iter().chain(&right.cols).cloned().collect();
    let residual = and_all(residual);
    if lkeys.is_empty() {
        let cost = left.cost + right.cost + left.est * right.est;
        return Plan::new(Node::NestedLoop { left: Box::new(left), right: Box::new(right), kind: JoinType::Left, on: residual }, cols, est, cost);
    }
    let cost = left.cost + right.cost + left.est + right.est;
    Plan::new(
        Node::HashJoin { left: Box::new(left), right: Box::new(right), kind: JoinType::Left, left_keys: lkeys, right_keys: rkeys, residual },
        cols,
        est,
        cost,
    )
}
