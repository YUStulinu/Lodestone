//! EXPLAIN output, as text (PostgreSQL's layout) or JSON (PostgreSQL's keys, which the plan
//! viewer reads).

use std::collections::HashMap;

use serde_json::{Map, Value as Json, json};

use super::ast::{BinOp, SetOp};
use super::exec::NodeStats;
use super::plan::*;

fn op(o: BinOp) -> &'static str {
    match o {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Concat => "||",
        BinOp::Eq => "=",
        BinOp::NotEq => "<>",
        BinOp::Lt => "<",
        BinOp::LtEq => "<=",
        BinOp::Gt => ">",
        BinOp::GtEq => ">=",
        BinOp::And => "AND",
        BinOp::Or => "OR",
        BinOp::Is => "IS NOT DISTINCT FROM",
        BinOp::IsNot => "IS DISTINCT FROM",
    }
}

pub fn expr(e: &Expr, cols: &[Col]) -> String {
    let f = |x: &Expr| expr(x, cols);
    match e {
        Expr::Const(v) => v.to_string(),
        Expr::Col(i) => match cols.get(*i) {
            Some(c) => match &c.table {
                Some(t) => format!("{t}.{}", c.name),
                None => c.name.clone(),
            },
            None => format!("#{i}"),
        },
        Expr::Outer(d, i) => format!("$outer{d}.#{i}"),
        Expr::Param(i) => format!("${i}"),
        Expr::Neg(x) => format!("(-{})", f(x)),
        Expr::Not(x) => format!("(NOT {})", f(x)),
        Expr::Bin(o, a, b) => format!("({} {} {})", f(a), op(*o), f(b)),
        Expr::Func(func, args) => format!("{}({})", format!("{func:?}").to_lowercase(), args.iter().map(f).collect::<Vec<_>>().join(", ")),
        Expr::Case(o, whens, e) => {
            let mut s = "CASE".to_string();
            if let Some(o) = o {
                s += &format!(" {}", f(o));
            }
            for (w, t) in whens {
                s += &format!(" WHEN {} THEN {}", f(w), f(t));
            }
            if let Some(e) = e {
                s += &format!(" ELSE {}", f(e));
            }
            s + " END"
        }
        Expr::Cast(x, t) => format!("CAST({} AS {t})", f(x)),
        Expr::InList(x, list, neg) => {
            format!("({} {}IN ({}))", f(x), if *neg { "NOT " } else { "" }, list.iter().map(f).collect::<Vec<_>>().join(", "))
        }
        Expr::Like(x, p, neg, ci) => {
            format!("({} {}{} {})", f(x), if *neg { "NOT " } else { "" }, if *ci { "ILIKE" } else { "LIKE" }, f(p))
        }
        Expr::IsNull(x, neg) => format!("({} IS {}NULL)", f(x), if *neg { "NOT " } else { "" }),
        Expr::Sub(sp, left) => match sp.kind {
            SubKind::Scalar => format!("(SubPlan {})", sp.id + 1),
            SubKind::Exists => format!("({}EXISTS(SubPlan {}))", if sp.negated { "NOT " } else { "" }, sp.id + 1),
            SubKind::In => format!(
                "({} {}IN (SubPlan {}))",
                left.as_ref().map(|l| f(l)).unwrap_or_default(),
                if sp.negated { "NOT " } else { "" },
                sp.id + 1
            ),
        },
        Expr::AggRef(k) => format!("agg#{k}"),
    }
}

fn agg(a: &Agg, cols: &[Col]) -> String {
    let name = format!("{:?}", a.func).to_lowercase();
    match &a.arg {
        None => format!("{name}(*)"),
        Some(e) => format!("{name}({}{})", if a.distinct { "DISTINCT " } else { "" }, expr(e, cols)),
    }
}

fn bound(b: &Option<(Expr, bool)>, cols: &[Col], lower: bool) -> Option<String> {
    b.as_ref().map(|(e, inc)| {
        format!(
            "{} {}",
            if lower {
                if *inc { ">=" } else { ">" }
            } else if *inc {
                "<="
            } else {
                "<"
            },
            expr(e, cols)
        )
    })
}

