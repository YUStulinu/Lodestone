//! A recursive-descent SQL parser; expressions by precedence climbing with PostgreSQL's
//! precedences (OR < AND < NOT < IS < comparison < BETWEEN/IN/LIKE < || < + - < * / % < unary < ::).

use super::ast::*;
use super::lexer::{Token, lex_with_pos};
use super::types::{DataType, Result, Value, err};

pub struct Parser<'a> {
    sql: &'a str,
    tokens: Vec<(Token, usize)>,
    pos: usize,
}

/// Words that end an expression or cannot be an alias.
const RESERVED: &[&str] = &[
    "select", "from", "where", "group", "having", "order", "limit", "offset", "union", "intersect", "except", "join",
    "inner", "left", "right", "cross", "on", "using", "and", "or", "not", "as", "by", "asc", "desc", "into", "values",
    "set", "when", "then", "else", "end", "case", "is", "in", "between", "like", "ilike", "null", "outer", "full",
    "natural", "all", "distinct", "create", "drop", "insert", "update", "delete", "returning", "nulls", "window",
];

pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let mut p = Parser { sql, tokens: lex_with_pos(sql)?, pos: 0 };
    let mut out = Vec::new();
    loop {
        while p.eat_sym(";") {}
        if p.pos >= p.tokens.len() {
            return Ok(out);
        }
        out.push(p.statement()?);
        if p.pos < p.tokens.len() && !p.eat_sym(";") {
            return p.unexpected();
        }
    }
}

