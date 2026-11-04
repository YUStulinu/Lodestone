//! The syntax tree the parser produces.

use super::types::{DataType, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Select(Box<Query>),
    Insert { table: String, columns: Option<Vec<String>>, source: InsertSource },
    Update { table: String, alias: Option<String>, set: Vec<(String, Expr)>, filter: Option<Expr> },
    Delete { table: String, alias: Option<String>, filter: Option<Expr> },
    CreateTable { name: String, if_not_exists: bool, columns: Vec<ColumnDef>, primary_key: Vec<String>, unique: Vec<Vec<String>> },
    DropTable { name: String, if_exists: bool },
    CreateIndex { name: String, table: String, columns: Vec<String>, unique: bool, if_not_exists: bool },
    DropIndex { name: String, if_exists: bool },
    CreateView { name: String, if_not_exists: bool, query: Box<Query>, sql: String },
    DropView { name: String, if_exists: bool },
    Begin { read_only: bool },
    Commit,
    Rollback,
    Explain { analyze: bool, json: bool, statement: Box<Statement> },
    Vacuum,
    Checkpoint,
    ShowTables,
    Reindex(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Query(Box<Query>),
    Default,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub body: SetExpr,
    pub order_by: Vec<OrderItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetExpr {
    Select(Box<Select>),
    Values(Vec<Vec<Expr>>),
    SetOp { op: SetOp, all: bool, left: Box<SetExpr>, right: Box<SetExpr> },
    Query(Box<Query>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    Union,
    Intersect,
    Except,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Vec<FromItem>,
    pub filter: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Wildcard,
    QualifiedWildcard(String),
    Expr(Expr, Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum FromItem {
    Table { name: String, alias: Option<String> },
    Subquery { query: Box<Query>, alias: Option<String> },
    Join { left: Box<FromItem>, right: Box<FromItem>, kind: JoinKind, on: Option<Expr>, using: Vec<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub desc: bool,
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Is,
    IsNot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Plus,
    Not,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column(Option<String>, String),
    Param(usize),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Function { name: String, args: Vec<Expr>, distinct: bool, star: bool },
    Case { operand: Option<Box<Expr>>, whens: Vec<(Expr, Expr)>, else_: Option<Box<Expr>> },
    Cast(Box<Expr>, DataType),
    InList { expr: Box<Expr>, list: Vec<Expr>, negated: bool },
    InSubquery { expr: Box<Expr>, query: Box<Query>, negated: bool },
    Exists { query: Box<Query>, negated: bool },
    Subquery(Box<Query>),
    Between { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr>, negated: bool },
    Like { expr: Box<Expr>, pattern: Box<Expr>, negated: bool, case_insensitive: bool },
    IsNull { expr: Box<Expr>, negated: bool },
}