/// A node's name and its details.
fn describe(p: &Plan) -> (String, Vec<(&'static str, String)>, Map<String, Json>) {
    let mut d: Vec<(&'static str, String)> = Vec::new();
    let mut extra = Map::new();
    let input_cols = |i: &Plan| i.cols.clone();
    let name = match &p.node {
        Node::Scan { table, alias, access, filter, .. } => {
            extra.insert("Relation Name".into(), json!(table.name));
            if alias != &table.name {
                extra.insert("Alias".into(), json!(alias));
            }
            let cols = &p.cols;
            let pk: Vec<Col> = table.primary_key.iter().map(|&c| cols[c].clone()).collect();
            let name = match access {
                Access::Full => format!("Seq Scan on {}", table.name),
                Access::Keys(keys) => {
                    let ks: Vec<String> = keys
                        .iter()
                        .map(|k| {
                            k.iter().zip(&pk).map(|(e, c)| format!("{} = {}", c.name, expr(e, cols))).collect::<Vec<_>>().join(" AND ")
                        })
                        .collect();
                    d.push(("Key Cond", ks.join(" OR ")));
                    format!("Primary Key Lookup on {}", table.name)
                }
                Access::KeyRange { lo, hi } => {
                    let c = &pk[0].name;
                    let parts: Vec<String> =
                        [bound(lo, cols, true), bound(hi, cols, false)].into_iter().flatten().map(|b| format!("{c} {b}")).collect();
                    d.push(("Key Cond", parts.join(" AND ")));
                    format!("Primary Key Range Scan on {}", table.name)
                }
                Access::IndexEq { index, keys } => {
                    let ix = &table.indexes[*index];
                    extra.insert("Index Name".into(), json!(ix.name));
                    let ks: Vec<String> = keys
                        .iter()
                        .map(|k| {
                            k.iter()
                                .zip(&ix.columns)
                                .map(|(e, &c)| format!("{} = {}", table.columns[c].name, expr(e, cols)))
                                .collect::<Vec<_>>()
                                .join(" AND ")
                        })
                        .collect();
                    d.push(("Index Cond", ks.join(" OR ")));
                    format!("Index Scan using {} on {}", ix.name, table.name)
                }
                Access::IndexRange { index, lo, hi } => {
                    let ix = &table.indexes[*index];
                    extra.insert("Index Name".into(), json!(ix.name));
                    let c = &table.columns[ix.columns[0]].name;
                    let parts: Vec<String> =
                        [bound(lo, cols, true), bound(hi, cols, false)].into_iter().flatten().map(|b| format!("{c} {b}")).collect();
                    d.push(("Index Cond", parts.join(" AND ")));
                    format!("Index Range Scan using {} on {}", ix.name, table.name)
                }
            };
            if let Some(f) = filter {
                d.push(("Filter", expr(f, cols)));
            }
            name
        }
        Node::Values { rows } => {
            if rows.len() == 1 && rows[0].is_empty() {
                "Result".into()
            } else {
                format!("Values ({} rows)", rows.len())
            }
        }
        Node::Filter { input, pred } => {
            d.push(("Filter", expr(pred, &input_cols(input))));
            "Filter".into()
        }
        Node::Project { input, exprs } => {
            let c = input_cols(input);
            d.push(("Output", exprs.iter().map(|e| expr(e, &c)).collect::<Vec<_>>().join(", ")));
            "Project".into()
        }
        Node::NestedLoop { kind, on, .. } => {
            if let Some(on) = on {
                d.push(("Join Filter", expr(on, &p.cols)));
            }
            if *kind == JoinType::Left { "Nested Loop Left Join".into() } else { "Nested Loop".into() }
        }
        Node::HashJoin { left, right, kind, left_keys, right_keys, residual } => {
            let (lc, rc) = (input_cols(left), input_cols(right));
            let cond: Vec<String> = left_keys.iter().zip(right_keys).map(|(l, r)| format!("{} = {}", expr(l, &lc), expr(r, &rc))).collect();
            d.push(("Hash Cond", cond.join(" AND ")));
            if let Some(r) = residual {
                d.push(("Join Filter", expr(r, &p.cols)));
            }
            if *kind == JoinType::Left { "Hash Left Join".into() } else { "Hash Join".into() }
        }
        Node::Aggregate { input, group, aggs } => {
            let c = input_cols(input);
            if !group.is_empty() {
                d.push(("Group Key", group.iter().map(|g| expr(g, &c)).collect::<Vec<_>>().join(", ")));
            }
            d.push(("Aggregates", aggs.iter().map(|a| agg(a, &c)).collect::<Vec<_>>().join(", ")));
            if group.is_empty() { "Aggregate".into() } else { "HashAggregate".into() }
        }
        Node::Sort { input, keys } => {
            let c = input_cols(input);
            d.push((
                "Sort Key",
                keys.iter()
                    .map(|k| {
                        format!(
                            "{}{}{}",
                            expr(&k.expr, &c),
                            if k.desc { " DESC" } else { "" },
                            if k.nulls_first != k.desc { if k.nulls_first { " NULLS FIRST" } else { " NULLS LAST" } } else { "" }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
            "Sort".into()
        }
        Node::Limit { limit, offset, .. } => {
            if let Some(l) = limit {
                d.push(("Limit", expr(l, &[])));
            }
            if let Some(o) = offset {
                d.push(("Offset", expr(o, &[])));
            }
            "Limit".into()
        }
        Node::Distinct { .. } => "HashDistinct".into(),
        Node::SetOp { op, all, .. } => format!(
            "{}{}",
            match op {
                SetOp::Union => "Union",
                SetOp::Intersect => "Intersect",
                SetOp::Except => "Except",
            },
            if *all { " All" } else { "" }
        ),
        Node::Insert { table, .. } => format!("Insert on {}", table.name),
        Node::Update { table, input, set } => {
            let c = input_cols(input);
            d.push((
                "Set",
                set.iter().map(|(i, e)| format!("{} = {}", table.columns[*i].name, expr(e, &c))).collect::<Vec<_>>().join(", "),
            ));
            format!("Update on {}", table.name)
        }
        Node::Delete { table, .. } => format!("Delete on {}", table.name),
    };
    (name, d, extra)
}

/// Subqueries used by a node's expressions (not those nested inside them).
fn subplans(p: &Plan) -> Vec<Rc<SubPlan>> {
    let mut out = Vec::new();
    fn walk(e: &Expr, out: &mut Vec<Rc<SubPlan>>) {
        if let Expr::Sub(sp, left) = e {
            if let Some(l) = left {
                walk(l, out);
            }
            out.push(sp.clone());
            return;
        }
        e.for_each_child(&mut |c| walk(c, out));
    }
    let mut copy = p.clone();
    for e in copy.exprs_mut() {
        walk(e, &mut out);
    }
    out
}

use std::rc::Rc;

pub fn text(p: &Plan, stats: Option<&HashMap<usize, NodeStats>>) -> Vec<String> {
    let mut out = Vec::new();
    text_node(p, stats, 0, None, &mut out);
    out
}

fn actual(p: &Plan, stats: Option<&HashMap<usize, NodeStats>>) -> String {
    match stats {
        None => String::new(),
        Some(s) => match s.get(&p.id) {
            None => " (never executed)".into(),
            Some(n) => {
                let loops = n.loops.max(1);
                format!(" (actual time={:.3} ms rows={} loops={})", n.nanos as f64 / 1e6, n.rows / loops, n.loops)
            }
        },
    }
}

fn text_node(p: &Plan, stats: Option<&HashMap<usize, NodeStats>>, depth: usize, label: Option<String>, out: &mut Vec<String>) {
    // PostgreSQL's layout: children under "->", details indented past the arrow.
    let (name, details, _) = describe(p);
    let indent = if depth == 0 { 0 } else { 6 * (depth - 1) + 2 };
    if let Some(l) = label {
        out.push(format!("{}{l}", " ".repeat(indent)));
    }
    let arrow = if depth == 0 { String::new() } else { format!("{}->  ", " ".repeat(indent)) };
    out.push(format!("{arrow}{name}  (cost={:.2} rows={:.0}){}", p.cost, p.est, actual(p, stats)));
    for (k, v) in details {
        out.push(format!("{}{k}: {v}", " ".repeat(6 * depth + 2)));
    }
    for c in p.children() {
        text_node(c, stats, depth + 1, None, out);
    }
    for sp in subplans(p) {
        let label = format!("SubPlan {}{}", sp.id + 1, if sp.correlated { "" } else { " (runs once)" });
        text_node(&sp.plan, stats, depth + 1, Some(label), out);
    }
}

pub fn json(p: &Plan, stats: Option<&HashMap<usize, NodeStats>>) -> Json {
    let (name, details, mut obj) = describe(p);
    obj.insert("Node Type".into(), json!(name));
    obj.insert("Plan Rows".into(), json!(p.est.round()));
    obj.insert("Total Cost".into(), json!((p.cost * 100.0).round() / 100.0));
    if let Some(s) = stats {
        match s.get(&p.id) {
            Some(n) => {
                obj.insert("Actual Rows".into(), json!(n.rows / n.loops.max(1)));
                obj.insert("Actual Loops".into(), json!(n.loops));
                obj.insert("Actual Total Time".into(), json!((n.nanos as f64 / 1e3).round() / 1e3));
            }
            None => {
                obj.insert("Actual Loops".into(), json!(0));
            }
        }
    }
    for (k, v) in details {
        obj.insert(k.into(), json!(v));
    }
    let mut plans: Vec<Json> = p.children().into_iter().map(|c| json(c, stats)).collect();
    for sp in subplans(p) {
        let mut j = json(&sp.plan, stats);
        if let Json::Object(o) = &mut j {
            o.insert("Subplan Name".into(), json!(format!("SubPlan {}", sp.id + 1)));
            o.insert("Correlated".into(), json!(sp.correlated));
        }
        plans.push(j);
    }
    if !plans.is_empty() {
        obj.insert("Plans".into(), Json::Array(plans));
    }
    Json::Object(obj)
}