pub fn parse_expr(sql: &str) -> Result<Expr> {
    let mut p = Parser { sql, tokens: lex_with_pos(sql)?, pos: 0 };
    let e = p.expr()?;
    if p.pos < p.tokens.len() {
        return p.unexpected();
    }
    Ok(e)
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos).map(|t| &t.0)
    }

    fn peek_at(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.pos + n).map(|t| &t.0)
    }

    fn next(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).map(|t| t.0.clone());
        self.pos += 1;
        t
    }

    fn unexpected<T>(&self) -> Result<T> {
        match self.peek() {
            None => err!("42601", "syntax error at end of input"),
            Some(t) => err!("42601", "syntax error at or near \"{}\"", show(t)),
        }
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn is_word_at(&self, n: usize, w: &str) -> bool {
        matches!(self.peek_at(n), Some(Token::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, w: &str) -> Result<()> {
        if self.eat_word(w) {
            Ok(())
        } else {
            match self.peek() {
                None => err!("42601", "syntax error at end of input: expected {}", w.to_uppercase()),
                Some(t) => err!("42601", "syntax error at or near \"{}\": expected {}", show(t), w.to_uppercase()),
            }
        }
    }

    fn eat_sym(&mut self, s: &str) -> bool {
        if matches!(self.peek(), Some(Token::Sym(x)) if *x == s) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn is_sym(&self, s: &str) -> bool {
        matches!(self.peek(), Some(Token::Sym(x)) if *x == s)
    }

    fn expect_sym(&mut self, s: &str) -> Result<()> {
        if self.eat_sym(s) { Ok(()) } else { self.unexpected() }
    }

    fn ident(&mut self) -> Result<String> {
        match self.peek() {
            Some(Token::Word(w)) if !RESERVED.contains(&w.to_ascii_lowercase().as_str()) => {
                let w = w.to_ascii_lowercase();
                self.pos += 1;
                Ok(w)
            }
            Some(Token::Quoted(q)) => {
                let q = q.clone();
                self.pos += 1;
                Ok(q)
            }
            _ => self.unexpected(),
        }
    }

    fn ident_list(&mut self) -> Result<Vec<String>> {
        self.expect_sym("(")?;
        let mut out = vec![self.ident()?];
        self.skip_order_words();
        while self.eat_sym(",") {
            out.push(self.ident()?);
            self.skip_order_words();
        }
        self.expect_sym(")")?;
        Ok(out)
    }

    fn skip_order_words(&mut self) {
        let _ = self.eat_word("asc") || self.eat_word("desc");
    }

    fn statement(&mut self) -> Result<Statement> {
        if self.is_word("select") || self.is_word("values") || self.is_sym("(") {
            return Ok(Statement::Select(Box::new(self.query()?)));
        }
        let Some(Token::Word(w)) = self.peek() else { return self.unexpected() };
        match w.to_ascii_lowercase().as_str() {
            "insert" => self.insert(),
            "update" => self.update(),
            "delete" => self.delete(),
            "create" => self.create(),
            "drop" => self.drop_stmt(),
            "begin" | "start" => {
                self.pos += 1;
                let _ = self.eat_word("transaction") || self.eat_word("work");
                let read_only = if self.eat_word("read") {
                    if self.eat_word("only") {
                        true
                    } else {
                        self.expect_word("write")?;
                        false
                    }
                } else {
                    false
                };
                Ok(Statement::Begin { read_only })
            }
            "commit" | "end" => {
                self.pos += 1;
                let _ = self.eat_word("transaction") || self.eat_word("work");
                Ok(Statement::Commit)
            }
            "rollback" | "abort" => {
                self.pos += 1;
                let _ = self.eat_word("transaction") || self.eat_word("work");
                Ok(Statement::Rollback)
            }
            "explain" => {
                self.pos += 1;
                let (mut analyze, mut json) = (false, false);
                if self.eat_sym("(") {
                    loop {
                        if self.eat_word("analyze") {
                            analyze = !(self.eat_word("false") || self.eat_word("off"));
                            let _ = self.eat_word("true") || self.eat_word("on");
                        } else if self.eat_word("format") {
                            json = self.eat_word("json");
                            if !json {
                                self.expect_word("text")?;
                            }
                        } else if self.eat_word("verbose") || self.eat_word("costs") || self.eat_word("timing") {
                            let _ = self.eat_word("true") || self.eat_word("false") || self.eat_word("on") || self.eat_word("off");
                        } else {
                            return self.unexpected();
                        }
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                    self.expect_sym(")")?;
                } else {
                    analyze = self.eat_word("analyze");
                    self.eat_word("verbose");
                    if self.eat_word("query") {
                        self.expect_word("plan")?; // SQLite's EXPLAIN QUERY PLAN
                    }
                }
                Ok(Statement::Explain { analyze, json, statement: Box::new(self.statement()?) })
            }
            "vacuum" => {
                self.pos += 1;
                Ok(Statement::Vacuum)
            }
            "checkpoint" => {
                self.pos += 1;
                Ok(Statement::Checkpoint)
            }
            "reindex" => {
                self.pos += 1;
                if !matches!(self.peek(), None | Some(Token::Sym(";"))) {
                    self.ident()?;
                }
                Ok(Statement::Reindex)
            }
            "show" => {
                self.pos += 1;
                self.expect_word("tables")?;
                Ok(Statement::ShowTables)
            }
            _ => self.unexpected(),
        }
    }

    fn if_exists(&mut self) -> Result<bool> {
        if self.eat_word("if") {
            self.expect_word("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn if_not_exists(&mut self) -> Result<bool> {
        if self.eat_word("if") {
            self.expect_word("not")?;
            self.expect_word("exists")?;
            return Ok(true);
        }
        Ok(false)
    }

    fn drop_stmt(&mut self) -> Result<Statement> {
        self.expect_word("drop")?;
        if self.eat_word("table") {
            let if_exists = self.if_exists()?;
            Ok(Statement::DropTable { if_exists, name: self.ident()? })
        } else if self.eat_word("index") {
            let if_exists = self.if_exists()?;
            Ok(Statement::DropIndex { if_exists, name: self.ident()? })
        } else if self.eat_word("view") {
            let if_exists = self.if_exists()?;
            Ok(Statement::DropView { if_exists, name: self.ident()? })
        } else {
            self.unexpected()
        }
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_word("create")?;
        let _ = self.eat_word("temp") || self.eat_word("temporary");
        if self.eat_word("table") {
            return self.create_table();
        }
        let unique = self.eat_word("unique");
        if self.eat_word("index") {
            let if_not_exists = self.if_not_exists()?;
            let name = self.ident()?;
            self.expect_word("on")?;
            let table = self.ident()?;
            let columns = self.ident_list()?;
            return Ok(Statement::CreateIndex { name, table, columns, unique, if_not_exists });
        }
        if !unique && self.eat_word("view") {
            let if_not_exists = self.if_not_exists()?;
            let name = self.ident()?;
            self.expect_word("as")?;
            let start = self.tokens.get(self.pos).map_or(self.sql.len(), |t| t.1);
            let query = self.query()?;
            let end = self.tokens.get(self.pos).map_or(self.sql.len(), |t| t.1);
            let sql = self.sql[start..end].trim().to_string();
            return Ok(Statement::CreateView { name, if_not_exists, query: Box::new(query), sql });
        }
        self.unexpected()
    }

    fn data_type(&mut self) -> Result<DataType> {
        let Some(Token::Word(w)) = self.next() else {
            self.pos -= 1;
            return self.unexpected();
        };
        let t = match w.to_ascii_lowercase().as_str() {
            "int" | "integer" | "bigint" | "smallint" | "tinyint" | "int2" | "int4" | "int8" | "mediumint" | "serial"
            | "bigserial" => DataType::Int,
            "real" | "float" | "float4" | "float8" | "numeric" | "decimal" | "number" => DataType::Float,
            "double" => {
                self.eat_word("precision");
                DataType::Float
            }
            "text" | "varchar" | "char" | "string" | "clob" | "nvarchar" | "nchar" => DataType::Text,
            "character" => {
                self.eat_word("varying");
                DataType::Text
            }
            "bool" | "boolean" => DataType::Bool,
            other => return err!("42704", "type \"{other}\" does not exist"),
        };
        if self.eat_sym("(") {
            // Lengths and precisions are accepted and ignored.
            while !self.eat_sym(")") {
                if self.next().is_none() {
                    return self.unexpected();
                }
            }
        }
        Ok(t)
    }

    fn create_table(&mut self) -> Result<Statement> {
        let if_not_exists = self.if_not_exists()?;
        let name = self.ident()?;
        self.expect_sym("(")?;
        let (mut columns, mut primary_key, mut unique) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            if self.eat_word("constraint") {
                self.ident()?;
            }
            if self.eat_word("primary") {
                self.expect_word("key")?;
                primary_key = self.ident_list()?;
            } else if self.eat_word("unique") {
                unique.push(self.ident_list()?);
            } else if self.is_word("foreign") || self.is_word("check") {
                return err!("0A000", "{} constraints are not supported", show(self.peek().unwrap()).to_uppercase());
            } else {
                let cname = self.ident()?;
                let data_type = self.data_type()?;
                let mut col = ColumnDef { name: cname, data_type, nullable: true, primary_key: false, unique: false, default: None };
                loop {
                    if self.eat_word("primary") {
                        self.expect_word("key")?;
                        self.skip_order_words();
                        self.eat_word("autoincrement");
                        col.primary_key = true;
                        col.nullable = false;
                    } else if self.eat_word("not") {
                        self.expect_word("null")?;
                        col.nullable = false;
                    } else if self.eat_word("null") {
                        col.nullable = true;
                    } else if self.eat_word("unique") {
                        col.unique = true;
                    } else if self.eat_word("default") {
                        col.default = Some(self.expr_prec(10)?);
                    } else if self.eat_word("references") {
                        return err!("0A000", "FOREIGN KEY constraints are not supported");
                    } else {
                        break;
                    }
                }
                columns.push(col);
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(Statement::CreateTable { name, if_not_exists, columns, primary_key, unique })
    }

    fn insert(&mut self) -> Result<Statement> {
        self.expect_word("insert")?;
        if self.eat_word("or") {
            return err!("0A000", "INSERT OR ... is not supported");
        }
        self.expect_word("into")?;
        let table = self.ident()?;
        let columns = if self.is_sym("(") && !self.is_word_at(1, "select") { Some(self.ident_list()?) } else { None };
        let source = if self.eat_word("default") {
            self.expect_word("values")?;
            InsertSource::Default
        } else if self.eat_word("values") {
            InsertSource::Values(self.values_rows()?)
        } else {
            InsertSource::Query(Box::new(self.query()?))
        };
        Ok(Statement::Insert { table, columns, source })
    }

    fn values_rows(&mut self) -> Result<Vec<Vec<Expr>>> {
        let mut rows = Vec::new();
        loop {
            self.expect_sym("(")?;
            let mut row = vec![self.expr()?];
            while self.eat_sym(",") {
                row.push(self.expr()?);
            }
            self.expect_sym(")")?;
            rows.push(row);
            if !self.eat_sym(",") {
                return Ok(rows);
            }
        }
    }

    fn alias(&mut self) -> Result<Option<String>> {
        if self.eat_word("as") {
            return Ok(Some(self.ident()?));
        }
        match self.peek() {
            Some(Token::Word(w)) if !RESERVED.contains(&w.to_ascii_lowercase().as_str()) => Ok(Some(self.ident()?)),
            Some(Token::Quoted(_)) => Ok(Some(self.ident()?)),
            _ => Ok(None),
        }
    }

    fn update(&mut self) -> Result<Statement> {
        self.expect_word("update")?;
        let table = self.ident()?;
        let alias = if self.is_word("set") { None } else { self.alias()? };
        self.expect_word("set")?;
        let mut set = Vec::new();
        loop {
            let col = self.ident()?;
            self.expect_sym("=")?;
            set.push((col, self.expr()?));
            if !self.eat_sym(",") {
                break;
            }
        }
        let filter = if self.eat_word("where") { Some(self.expr()?) } else { None };
        Ok(Statement::Update { table, alias, set, filter })
    }

    fn delete(&mut self) -> Result<Statement> {
        self.expect_word("delete")?;
        self.expect_word("from")?;
        let table = self.ident()?;
        let alias = self.alias()?;
        let filter = if self.eat_word("where") { Some(self.expr()?) } else { None };
        Ok(Statement::Delete { table, alias, filter })
    }

    pub fn query(&mut self) -> Result<Query> {
        let body = self.set_expr()?;
        let mut order_by = Vec::new();
        if self.eat_word("order") {
            self.expect_word("by")?;
            loop {
                let expr = self.expr()?;
                let desc = if self.eat_word("desc") {
                    true
                } else {
                    self.eat_word("asc");
                    false
                };
                let nulls_first = if self.eat_word("nulls") {
                    if self.eat_word("first") {
                        Some(true)
                    } else {
                        self.expect_word("last")?;
                        Some(false)
                    }
                } else {
                    None
                };
                order_by.push(OrderItem { expr, desc, nulls_first });
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let (mut limit, mut offset) = (None, None);
        for _ in 0..2 {
            if self.eat_word("limit") {
                limit = if self.eat_word("all") { None } else { Some(self.expr()?) };
                if self.eat_sym(",") {
                    // LIMIT offset, count (MySQL and SQLite)
                    offset = limit.take();
                    limit = Some(self.expr()?);
                }
            } else if self.eat_word("offset") {
                offset = Some(self.expr()?);
                let _ = self.eat_word("rows") || self.eat_word("row");
            }
        }
        Ok(Query { body, order_by, limit, offset })
    }

    fn set_expr(&mut self) -> Result<SetExpr> {
        let mut left = self.set_operand()?;
        loop {
            let op = if self.eat_word("union") {
                SetOp::Union
            } else if self.eat_word("intersect") {
                SetOp::Intersect
            } else if self.eat_word("except") {
                SetOp::Except
            } else {
                return Ok(left);
            };
            let all = self.eat_word("all");
            if !all {
                self.eat_word("distinct");
            }
            let right = self.set_operand()?;
            left = SetExpr::SetOp { op, all, left: Box::new(left), right: Box::new(right) };
        }
    }

    fn set_operand(&mut self) -> Result<SetExpr> {
        if self.eat_sym("(") {
            let q = self.query()?;
            self.expect_sym(")")?;
            return Ok(SetExpr::Query(Box::new(q)));
        }
        if self.eat_word("values") {
            return Ok(SetExpr::Values(self.values_rows()?));
        }
        Ok(SetExpr::Select(Box::new(self.select()?)))
    }

    fn select(&mut self) -> Result<Select> {
        self.expect_word("select")?;
        let distinct = self.eat_word("distinct");
        if !distinct {
            self.eat_word("all");
        }
        let mut items = Vec::new();
        loop {
            if self.eat_sym("*") {
                items.push(SelectItem::Wildcard);
            } else if matches!(self.peek(), Some(Token::Word(_) | Token::Quoted(_)))
                && matches!(self.peek_at(1), Some(Token::Sym(".")))
                && matches!(self.peek_at(2), Some(Token::Sym("*")))
            {
                let t = self.ident()?;
                self.pos += 2;
                items.push(SelectItem::QualifiedWildcard(t));
            } else {
                let e = self.expr()?;
                items.push(SelectItem::Expr(e, self.alias()?));
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        let mut from = Vec::new();
        if self.eat_word("from") {
            loop {
                from.push(self.from_item()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let filter = if self.eat_word("where") { Some(self.expr()?) } else { None };
        let mut group_by = Vec::new();
        if self.eat_word("group") {
            self.expect_word("by")?;
            loop {
                group_by.push(self.expr()?);
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let having = if self.eat_word("having") { Some(self.expr()?) } else { None };
        Ok(Select { distinct, items, from, filter, group_by, having })
    }

    fn table_factor(&mut self) -> Result<FromItem> {
        if self.eat_sym("(") {
            if self.is_word("select") || self.is_word("values") || self.is_sym("(") && self.is_word_at(1, "select") {
                let q = self.query()?;
                self.expect_sym(")")?;
                let alias = self.alias()?;
                return Ok(FromItem::Subquery { query: Box::new(q), alias });
            }
            let item = self.from_item()?;
            self.expect_sym(")")?;
            return Ok(item);
        }
        let name = self.ident()?;
        let alias = self.alias()?;
        Ok(FromItem::Table { name, alias })
    }

    fn from_item(&mut self) -> Result<FromItem> {
        let mut left = self.table_factor()?;
        loop {
            let kind = if self.eat_word("cross") {
                self.expect_word("join")?;
                JoinKind::Cross
            } else if self.eat_word("inner") {
                self.expect_word("join")?;
                JoinKind::Inner
            } else if self.eat_word("join") {
                JoinKind::Inner
            } else if self.eat_word("left") {
                self.eat_word("outer");
                self.expect_word("join")?;
                JoinKind::Left
            } else if self.eat_word("right") {
                self.eat_word("outer");
                self.expect_word("join")?;
                JoinKind::Right
            } else if self.is_word("natural") || self.is_word("full") {
                return err!("0A000", "{} joins are not supported", show(self.peek().unwrap()).to_uppercase());
            } else {
                return Ok(left);
            };
            let right = self.table_factor()?;
            let (mut on, mut using) = (None, Vec::new());
            if kind != JoinKind::Cross {
                if self.eat_word("on") {
                    on = Some(self.expr()?);
                } else if self.eat_word("using") {
                    using = self.ident_list()?;
                } else if kind != JoinKind::Inner {
                    return self.unexpected();
                }
            }
            left = FromItem::Join { left: Box::new(left), right: Box::new(right), kind, on, using };
        }
    }

    pub fn expr(&mut self) -> Result<Expr> {
        self.expr_prec(0)
    }

    fn expr_prec(&mut self, min: u8) -> Result<Expr> {
        let mut left = self.prefix()?;
        loop {
            let negated = self.is_word("not")
                && (self.is_word_at(1, "between") || self.is_word_at(1, "in") || self.is_word_at(1, "like") || self.is_word_at(1, "ilike"));
            let op_word = if negated { 1 } else { 0 };
            let word = |p: &Self, w: &str| p.is_word_at(op_word, w);
            let prec = match self.peek() {
                Some(Token::Word(_)) if self.is_word("or") => 1,
                Some(Token::Word(_)) if self.is_word("and") => 2,
                Some(Token::Word(_)) if self.is_word("is") || self.is_word("isnull") || self.is_word("notnull") => 4,
                Some(Token::Sym("=" | "==" | "<>" | "!=" | "<" | "<=" | ">" | ">=")) => 5,
                Some(Token::Word(_)) if word(self, "between") || word(self, "in") || word(self, "like") || word(self, "ilike") => 6,
                Some(Token::Sym("||")) => 7,
                Some(Token::Sym("+" | "-")) => 8,
                Some(Token::Sym("*" | "/" | "%")) => 9,
                Some(Token::Sym("::")) => 11,
                _ => return Ok(left),
            };
            if prec < min {
                return Ok(left);
            }
            if negated {
                self.pos += 1;
            }
            let tok = self.next().unwrap();
            left = match tok {
                Token::Sym("::") => Expr::Cast(Box::new(left), self.data_type()?),
                Token::Sym(s) => {
                    let op = match s {
                        "=" | "==" => BinOp::Eq,
                        "<>" | "!=" => BinOp::NotEq,
                        "<" => BinOp::Lt,
                        "<=" => BinOp::LtEq,
                        ">" => BinOp::Gt,
                        ">=" => BinOp::GtEq,
                        "||" => BinOp::Concat,
                        "+" => BinOp::Add,
                        "-" => BinOp::Sub,
                        "*" => BinOp::Mul,
                        "/" => BinOp::Div,
                        _ => BinOp::Mod,
                    };
                    Expr::Binary(op, Box::new(left), Box::new(self.expr_prec(prec + 1)?))
                }
                Token::Word(w) => match w.to_ascii_lowercase().as_str() {
                    "or" => Expr::Binary(BinOp::Or, Box::new(left), Box::new(self.expr_prec(prec + 1)?)),
                    "and" => Expr::Binary(BinOp::And, Box::new(left), Box::new(self.expr_prec(prec + 1)?)),
                    "isnull" => Expr::IsNull { expr: Box::new(left), negated: false },
                    "notnull" => Expr::IsNull { expr: Box::new(left), negated: true },
                    "is" => {
                        let not = self.eat_word("not");
                        if self.eat_word("null") {
                            Expr::IsNull { expr: Box::new(left), negated: not }
                        } else {
                            self.eat_word("distinct");
                            if self.eat_word("from") {
                                // IS [NOT] DISTINCT FROM: the null-safe comparisons
                                let op = if not { BinOp::Is } else { BinOp::IsNot };
                                Expr::Binary(op, Box::new(left), Box::new(self.expr_prec(prec + 1)?))
                            } else {
                                let op = if not { BinOp::IsNot } else { BinOp::Is };
                                Expr::Binary(op, Box::new(left), Box::new(self.expr_prec(prec + 1)?))
                            }
                        }
                    }
                    "between" => {
                        let low = self.expr_prec(7)?;
                        self.expect_word("and")?;
                        let high = self.expr_prec(7)?;
                        Expr::Between { expr: Box::new(left), low: Box::new(low), high: Box::new(high), negated }
                    }
                    "in" => {
                        self.expect_sym("(")?;
                        if self.is_word("select") || self.is_word("values") {
                            let q = self.query()?;
                            self.expect_sym(")")?;
                            Expr::InSubquery { expr: Box::new(left), query: Box::new(q), negated }
                        } else {
                            let mut list = Vec::new();
                            if !self.is_sym(")") {
                                list.push(self.expr()?);
                                while self.eat_sym(",") {
                                    list.push(self.expr()?);
                                }
                            }
                            self.expect_sym(")")?;
                            Expr::InList { expr: Box::new(left), list, negated }
                        }
                    }
                    "like" | "ilike" => {
                        let pattern = self.expr_prec(7)?;
                        Expr::Like { expr: Box::new(left), pattern: Box::new(pattern), negated, case_insensitive: w.eq_ignore_ascii_case("ilike") }
                    }
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            };
        }
    }

    fn prefix(&mut self) -> Result<Expr> {
        let Some(tok) = self.next() else { return err!("42601", "syntax error at end of input") };
        Ok(match tok {
            Token::Int(s) => match s.parse::<i64>() {
                Ok(i) => Expr::Literal(Value::Int(i)),
                Err(_) => Expr::Literal(Value::Float(s.parse().unwrap())),
            },
            Token::Float(s) => Expr::Literal(Value::Float(s.parse().map_err(|_| super::types::Error::new("42601", "bad number"))?)),
            Token::Str(s) => Expr::Literal(Value::Text(s)),
            Token::Param(n) => Expr::Param(n),
            Token::Sym("-") => match self.expr_prec(10)? {
                Expr::Literal(Value::Int(i)) => Expr::Literal(Value::Int(-i)),
                Expr::Literal(Value::Float(f)) => Expr::Literal(Value::Float(-f)),
                e => Expr::Unary(UnOp::Neg, Box::new(e)),
            },
            Token::Sym("+") => Expr::Unary(UnOp::Plus, Box::new(self.expr_prec(10)?)),
            Token::Sym("~") => return err!("0A000", "bitwise operators are not supported"),
            Token::Sym("(") => {
                if self.is_word("select") || self.is_word("values") {
                    let q = self.query()?;
                    self.expect_sym(")")?;
                    Expr::Subquery(Box::new(q))
                } else {
                    let e = self.expr()?;
                    self.expect_sym(")")?;
                    e
                }
            }
            Token::Quoted(q) => self.column_or_call(q, true)?,
            Token::Word(w) => match w.to_ascii_lowercase().as_str() {
                "null" => Expr::Literal(Value::Null),
                "true" => Expr::Literal(Value::Bool(true)),
                "false" => Expr::Literal(Value::Bool(false)),
                "not" => Expr::Unary(UnOp::Not, Box::new(self.expr_prec(3)?)),
                "exists" => {
                    self.expect_sym("(")?;
                    let q = self.query()?;
                    self.expect_sym(")")?;
                    Expr::Exists { query: Box::new(q), negated: false }
                }
                "case" => {
                    let operand = if self.is_word("when") { None } else { Some(Box::new(self.expr()?)) };
                    let mut whens = Vec::new();
                    while self.eat_word("when") {
                        let w = self.expr()?;
                        self.expect_word("then")?;
                        whens.push((w, self.expr()?));
                    }
                    if whens.is_empty() {
                        return self.unexpected();
                    }
                    let else_ = if self.eat_word("else") { Some(Box::new(self.expr()?)) } else { None };
                    self.expect_word("end")?;
                    Expr::Case { operand, whens, else_ }
                }
                "cast" => {
                    self.expect_sym("(")?;
                    let e = self.expr()?;
                    self.expect_word("as")?;
                    let t = self.data_type()?;
                    self.expect_sym(")")?;
                    Expr::Cast(Box::new(e), t)
                }
                lw if RESERVED.contains(&lw) && !self.is_sym("(") => {
                    self.pos -= 1;
                    return self.unexpected();
                }
                _ => self.column_or_call(w.to_ascii_lowercase(), false)?,
            },
            _ => {
                self.pos -= 1;
                return self.unexpected();
            }
        })
    }

    fn column_or_call(&mut self, name: String, _quoted: bool) -> Result<Expr> {
        if self.eat_sym("(") {
            let mut args = Vec::new();
            let (mut distinct, mut star) = (false, false);
            if self.eat_sym("*") {
                star = true;
            } else if !self.is_sym(")") {
                distinct = self.eat_word("distinct");
                if !distinct {
                    self.eat_word("all");
                }
                args.push(self.expr()?);
                while self.eat_sym(",") {
                    args.push(self.expr()?);
                }
            }
            self.expect_sym(")")?;
            return Ok(Expr::Function { name: name.to_ascii_lowercase(), args, distinct, star });
        }
        if self.eat_sym(".") {
            let col = self.ident()?;
            return Ok(Expr::Column(Some(name), col));
        }
        Ok(Expr::Column(None, name))
    }
}

fn show(t: &Token) -> String {
    match t {
        Token::Word(w) | Token::Quoted(w) => w.clone(),
        Token::Str(s) => format!("'{s}'"),
        Token::Int(s) | Token::Float(s) => s.clone(),
        Token::Param(n) => format!("${n}"),
        Token::Sym(s) => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(sql: &str) -> Statement {
        let mut v = parse(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(v.len(), 1);
        v.remove(0)
    }

    #[test]
    fn precedence() {
        let e = parse_expr("1 + 2 * 3 = 7 AND NOT a OR b IS NULL").unwrap();
        let Expr::Binary(BinOp::Or, l, r) = e else { panic!() };
        assert!(matches!(*r, Expr::IsNull { negated: false, .. }));
        let Expr::Binary(BinOp::And, cmp, not) = *l else { panic!() };
        assert!(matches!(*not, Expr::Unary(UnOp::Not, _)));
        let Expr::Binary(BinOp::Eq, sum, _) = *cmp else { panic!() };
        let Expr::Binary(BinOp::Add, _, mul) = *sum else { panic!() };
        assert!(matches!(*mul, Expr::Binary(BinOp::Mul, _, _)));
        assert!(matches!(parse_expr("x NOT BETWEEN 1 AND 2 + 3").unwrap(), Expr::Between { negated: true, .. }));
        assert!(matches!(parse_expr("- 5").unwrap(), Expr::Literal(Value::Int(-5))));
        assert!(matches!(parse_expr("a NOT IN (SELECT 1)").unwrap(), Expr::InSubquery { negated: true, .. }));
        assert!(matches!(parse_expr("x::text").unwrap(), Expr::Cast(_, DataType::Text)));
    }

    #[test]
    fn statements() {
        assert!(matches!(
            one("CREATE TABLE t1(a INTEGER PRIMARY KEY, b VARCHAR(30) NOT NULL DEFAULT 'x', c REAL, UNIQUE (b, c))"),
            Statement::CreateTable { ref columns, ref unique, .. } if columns.len() == 3 && columns[0].primary_key && !columns[1].nullable && unique.len() == 1
        ));
        let Statement::Select(q) = one("SELECT DISTINCT t.a AS x, count(*), b c FROM t1 AS t LEFT JOIN t2 ON t.a = t2.a, t3 WHERE a > 1 GROUP BY 1 HAVING count(*) > 2 ORDER BY 2 DESC NULLS LAST LIMIT 5 OFFSET 1")
        else {
            panic!()
        };
        let SetExpr::Select(s) = &q.body else { panic!() };
        assert!(s.distinct && s.items.len() == 3 && s.from.len() == 2 && s.having.is_some());
        assert_eq!(q.order_by[0].nulls_first, Some(false));
        assert!(matches!(one("SELECT 1 UNION ALL SELECT 2 EXCEPT SELECT 3").clone(), Statement::Select(_)));
        assert!(matches!(one("INSERT INTO t VALUES (1, 'a'), (2, NULL)"), Statement::Insert { .. }));
        assert!(matches!(one("INSERT INTO t (a) SELECT a FROM u"), Statement::Insert { columns: Some(_), source: InsertSource::Query(_), .. }));
        assert!(matches!(one("UPDATE t SET a = a + 1, b = 2 WHERE c"), Statement::Update { ref set, .. } if set.len() == 2));
        assert!(matches!(one("EXPLAIN (ANALYZE, FORMAT JSON) SELECT 1"), Statement::Explain { analyze: true, json: true, .. }));
        let Statement::CreateView { sql, .. } = one("CREATE VIEW v AS SELECT a FROM t WHERE a > 1") else { panic!() };
        assert_eq!(sql, "SELECT a FROM t WHERE a > 1");
        assert_eq!(parse("BEGIN; SELECT 1; COMMIT;").unwrap().len(), 3);
        assert!(parse("SELECT FROM").is_err());
        assert!(parse("SELECT 1 2").is_err());
    }
}
