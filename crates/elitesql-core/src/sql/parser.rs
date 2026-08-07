//! Recursive-descent parser for the EliteSQL SQL V1 subset. Everything outside
//! the subset is rejected at parse time with an explicit message, per the
//! design principle: "hacer facil lo comun y explicito lo avanzado".

use crate::error::{Error, Result};
use crate::value::ColumnType;

use super::ast::*;
use super::lexer::{lex, Lexed, Tok};

const MAX_EXPR_DEPTH: u32 = 64;

/// Reserved words that can never be a table alias.
const RESERVED: &[&str] = &[
    "SELECT", "FROM", "WHERE", "JOIN", "INNER", "LEFT", "RIGHT", "FULL", "OUTER", "CROSS", "ON",
    "AND", "OR", "NOT", "ORDER", "BY", "LIMIT", "OFFSET", "GROUP", "HAVING", "UNION", "AS", "IN",
    "IS", "NULL", "VALUES", "INSERT", "INTO", "UPDATE", "SET", "DELETE", "CREATE", "TABLE",
    "INDEX", "UNIQUE", "TRUE", "FALSE", "ASC", "DESC", "EXCEPT", "INTERSECT",
];

pub(crate) fn parse(sql: &str) -> Result<Statement> {
    let toks = lex(sql)?;
    let mut p = Parser { toks, pos: 0 };
    let stmt = p.parse_statement()?;
    // Optional trailing semicolon, then end of input.
    p.eat(&Tok::Semi);
    if let Some(t) = p.peek() {
        if t.tok == Tok::Semi || matches!(&t.tok, Tok::Ident(_)) {
            return Err(p.err_at("only one statement per call is supported"));
        }
        return Err(p.err_at("unexpected trailing input"));
    }
    Ok(stmt)
}

struct Parser {
    toks: Vec<Lexed>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Lexed> {
        self.toks.get(self.pos)
    }

    fn peek2(&self) -> Option<&Lexed> {
        self.toks.get(self.pos + 1)
    }

