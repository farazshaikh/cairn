//! Recursive-descent parser for statements; expressions are in `expr.rs`.

use crate::MAX_DEPTH;
use crate::ast::{
    Assignment, ColumnDef, CreateIndex, CreateTable, DataType, Delete, DropTable, Expr, FromClause,
    Ident, Insert, Join, JoinKind, Limit, Name, OrderItem, Row, Select, SelectItem, Spanned,
    Statement, StatementKind, TableRef, Update,
};
use crate::error::SqlError;
use crate::lexer::tokenize;
use crate::span::Span;
use crate::token::{Keyword, Token, TokenKind};

/// Longest token text quoted after "found" in an error message.
const FOUND_TEXT_LIMIT: usize = 40;

/// Errors are boxed inside the parser so that results stay small; the
/// recursive expression functions then keep small stack frames.
pub(crate) type ParseResult<T> = Result<T, Box<SqlError>>;

/// What a recursive descent into a sub-expression adds to the limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Descent {
    /// A grouping parenthesis: nesting only, it builds no node.
    Group,
    /// An operand of a binary operator or predicate: one more open node.
    Node,
    /// A prefix operator, CASE, or the parenthesis of a call or IN list:
    /// both nesting and an open node.
    NestedNode,
}

pub(crate) struct Parser<'a> {
    src: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    /// Returned by `peek` past the end; `tokenize` already ends with `Eof`.
    eof: Token,
    /// Span of the most recently consumed token.
    last: Span,
    /// Nesting constructs open around the current expression (HLD D10a).
    depth: usize,
    /// Expression nodes still being built around the current position. Each
    /// becomes an ancestor of what is parsed next, so this never exceeds the
    /// final tree height (HLD D10b) and rejects over-tall input early.
    open_nodes: usize,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(src: &'a str) -> ParseResult<Parser<'a>> {
        let tokens = tokenize(src)?;
        let start = Span {
            start: 0,
            end: 0,
            line: 1,
            column: 1,
        };
        let eof = tokens.last().cloned().unwrap_or(Token {
            kind: TokenKind::Eof,
            span: start,
        });
        Ok(Parser {
            src,
            tokens,
            pos: 0,
            eof,
            last: start,
            depth: 0,
            open_nodes: 0,
        })
    }

    /// `statement (';' statement)* [';']` up to the end of input.
    pub(crate) fn statements(mut self) -> ParseResult<Vec<Statement>> {
        let mut statements = vec![self.statement()?];
        loop {
            if self.at_eof() {
                return Ok(statements);
            }
            if self.eat(&TokenKind::Semicolon).is_none() {
                return Err(self.expected("';' or end of input", "after statement"));
            }
            if self.at_eof() {
                return Ok(statements);
            }
            statements.push(self.statement()?);
        }
    }

    /// Exactly one expression followed by the end of input.
    pub(crate) fn single_expr(mut self) -> ParseResult<Expr> {
        let expr = self.expr()?;
        if !self.at_eof() {
            return Err(self.expected("end of input", "after expression"));
        }
        Ok(expr)
    }

    fn statement(&mut self) -> ParseResult<Statement> {
        let start = self.peek().span;
        let kind = match self.peek_keyword() {
            Some(Keyword::Create) => self.create()?,
            Some(Keyword::Drop) => StatementKind::DropTable(self.drop_table()?),
            Some(Keyword::Insert) => StatementKind::Insert(self.insert()?),
            Some(Keyword::Update) => StatementKind::Update(self.update()?),
            Some(Keyword::Delete) => StatementKind::Delete(self.delete()?),
            Some(Keyword::Select) => StatementKind::Select(Box::new(self.select()?)),
            Some(Keyword::Begin) => self.single_word(StatementKind::Begin),
            Some(Keyword::Commit) => self.single_word(StatementKind::Commit),
            Some(Keyword::Rollback) => self.single_word(StatementKind::Rollback),
            _ => return Err(self.expected("statement", "")),
        };
        Ok(Spanned::new(kind, start.until(self.last)))
    }

    fn single_word(&mut self, statement: StatementKind) -> StatementKind {
        self.bump();
        statement
    }

    fn create(&mut self) -> ParseResult<StatementKind> {
        self.bump();
        if self.eat_keyword(Keyword::Table).is_some() {
            return Ok(StatementKind::CreateTable(self.create_table()?));
        }
        if self.eat_keyword(Keyword::Unique).is_some() {
            self.expect_keyword(Keyword::Index, "after UNIQUE")?;
            return Ok(StatementKind::CreateIndex(self.create_index(true)?));
        }
        if self.eat_keyword(Keyword::Index).is_some() {
            return Ok(StatementKind::CreateIndex(self.create_index(false)?));
        }
        Err(self.expected("TABLE, INDEX or UNIQUE", "after CREATE"))
    }

    fn create_table(&mut self) -> ParseResult<CreateTable> {
        let if_not_exists = self.eat_keyword(Keyword::If).is_some();
        if if_not_exists {
            self.expect_keyword(Keyword::Not, "after IF")?;
            self.expect_keyword(Keyword::Exists, "after IF NOT")?;
        }
        let context = if if_not_exists {
            "after EXISTS"
        } else {
            "after TABLE"
        };
        let name = self.expect_ident("table name", context)?;
        self.expect(&TokenKind::LParen, "'('", "after table name")?;
        let mut columns = vec![self.column_def()?];
        while self.eat(&TokenKind::Comma).is_some() {
            columns.push(self.column_def()?);
        }
        self.expect(&TokenKind::RParen, "')'", "after column list")?;
        Ok(CreateTable {
            if_not_exists,
            name,
            columns,
        })
    }

    /// A column name, its type, then constraints in any order, each at most once.
    fn column_def(&mut self) -> ParseResult<Spanned<ColumnDef>> {
        let name = self.expect_ident("column name", "")?;
        let data_type = self.data_type()?;
        let mut def = ColumnDef {
            name,
            data_type,
            primary_key: false,
            not_null: false,
            unique: false,
        };
        while let Some(constraint) = self.constraint()? {
            let flag = match constraint.node {
                Constraint::PrimaryKey => &mut def.primary_key,
                Constraint::NotNull => &mut def.not_null,
                Constraint::Unique => &mut def.unique,
            };
            if *flag {
                let message = format!("duplicate constraint {}", constraint.node.name());
                return Err(self.error(message, constraint.span));
            }
            *flag = true;
        }
        let span = def.name.span.until(self.last);
        Ok(Spanned::new(def, span))
    }

    /// Type names are identifiers, not reserved words, so `text` stays usable as a column name.
    fn data_type(&mut self) -> ParseResult<Spanned<DataType>> {
        let data_type = match &self.peek().kind {
            TokenKind::Ident(name) => match name.as_str() {
                "integer" => Some(DataType::Integer),
                "real" => Some(DataType::Real),
                "text" => Some(DataType::Text),
                "boolean" => Some(DataType::Boolean),
                _ => None,
            },
            _ => None,
        };
        let Some(data_type) = data_type else {
            return Err(self.expected("column type INTEGER, REAL, TEXT or BOOLEAN", ""));
        };
        let span = self.bump();
        Ok(Spanned::new(data_type, span))
    }

    fn constraint(&mut self) -> ParseResult<Option<Spanned<Constraint>>> {
        let start = self.peek().span;
        let constraint = match self.peek_keyword() {
            Some(Keyword::Primary) => {
                self.bump();
                self.expect_keyword(Keyword::Key, "after PRIMARY")?;
                Constraint::PrimaryKey
            }
            Some(Keyword::Not) => {
                self.bump();
                self.expect_keyword(Keyword::Null, "after NOT")?;
                Constraint::NotNull
            }
            Some(Keyword::Unique) => {
                self.bump();
                Constraint::Unique
            }
            _ => return Ok(None),
        };
        Ok(Some(Spanned::new(constraint, start.until(self.last))))
    }

    fn create_index(&mut self, unique: bool) -> ParseResult<CreateIndex> {
        let name = self.expect_ident("index name", "after INDEX")?;
        self.expect_keyword(Keyword::On, "after index name")?;
        let table = self.expect_ident("table name", "after ON")?;
        self.expect(&TokenKind::LParen, "'('", "after table name")?;
        let column = self.expect_ident("column name", "after '('")?;
        self.expect(&TokenKind::RParen, "')'", "after index column")?;
        Ok(CreateIndex {
            unique,
            name,
            table,
            column,
        })
    }

    fn drop_table(&mut self) -> ParseResult<DropTable> {
        self.bump();
        self.expect_keyword(Keyword::Table, "after DROP")?;
        let if_exists = self.eat_keyword(Keyword::If).is_some();
        if if_exists {
            self.expect_keyword(Keyword::Exists, "after IF")?;
        }
        let context = if if_exists {
            "after EXISTS"
        } else {
            "after TABLE"
        };
        let name = self.expect_ident("table name", context)?;
        Ok(DropTable { if_exists, name })
    }

    fn insert(&mut self) -> ParseResult<Insert> {
        self.bump();
        self.expect_keyword(Keyword::Into, "after INSERT")?;
        let table = self.expect_ident("table name", "after INTO")?;
        let columns = match self.eat(&TokenKind::LParen) {
            Some(_) => Some(self.column_list()?),
            None => None,
        };
        let context = if columns.is_some() {
            "after column list"
        } else {
            "after table name"
        };
        self.expect_keyword(Keyword::Values, context)?;
        let mut rows = vec![self.row("after VALUES")?];
        while self.eat(&TokenKind::Comma).is_some() {
            rows.push(self.row("after ','")?);
        }
        Ok(Insert {
            table,
            columns,
            rows,
        })
    }

    /// The names after an already consumed `(`, through the closing `)`.
    fn column_list(&mut self) -> ParseResult<Vec<Ident>> {
        let mut columns = vec![self.expect_ident("column name", "")?];
        while self.eat(&TokenKind::Comma).is_some() {
            columns.push(self.expect_ident("column name", "")?);
        }
        self.expect(&TokenKind::RParen, "')'", "after column list")?;
        Ok(columns)
    }

    fn row(&mut self, context: &str) -> ParseResult<Spanned<Row>> {
        let open = self.expect(&TokenKind::LParen, "'('", context)?;
        let values = self.expr_list()?;
        let close = self.expect(&TokenKind::RParen, "')'", "after row values")?;
        Ok(Spanned::new(Row(values), open.until(close)))
    }

    fn update(&mut self) -> ParseResult<Update> {
        self.bump();
        let table = self.expect_ident("table name", "after UPDATE")?;
        self.expect_keyword(Keyword::Set, "after table name")?;
        let mut assignments = vec![self.assignment("after SET")?];
        while self.eat(&TokenKind::Comma).is_some() {
            assignments.push(self.assignment("after ','")?);
        }
        let where_clause = self.where_clause()?;
        Ok(Update {
            table,
            assignments,
            where_clause,
        })
    }

    fn assignment(&mut self, context: &str) -> ParseResult<Spanned<Assignment>> {
        let column = self.expect_ident("column name", context)?;
        self.expect(&TokenKind::Eq, "'='", "after column name")?;
        let value = self.expr()?;
        let span = column.span.until(value.span);
        Ok(Spanned::new(Assignment { column, value }, span))
    }

    fn delete(&mut self) -> ParseResult<Delete> {
        self.bump();
        self.expect_keyword(Keyword::From, "after DELETE")?;
        let table = self.expect_ident("table name", "after FROM")?;
        let where_clause = self.where_clause()?;
        Ok(Delete {
            table,
            where_clause,
        })
    }

    fn where_clause(&mut self) -> ParseResult<Option<Expr>> {
        match self.eat_keyword(Keyword::Where) {
            Some(_) => Ok(Some(self.expr()?)),
            None => Ok(None),
        }
    }

    /// Every clause is optional and the clause order is fixed.
    fn select(&mut self) -> ParseResult<Select> {
        self.bump();
        let distinct = self.eat_keyword(Keyword::Distinct).is_some();
        let mut items = vec![self.select_item()?];
        while self.eat(&TokenKind::Comma).is_some() {
            items.push(self.select_item()?);
        }
        let from = match self.eat_keyword(Keyword::From) {
            Some(_) => Some(self.select_from()?),
            None => None,
        };
        let where_clause = self.where_clause()?;
        let mut group_by = Vec::new();
        if self.eat_keyword(Keyword::Group).is_some() {
            self.expect_keyword(Keyword::By, "after GROUP")?;
            group_by = self.expr_list()?;
        }
        let having = match self.eat_keyword(Keyword::Having) {
            Some(_) => Some(self.expr()?),
            None => None,
        };
        let mut order_by = Vec::new();
        if self.eat_keyword(Keyword::Order).is_some() {
            self.expect_keyword(Keyword::By, "after ORDER")?;
            order_by.push(self.order_item()?);
            while self.eat(&TokenKind::Comma).is_some() {
                order_by.push(self.order_item()?);
            }
        }
        let limit = match self.eat_keyword(Keyword::Limit) {
            Some(_) => Some(self.limit()?),
            None => None,
        };
        Ok(Select {
            distinct,
            items,
            from,
            where_clause,
            group_by,
            having,
            order_by,
            limit,
        })
    }

    fn select_item(&mut self) -> ParseResult<Spanned<SelectItem>> {
        let start = self.peek().span;
        if self.eat(&TokenKind::Star).is_some() {
            return Ok(Spanned::new(SelectItem::Wildcard, start));
        }
        if self.at_qualified_wildcard() {
            let table = self.expect_ident("table name", "")?;
            self.bump();
            let star = self.bump();
            return Ok(Spanned::new(
                SelectItem::QualifiedWildcard(table),
                start.until(star),
            ));
        }
        let expr = self.expr()?;
        let alias = self.alias()?;
        let item = SelectItem::Expr { expr, alias };
        Ok(Spanned::new(item, start.until(self.last)))
    }

    fn at_qualified_wildcard(&self) -> bool {
        matches!(self.peek_nth(0).kind, TokenKind::Ident(_))
            && self.peek_nth(1).kind == TokenKind::Dot
            && self.peek_nth(2).kind == TokenKind::Star
    }

    fn alias(&mut self) -> ParseResult<Option<Ident>> {
        match self.eat_keyword(Keyword::As) {
            Some(_) => Ok(Some(self.expect_ident("alias", "after AS")?)),
            None => Ok(None),
        }
    }

    fn select_from(&mut self) -> ParseResult<FromClause> {
        let table = self.table_ref("after FROM")?;
        let mut joins = Vec::new();
        while let Some(join) = self.join()? {
            joins.push(join);
        }
        Ok(FromClause { table, joins })
    }

    fn table_ref(&mut self, context: &str) -> ParseResult<Spanned<TableRef>> {
        let name = self.expect_ident("table name", context)?;
        let alias = self.alias()?;
        let span = name.span.until(self.last);
        Ok(Spanned::new(TableRef { name, alias }, span))
    }

    fn join(&mut self) -> ParseResult<Option<Spanned<Join>>> {
        let start = self.peek().span;
        let (kind, context) = match self.peek_keyword() {
            Some(Keyword::Join) => (JoinKind::Inner, ""),
            Some(Keyword::Inner) => {
                self.bump();
                (JoinKind::Inner, "after INNER")
            }
            Some(Keyword::Left) => {
                self.bump();
                (JoinKind::Left, "after LEFT")
            }
            _ => return Ok(None),
        };
        self.expect_keyword(Keyword::Join, context)?;
        let table = self.table_ref("after JOIN")?;
        self.expect_keyword(Keyword::On, "after join table")?;
        let on = self.expr()?;
        let span = start.until(on.span);
        Ok(Some(Spanned::new(Join { kind, table, on }, span)))
    }

    fn order_item(&mut self) -> ParseResult<Spanned<OrderItem>> {
        let expr = self.expr()?;
        let descending = self.eat_keyword(Keyword::Desc).is_some();
        if !descending {
            self.eat_keyword(Keyword::Asc);
        }
        let span = expr.span.until(self.last);
        Ok(Spanned::new(OrderItem { expr, descending }, span))
    }

    fn limit(&mut self) -> ParseResult<Limit> {
        let count = self.integer("after LIMIT")?;
        let offset = match self.eat_keyword(Keyword::Offset) {
            Some(_) => Some(self.integer("after OFFSET")?),
            None => None,
        };
        Ok(Limit { count, offset })
    }

    fn integer(&mut self, context: &str) -> ParseResult<Spanned<i64>> {
        let TokenKind::Integer(value) = self.peek().kind else {
            return Err(self.expected("integer", context));
        };
        let span = self.bump();
        Ok(Spanned::new(value, span))
    }

    fn expr_list(&mut self) -> ParseResult<Vec<Expr>> {
        let mut exprs = vec![self.expr()?];
        while self.eat(&TokenKind::Comma).is_some() {
            exprs.push(self.expr()?);
        }
        Ok(exprs)
    }

    pub(crate) fn peek(&self) -> &Token {
        self.peek_nth(0)
    }

    fn peek_nth(&self, n: usize) -> &Token {
        self.tokens.get(self.pos + n).unwrap_or(&self.eof)
    }

    pub(crate) fn peek_keyword(&self) -> Option<Keyword> {
        match self.peek().kind {
            TokenKind::Keyword(keyword) => Some(keyword),
            _ => None,
        }
    }

    pub(crate) fn at(&self, kind: &TokenKind) -> bool {
        self.peek().kind == *kind
    }

    fn at_eof(&self) -> bool {
        self.at(&TokenKind::Eof)
    }

    /// Consumes the current token, never moving past `Eof`, and returns its span.
    pub(crate) fn bump(&mut self) -> Span {
        let span = self.peek().span;
        if !self.at_eof() {
            self.pos += 1;
        }
        self.last = span;
        span
    }

    /// Span of the most recently consumed token.
    pub(crate) fn last(&self) -> Span {
        self.last
    }

    pub(crate) fn eat(&mut self, kind: &TokenKind) -> Option<Span> {
        if !self.at(kind) {
            return None;
        }
        Some(self.bump())
    }

    pub(crate) fn eat_keyword(&mut self, keyword: Keyword) -> Option<Span> {
        self.eat(&TokenKind::Keyword(keyword))
    }

    pub(crate) fn expect(
        &mut self,
        kind: &TokenKind,
        what: &str,
        context: &str,
    ) -> ParseResult<Span> {
        match self.eat(kind) {
            Some(span) => Ok(span),
            None => Err(self.expected(what, context)),
        }
    }

    pub(crate) fn expect_keyword(&mut self, keyword: Keyword, context: &str) -> ParseResult<Span> {
        self.expect(&TokenKind::Keyword(keyword), keyword.as_str(), context)
    }

    pub(crate) fn expect_ident(&mut self, what: &str, context: &str) -> ParseResult<Ident> {
        let TokenKind::Ident(name) = &self.peek().kind else {
            return Err(self.expected(what, context));
        };
        let name = Name(name.clone());
        let span = self.bump();
        Ok(Spanned::new(name, span))
    }

    /// Starts parsing a sub-expression opened by the token at `at`, failing
    /// once either limit would exceed [`MAX_DEPTH`].
    pub(crate) fn enter(&mut self, descent: Descent, at: Span) -> ParseResult<()> {
        if descent != Descent::Node {
            self.depth += 1;
        }
        if descent != Descent::Group {
            self.open_nodes += 1;
        }
        if self.depth > MAX_DEPTH || self.open_nodes > MAX_DEPTH {
            return Err(self.depth_error(at));
        }
        Ok(())
    }

    pub(crate) fn leave(&mut self, descent: Descent) {
        if descent != Descent::Node {
            self.depth = self.depth.saturating_sub(1);
        }
        if descent != Descent::Group {
            self.open_nodes = self.open_nodes.saturating_sub(1);
        }
    }

    pub(crate) fn depth_error(&self, at: Span) -> Box<SqlError> {
        self.error(
            format!("expression nesting exceeds the limit of {MAX_DEPTH}"),
            at,
        )
    }

    /// Builds `expected <what>[ <context>], found <current token>` at the current token.
    pub(crate) fn expected(&self, what: &str, context: &str) -> Box<SqlError> {
        let token = self.peek();
        let mut message = format!("expected {what}");
        if !context.is_empty() {
            message.push(' ');
            message.push_str(context);
        }
        message.push_str(", found ");
        message.push_str(&self.describe(token));
        self.error(message, token.span)
    }

    /// The token's source text in quotes, cut to its first line and 40 characters.
    fn describe(&self, token: &Token) -> String {
        if token.kind == TokenKind::Eof {
            return "end of input".to_string();
        }
        let text = self.src.get(token.span.start..token.span.end).unwrap_or("");
        let first_line = text.split('\n').next().unwrap_or("");
        let shown: String = first_line.chars().take(FOUND_TEXT_LIMIT).collect();
        let ellipsis = if shown.len() < text.len() { "..." } else { "" };
        format!("'{shown}{ellipsis}'")
    }

    pub(crate) fn error(&self, message: impl Into<String>, span: Span) -> Box<SqlError> {
        Box::new(SqlError::new(message, span, self.src))
    }
}

#[derive(Debug, Clone, Copy)]
enum Constraint {
    PrimaryKey,
    NotNull,
    Unique,
}

impl Constraint {
    fn name(self) -> &'static str {
        match self {
            Constraint::PrimaryKey => "PRIMARY KEY",
            Constraint::NotNull => "NOT NULL",
            Constraint::Unique => "UNIQUE",
        }
    }
}