    fn next(&mut self) -> Option<Lexed> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn err_at(&self, msg: &str) -> Error {
        match self.peek() {
            Some(t) => Error::Sql(format!("{msg} (at byte {})", t.pos)),
            None => Error::Sql(format!("{msg} (at end of input)")),
        }
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek().map(|t| &t.tok) == Some(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok, what: &str) -> Result<()> {
        if self.eat(tok) {
            Ok(())
        } else {
            Err(self.err_at(&format!("expected {what}")))
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Lexed { tok: Tok::Ident(w), .. }) if w.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.err_at(&format!("expected {kw}")))
        }
    }

    fn ident(&mut self, what: &str) -> Result<String> {
        match self.next() {
            Some(Lexed { tok: Tok::Ident(w), .. }) => Ok(w),
            _ => {
                self.pos = self.pos.saturating_sub(1);
                Err(self.err_at(&format!("expected {what}")))
            }
        }
    }

    // --- statements ----------------------------------------------------------

    fn parse_statement(&mut self) -> Result<Statement> {
        if self.eat_kw("SELECT") {
            return self.parse_select();
        }
        if self.eat_kw("INSERT") {
            return self.parse_insert();
        }
        if self.eat_kw("UPDATE") {
            return self.parse_update();
        }
        if self.eat_kw("DELETE") {
            return self.parse_delete();
        }
        if self.eat_kw("CREATE") {
            return self.parse_create();
        }
        for (kw, msg) in [
            ("WITH", "CTEs (WITH) are not supported in V1"),
            ("DROP", "DROP is not supported in V1"),
            ("ALTER", "ALTER is not supported in V1"),
            ("EXPLAIN", "EXPLAIN is not supported in V1"),
            ("BEGIN", "SQL transactions are not supported in V1; use the Txn API"),
            ("COMMIT", "SQL transactions are not supported in V1; use the Txn API"),
            ("ROLLBACK", "SQL transactions are not supported in V1; use the Txn API"),
            ("PRAGMA", "PRAGMA is not supported"),
        ] {
            if self.is_kw(kw) {
                return Err(self.err_at(msg));
            }
        }
        Err(self.err_at("expected SELECT, INSERT, UPDATE, DELETE or CREATE"))
    }

    fn parse_create(&mut self) -> Result<Statement> {
        if self.eat_kw("TABLE") {
            return self.parse_create_table();
        }
        let unique = self.eat_kw("UNIQUE");
        if self.eat_kw("INDEX") {
            return self.parse_create_index(unique);
        }
        if self.is_kw("VIEW") || self.is_kw("MATERIALIZED") {
            return Err(self.err_at("views are not supported in V1"));
        }
        if self.is_kw("TRIGGER") {
            return Err(self.err_at("triggers are not supported in V1"));
        }
        Err(self.err_at("expected TABLE or [UNIQUE] INDEX after CREATE"))
    }

    fn parse_create_table(&mut self) -> Result<Statement> {
        let name = self.ident("table name")?;
        self.expect(&Tok::LParen, "'('")?;
        let mut columns = Vec::new();
        loop {
            if self.is_kw("PRIMARY") || self.is_kw("CONSTRAINT") || self.is_kw("FOREIGN") {
                return Err(self.err_at(
                    "table constraints are not supported in V1; the primary key is the implicit 'id' (text ULID)",
                ));
            }
            let col_name = self.ident("column name")?;
            let (ty, dim) = self.parse_type()?;
            let mut not_null = false;
            loop {
                if self.eat_kw("NOT") {
                    self.expect_kw("NULL")?;
                    not_null = true;
                } else if self.is_kw("PRIMARY") {
                    return Err(self.err_at(
                        "PRIMARY KEY is not supported; the primary key is the implicit 'id' (text ULID)",
                    ));
                } else if self.is_kw("DEFAULT") {
                    return Err(self.err_at("DEFAULT is not supported in V1"));
                } else if self.is_kw("REFERENCES") {
                    return Err(self.err_at("foreign keys are not supported in V1"));
                } else if self.is_kw("UNIQUE") {
                    return Err(self.err_at(
                        "inline UNIQUE is not supported; use CREATE UNIQUE INDEX",
                    ));
                } else {
                    break;
                }
            }
            columns.push(ColumnDef { name: col_name, ty, not_null, dim });
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, "')' or ','")?;
            break;
        }
        Ok(Statement::CreateTable { name, columns })
    }

    fn parse_type(&mut self) -> Result<(ColumnType, Option<usize>)> {
        let word = self.ident("column type")?;
        let ty = match word.to_ascii_lowercase().as_str() {
            "bool" => ColumnType::Bool,
            "int64" => ColumnType::Int64,
            "float64" => ColumnType::Float64,
            "text" => ColumnType::Text,
            "blob" => ColumnType::Blob,
            "timestamp" => ColumnType::Timestamp,
            "json" => ColumnType::Json,
            "date" => ColumnType::Date,
            "time" => ColumnType::Time,
            "vector" => {
                self.expect(&Tok::LParen, "'(' — vector needs a dimension: vector(N)")?;
                let dim = self.parse_uint("vector dimension")? as usize;
                self.expect(&Tok::RParen, "')'")?;
                if dim == 0 {
                    return Err(Error::Sql("vector dimension must be >= 1".into()));
                }
                return Ok((ColumnType::Vector, Some(dim)));
            }
            "int" | "integer" | "bigint" | "smallint" => {
                return Err(Error::Sql(format!("unknown type '{word}': use int64")))
            }
            "real" | "double" | "float" => {
                return Err(Error::Sql(format!("unknown type '{word}': use float64")))
            }
            "varchar" | "char" | "string" | "nvarchar" => {
                return Err(Error::Sql(format!("unknown type '{word}': use text")))
            }
            "datetime" => {
                return Err(Error::Sql(
                    "unknown type 'datetime': use timestamp — it accepts 'YYYY-MM-DD HH:MM:SS' literals".into(),
                ))
            }
            "boolean" => return Err(Error::Sql("unknown type 'boolean': use bool".into())),
            _ => {
                return Err(Error::Sql(format!(
                    "unknown type '{word}': V1 types are bool, int64, float64, text, blob, timestamp, date, time, json, vector(N)"
                )))
            }
        };
        Ok((ty, None))
    }

    fn parse_create_index(&mut self, unique: bool) -> Result<Statement> {
        // Optional index name (accepted for familiarity, derived internally).
        let first = self.ident("index name or ON")?;
        if first.eq_ignore_ascii_case("ON") {
            let table = self.ident("table name")?;
            return self.finish_create_index(table, unique);
        }
        self.expect_kw("ON")?;
        let table = self.ident("table name")?;
        self.finish_create_index(table, unique)
    }

    fn finish_create_index(&mut self, table: String, unique: bool) -> Result<Statement> {
        self.expect(&Tok::LParen, "'('")?;
        let column = self.ident("column name")?;
        if self.eat(&Tok::Comma) {
            return Err(self.err_at("multi-column indexes are not supported in V1"));
        }
        self.expect(&Tok::RParen, "')'")?;
        Ok(Statement::CreateIndex { table, column, unique })
    }

    fn parse_insert(&mut self) -> Result<Statement> {
        self.expect_kw("INTO")?;
        let table = self.ident("table name")?;
        self.expect(&Tok::LParen, "'(' with a column list (required in V1)")?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.ident("column name")?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, "')' or ','")?;
            break;
        }
        if self.is_kw("SELECT") {
            return Err(self.err_at("INSERT ... SELECT is not supported in V1"));
        }
        self.expect_kw("VALUES")?;
        let mut rows = Vec::new();
        loop {
            self.expect(&Tok::LParen, "'('")?;
            let mut row = Vec::new();
            loop {
                row.push(self.parse_literal()?);
                if self.eat(&Tok::Comma) {
                    continue;
                }
                self.expect(&Tok::RParen, "')' or ','")?;
                break;
            }
            if row.len() != columns.len() {
                return Err(Error::Sql(format!(
                    "row has {} values but {} columns were listed",
                    row.len(),
                    columns.len()
                )));
            }
            rows.push(row);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if self.is_kw("RETURNING") {
            return Err(self.err_at(
                "RETURNING is not supported in V1; INSERT already reports the generated ids",
            ));
        }
        Ok(Statement::Insert { table, columns, rows })
    }

    fn parse_update(&mut self) -> Result<Statement> {
        let table = self.ident("table name")?;
        self.expect_kw("SET")?;
        let mut sets = Vec::new();
        loop {
            let col = self.ident("column name")?;
            self.expect(&Tok::Eq, "'='")?;
            let lit = self.parse_literal().map_err(|e| match e {
                Error::Sql(m) => Error::Sql(format!(
                    "{m}; SET only accepts literal values in V1 (no expressions)"
                )),
                other => other,
            })?;
            self.check_no_arith()?;
            sets.push((col, lit));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        let where_clause = self.parse_optional_where()?;
        Ok(Statement::Update { table, sets, where_clause })
    }

    fn parse_delete(&mut self) -> Result<Statement> {
        self.expect_kw("FROM")?;
        let table = self.ident("table name")?;
        let where_clause = self.parse_optional_where()?;
        Ok(Statement::Delete { table, where_clause })
    }

    fn parse_select(&mut self) -> Result<Statement> {
        if self.eat_kw("DISTINCT") {
            return Err(self.err_at("DISTINCT is not supported in V1"));
        }
        let mut projection = Vec::new();
        loop {
            if self.eat(&Tok::Star) {
                projection.push(SelectItem::Star);
            } else if let Some(func) = self.peek_agg_call() {
                self.pos += 1; // the function name
                let (func, arg) = self.parse_agg_call(func)?;
                self.check_no_arith()?;
                let alias = if self.eat_kw("AS") {
                    Some(self.ident("alias")?)
                } else {
                    None
                };
                projection.push(SelectItem::Aggregate { func, arg, alias });
            } else {
                let col = self.parse_column_ref()?;
                if self.peek().map(|t| &t.tok) == Some(&Tok::LParen) {
                    return Err(self.err_at(
                        "functions are not supported in V1 (aggregates: COUNT, SUM, AVG, MIN, MAX)",
                    ));
                }
                self.check_no_arith()?;
                let alias = if self.eat_kw("AS") {
                    Some(self.ident("alias")?)
                } else {
                    None
                };
                projection.push(SelectItem::Column { col, alias });
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect_kw("FROM")?;
        let from = self.parse_table_ref()?;
        let mut joins = Vec::new();
        loop {
            let kind = if self.eat_kw("JOIN") {
                JoinKind::Inner
            } else if self.is_kw("INNER") {
                self.eat_kw("INNER");
                self.expect_kw("JOIN")?;
                JoinKind::Inner
            } else if self.is_kw("LEFT") {
                self.eat_kw("LEFT");
                self.eat_kw("OUTER");
                self.expect_kw("JOIN")?;
                JoinKind::Left
            } else if self.is_kw("RIGHT") {
                self.eat_kw("RIGHT");
                self.eat_kw("OUTER");
                self.expect_kw("JOIN")?;
                JoinKind::Right
            } else if self.is_kw("FULL") {
                return Err(self.err_at("FULL OUTER JOIN is not supported in V1"));
            } else if self.is_kw("CROSS") {
                return Err(self.err_at("CROSS JOIN is not supported in V1"));
            } else {
                break;
            };
            let table = self.parse_table_ref()?;
            self.expect_kw("ON")?;
            let on = self.parse_join_on()?;
            joins.push(Join { kind, table, on });
        }
        let where_clause = self.parse_optional_where()?;
        let mut group_by = Vec::new();
        if self.eat_kw("GROUP") {
            self.expect_kw("BY")?;
            loop {
                group_by.push(self.parse_column_ref()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let having = if self.eat_kw("HAVING") {
            Some(self.parse_expr(0, true)?)
        } else {
            None
        };
        for (kw, msg) in [
            ("UNION", "UNION is not supported in V1"),
            ("EXCEPT", "EXCEPT is not supported in V1"),
            ("INTERSECT", "INTERSECT is not supported in V1"),
        ] {
            if self.is_kw(kw) {
                return Err(self.err_at(msg));
            }
        }
        let mut order_by = Vec::new();
        if self.eat_kw("ORDER") {
            self.expect_kw("BY")?;
            loop {
                if self.peek_agg_call().is_some() {
                    return Err(self.err_at(
                        "ORDER BY cannot use an aggregate call; give it an alias in SELECT and order by the alias",
                    ));
                }
                let col = self.parse_column_ref()?;
                let desc = if self.eat_kw("DESC") {
                    true
                } else {
                    self.eat_kw("ASC");
                    false
                };
                order_by.push((col, desc));
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
        }
        let mut limit = None;
        let mut offset = None;
        if self.eat_kw("LIMIT") {
            limit = Some(self.parse_uint("LIMIT")?);
            if self.eat_kw("OFFSET") {
                offset = Some(self.parse_uint("OFFSET")?);
            }
        }
        Ok(Statement::Select(Box::new(SelectStmt {
            projection,
            from,
            joins,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
            offset,
        })))
    }

    /// Detects `count(`/`sum(`/`avg(`/`min(`/`max(` at the cursor without
    /// consuming anything.
    fn peek_agg_call(&self) -> Option<AggFunc> {
        let Some(Lexed { tok: Tok::Ident(w), .. }) = self.peek() else {
            return None;
        };
        if self.peek2().map(|t| &t.tok) != Some(&Tok::LParen) {
            return None;
        }
        match w.to_ascii_lowercase().as_str() {
            "count" => Some(AggFunc::Count),
            "sum" => Some(AggFunc::Sum),
            "avg" => Some(AggFunc::Avg),
            "min" => Some(AggFunc::Min),
            "max" => Some(AggFunc::Max),
            _ => None,
        }
    }

    /// Parses the parenthesized argument of an aggregate call; the function
    /// name has already been consumed.
    fn parse_agg_call(&mut self, func: AggFunc) -> Result<(AggFunc, Option<ColumnRef>)> {
        self.expect(&Tok::LParen, "'('")?;
        if self.eat(&Tok::Star) {
            if func != AggFunc::Count {
                return Err(self.err_at("only COUNT accepts *"));
            }
            self.expect(&Tok::RParen, "')'")?;
            return Ok((func, None));
        }
        if self.is_kw("DISTINCT") {
            return Err(self.err_at("DISTINCT inside aggregates is not supported in V1"));
        }
        let col = self.parse_column_ref()?;
        self.expect(&Tok::RParen, "')'")?;
        Ok((func, Some(col)))
    }

    fn parse_uint(&mut self, what: &str) -> Result<u64> {
        match self.next() {
            Some(Lexed { tok: Tok::Int(n), .. }) if n >= 0 => Ok(n as u64),
            _ => {
                self.pos = self.pos.saturating_sub(1);
                Err(self.err_at(&format!("{what} expects a non-negative integer")))
            }
        }
    }

    fn parse_table_ref(&mut self) -> Result<TableRef> {
        if self.peek().map(|t| &t.tok) == Some(&Tok::LParen) {
            return Err(self.err_at("subqueries are not supported in V1"));
        }
        let name = self.ident("table name")?;
        let alias = if self.eat_kw("AS") {
            Some(self.ident("alias")?)
        } else if let Some(Lexed { tok: Tok::Ident(w), .. }) = self.peek() {
            if RESERVED.iter().any(|r| w.eq_ignore_ascii_case(r)) {
                None
            } else {
                let w = w.clone();
                self.pos += 1;
                Some(w)
            }
        } else {
            None
        };
        Ok(TableRef { name, alias })
    }

    fn parse_join_on(&mut self) -> Result<(ColumnRef, ColumnRef)> {
        let expr = self.parse_expr(0, false)?;
        match expr {
            Expr::Cmp {
                left: Operand::Col(l),
                op: CmpOp::Eq,
                right: Operand::Col(r),
            } => Ok((l, r)),
            _ => Err(Error::Sql(
                "ON only supports a single column equality (a.x = b.y) in V1; put extra filters in WHERE"
                    .into(),
            )),
        }
    }

    fn parse_optional_where(&mut self) -> Result<Option<Expr>> {
        if self.eat_kw("WHERE") {
            Ok(Some(self.parse_expr(0, false)?))
        } else {
            Ok(None)
        }
    }

    // --- expressions -----------------------------------------------------------
    // `allow_agg` is true only inside HAVING: aggregate calls anywhere else
    // are rejected with a pointed message.

    fn parse_expr(&mut self, depth: u32, allow_agg: bool) -> Result<Expr> {
        self.guard(depth)?;
        let mut left = self.parse_and(depth + 1, allow_agg)?;
        while self.eat_kw("OR") {
            let right = self.parse_and(depth + 1, allow_agg)?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self, depth: u32, allow_agg: bool) -> Result<Expr> {
        self.guard(depth)?;
        let mut left = self.parse_not(depth + 1, allow_agg)?;
        while self.eat_kw("AND") {
            let right = self.parse_not(depth + 1, allow_agg)?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_not(&mut self, depth: u32, allow_agg: bool) -> Result<Expr> {
        self.guard(depth)?;
        if self.eat_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.parse_not(depth + 1, allow_agg)?)));
        }
        self.parse_predicate(depth + 1, allow_agg)
    }

    fn parse_predicate(&mut self, depth: u32, allow_agg: bool) -> Result<Expr> {
        self.guard(depth)?;
        if self.peek().map(|t| &t.tok) == Some(&Tok::LParen) {
            if let Some(Lexed { tok: Tok::Ident(w), .. }) = self.peek2() {
                if w.eq_ignore_ascii_case("SELECT") {
                    return Err(self.err_at("subqueries are not supported in V1"));
                }
            }
            self.pos += 1;
            let inner = self.parse_expr(depth + 1, allow_agg)?;
            self.expect(&Tok::RParen, "')'")?;
            return Ok(inner);
        }
        let left = self.parse_operand(allow_agg)?;
        self.check_no_arith()?;

        // IS [NOT] NULL and [NOT] IN need a column on the left.
        if self.eat_kw("IS") {
            let negated = self.eat_kw("NOT");
            self.expect_kw("NULL")?;
            match left {
                Operand::Col(col) => return Ok(Expr::IsNull { col, negated }),
                _ => {
                    return Err(self.err_at("IS NULL applies to a column"));
                }
            }
        }
        let negated_in = if self.is_kw("NOT") {
            // lookahead: NOT IN
            if let Some(Lexed { tok: Tok::Ident(w), .. }) = self.peek2() {
                if w.eq_ignore_ascii_case("IN") {
                    self.pos += 2;
                    true
                } else {
                    return Err(self.err_at("unexpected NOT"));
                }
            } else {
                return Err(self.err_at("unexpected NOT"));
            }
        } else if self.eat_kw("IN") {
            false
        } else {
            // plain comparison
            let op = match self.next().map(|t| t.tok) {
                Some(Tok::Eq) => CmpOp::Eq,
                Some(Tok::Neq) => CmpOp::Neq,
                Some(Tok::Lt) => CmpOp::Lt,
                Some(Tok::Le) => CmpOp::Le,
                Some(Tok::Gt) => CmpOp::Gt,
                Some(Tok::Ge) => CmpOp::Ge,
                _ => {
                    self.pos = self.pos.saturating_sub(1);
                    if self.is_kw("LIKE") {
                        return Err(self.err_at("LIKE is not supported in V1"));
                    }
                    if self.is_kw("BETWEEN") {
                        return Err(self.err_at("BETWEEN is not supported in V1; use >= AND <="));
                    }
                    return Err(self.err_at("expected a comparison operator"));
                }
            };
            let right = self.parse_operand(allow_agg)?;
            self.check_no_arith()?;
            return Ok(Expr::Cmp { left, op, right });
        };
        // IN list
        let col = match left {
            Operand::Col(c) => c,
            _ => return Err(self.err_at("IN applies to a column")),
        };
        self.expect(&Tok::LParen, "'('")?;
        if self.is_kw("SELECT") {
            return Err(self.err_at("subqueries are not supported in V1"));
        }
        let mut list = Vec::new();
        loop {
            list.push(self.parse_literal()?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            self.expect(&Tok::RParen, "')' or ','")?;
            break;
        }
        Ok(Expr::InList { col, list, negated: negated_in })
    }

    fn parse_operand(&mut self, allow_agg: bool) -> Result<Operand> {
        if let Some(func) = self.peek_agg_call() {
            if !allow_agg {
                return Err(self.err_at(
                    "aggregates are only allowed in the SELECT list and HAVING",
                ));
            }
            self.pos += 1;
            let (func, arg) = self.parse_agg_call(func)?;
            return Ok(Operand::Agg { func, arg });
        }
        match self.peek().map(|t| t.tok.clone()) {
            Some(Tok::Ident(w)) => {
                if w.eq_ignore_ascii_case("TRUE") {
                    self.pos += 1;
                    Ok(Operand::Lit(Literal::Bool(true)))
                } else if w.eq_ignore_ascii_case("FALSE") {
                    self.pos += 1;
                    Ok(Operand::Lit(Literal::Bool(false)))
                } else if w.eq_ignore_ascii_case("NULL") {
                    self.pos += 1;
                    Ok(Operand::Lit(Literal::Null))
                } else {
                    let col = self.parse_column_ref()?;
                    if self.peek().map(|t| &t.tok) == Some(&Tok::LParen) {
                        return Err(self.err_at("functions are not supported in V1"));
                    }
                    Ok(Operand::Col(col))
                }
            }
            _ => Ok(Operand::Lit(self.parse_literal()?)),
        }
    }

    fn parse_column_ref(&mut self) -> Result<ColumnRef> {
        let first = self.ident("column name")?;
        if self.peek().map(|t| &t.tok) == Some(&Tok::Dot) {
            self.pos += 1;
            if self.peek().map(|t| &t.tok) == Some(&Tok::Star) {
                return Err(self.err_at("qualified star (t.*) is not supported in V1"));
            }
            let column = self.ident("column name after '.'")?;
            Ok(ColumnRef { table: Some(first), column })
        } else {
            Ok(ColumnRef { table: None, column: first })
        }
    }

    fn parse_literal(&mut self) -> Result<Literal> {
        match self.next() {
            Some(Lexed { tok: Tok::Int(n), .. }) => Ok(Literal::Int(n)),
            Some(Lexed { tok: Tok::Float(f), .. }) => Ok(Literal::Float(f)),
            Some(Lexed { tok: Tok::Str(s), .. }) => Ok(Literal::Str(s)),
            Some(Lexed { tok: Tok::Blob(b), .. }) => Ok(Literal::Blob(b)),
            Some(Lexed { tok: Tok::Minus, .. }) => match self.next() {
                Some(Lexed { tok: Tok::Int(n), .. }) => Ok(Literal::Int(-n)),
                Some(Lexed { tok: Tok::Float(f), .. }) => Ok(Literal::Float(-f)),
                _ => {
                    self.pos = self.pos.saturating_sub(1);
                    Err(self.err_at("expected a number after '-'"))
                }
            },
            Some(Lexed { tok: Tok::Ident(w), .. }) => {
                if w.eq_ignore_ascii_case("TRUE") {
                    Ok(Literal::Bool(true))
                } else if w.eq_ignore_ascii_case("FALSE") {
                    Ok(Literal::Bool(false))
                } else if w.eq_ignore_ascii_case("NULL") {
                    Ok(Literal::Null)
                } else {
                    self.pos -= 1;
                    Err(self.err_at("expected a literal value"))
                }
            }
            _ => {
                self.pos = self.pos.saturating_sub(1);
                Err(self.err_at("expected a literal value"))
            }
        }
    }

    fn check_no_arith(&self) -> Result<()> {
        match self.peek().map(|t| &t.tok) {
            Some(Tok::Plus) | Some(Tok::Minus) | Some(Tok::Slash) | Some(Tok::Percent)
            | Some(Tok::Star) => Err(self.err_at(
                "arithmetic expressions are not supported in V1; compute in the application",
            )),
            _ => Ok(()),
        }
    }

    fn guard(&self, depth: u32) -> Result<()> {
        if depth > MAX_EXPR_DEPTH {
            Err(Error::Sql("expression too deeply nested".into()))
        } else {
            Ok(())
        }
    }
}
