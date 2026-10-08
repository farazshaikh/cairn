//! Syntax tree produced by [`crate::parse`].
//!
//! Every node that comes from source text is wrapped in [`Spanned`]. Every
//! type prints canonical SQL through `Display` (see `display.rs`).

use crate::span::Span;

/// A syntax node and the source span it was parsed from.
///
/// Equality compares only `node`, so the same SQL written with different
/// spacing parses to equal trees. Compare `span` directly to test positions.
#[derive(Debug, Clone)]
pub struct Spanned<T> {
    /// The node.
    pub node: T,
    /// Where it was parsed from.
    pub span: Span,
}

impl<T> Spanned<T> {
    /// Wraps `node` with its source span.
    pub fn new(node: T, span: Span) -> Spanned<T> {
        Spanned { node, span }
    }
}

impl<T: PartialEq> PartialEq for Spanned<T> {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node
    }
}

/// A normalised identifier: lowercased unless it was double-quoted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Name(pub String);

/// A name with its position.
pub type Ident = Spanned<Name>;
/// An expression with its position.
pub type Expr = Spanned<ExprKind>;
/// A statement with its position.
pub type Statement = Spanned<StatementKind>;

/// One statement.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementKind {
    /// `CREATE TABLE`.
    CreateTable(CreateTable),
    /// `DROP TABLE`.
    DropTable(DropTable),
    /// `CREATE [UNIQUE] INDEX`.
    CreateIndex(CreateIndex),
    /// `INSERT INTO`.
    Insert(Insert),
    /// `UPDATE`.
    Update(Update),
    /// `DELETE FROM`.
    Delete(Delete),
    /// `SELECT`.
    Select(Box<Select>),
    /// `BEGIN`.
    Begin,
    /// `COMMIT`.
    Commit,
    /// `ROLLBACK`.
    Rollback,
}

/// `CREATE TABLE [IF NOT EXISTS] name (column, ...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    /// `IF NOT EXISTS` was written.
    pub if_not_exists: bool,
    /// The table name.
    pub name: Ident,
    /// The column definitions in order.
    pub columns: Vec<Spanned<ColumnDef>>,
}

/// One column of `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    /// The column name.
    pub name: Ident,
    /// The declared type.
    pub data_type: Spanned<DataType>,
    /// `PRIMARY KEY` was written.
    pub primary_key: bool,
    /// `NOT NULL` was written.
    pub not_null: bool,
    /// `UNIQUE` was written.
    pub unique: bool,
}

/// A column type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    /// `INTEGER`: a 64-bit integer.
    Integer,
    /// `REAL`: a finite double.
    Real,
    /// `TEXT`: UTF-8 text.
    Text,
    /// `BOOLEAN`.
    Boolean,
}

/// `DROP TABLE [IF EXISTS] name`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTable {
    /// `IF EXISTS` was written.
    pub if_exists: bool,
    /// The table name.
    pub name: Ident,
}

/// `CREATE [UNIQUE] INDEX name ON table (column)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    /// `UNIQUE` was written.
    pub unique: bool,
    /// The index name.
    pub name: Ident,
    /// The indexed table.
    pub table: Ident,
    /// The indexed column.
    pub column: Ident,
}

/// `INSERT INTO table [(column, ...)] VALUES (expr, ...), ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// The target table.
    pub table: Ident,
    /// The named columns, if a column list was written.
    pub columns: Option<Vec<Ident>>,
    /// The `VALUES` rows.
    pub rows: Vec<Spanned<Row>>,
}

/// One parenthesised `VALUES` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row(pub Vec<Expr>);

/// `UPDATE table SET column = expr, ... [WHERE expr]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    /// The target table.
    pub table: Ident,
    /// The `SET` assignments in order.
    pub assignments: Vec<Spanned<Assignment>>,
    /// The `WHERE` condition, if any.
    pub where_clause: Option<Expr>,
}

/// `column = expr` in `UPDATE ... SET`.
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    /// The assigned column.
    pub column: Ident,
    /// The new value.
    pub value: Expr,
}

/// `DELETE FROM table [WHERE expr]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    /// The target table.
    pub table: Ident,
    /// The `WHERE` condition, if any.
    pub where_clause: Option<Expr>,
}

/// A `SELECT` statement; clauses that were not written are empty or `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// `SELECT DISTINCT` was written.
    pub distinct: bool,
    /// The select list.
    pub items: Vec<Spanned<SelectItem>>,
    /// The `FROM` clause, if any.
    pub from: Option<Spanned<FromClause>>,
    /// The `WHERE` condition, if any.
    pub where_clause: Option<Expr>,
    /// The `GROUP BY` expressions.
    pub group_by: Vec<Expr>,
    /// The `HAVING` condition, if any.
    pub having: Option<Expr>,
    /// The `ORDER BY` items.
    pub order_by: Vec<Spanned<OrderItem>>,
    /// `LIMIT` and `OFFSET`, if written.
    pub limit: Option<Spanned<Limit>>,
}

/// One entry of a select list.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*`
    Wildcard,
    /// `t.*`
    QualifiedWildcard(Ident),
    /// `expr [AS alias]`.
    Expr {
        /// The expression.
        expr: Expr,
        /// The alias, if `AS alias` was written.
        alias: Option<Ident>,
    },
}

/// `FROM table [join ...]`.
#[derive(Debug, Clone, PartialEq)]
pub struct FromClause {
    /// The first table.
    pub table: Spanned<TableRef>,
    /// The joins in order.
    pub joins: Vec<Spanned<Join>>,
}

/// `table [AS alias]`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    /// The table name.
    pub name: Ident,
    /// The alias, if `AS alias` was written.
    pub alias: Option<Ident>,
}

/// `[INNER | LEFT] JOIN table [AS alias] ON expr`.
#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    /// Inner or left.
    pub kind: JoinKind,
    /// The joined table.
    pub table: Spanned<TableRef>,
    /// The `ON` condition.
    pub on: Expr,
}

/// `JOIN` and `INNER JOIN` are both `Inner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinKind {
    /// Keeps only matching row pairs.
    Inner,
    /// Also keeps each left row without a match, padded with NULLs.
    Left,
}

/// `ORDER BY x` and `ORDER BY x ASC` are the same item.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// The sort key: an expression, an output alias or a position.
    pub expr: Expr,
    /// `DESC` was written.
    pub descending: bool,
}

/// `LIMIT count [OFFSET offset]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    /// The most rows to return.
    pub count: Spanned<i64>,
    /// Rows to skip first, if `OFFSET` was written.
    pub offset: Option<Spanned<i64>>,
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    /// A literal value.
    Literal(Literal),
    /// `column` or `table.column`.
    Column {
        /// The table or alias, if qualified.
        table: Option<Ident>,
        /// The column name.
        name: Ident,
    },
    /// A prefix operator.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// Its operand.
        operand: Box<Expr>,
    },
    /// A binary operator.
    Binary {
        /// The operator.
        op: BinaryOp,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// `x IS [NOT] NULL`.
    IsNull {
        /// The tested expression.
        operand: Box<Expr>,
        /// `NOT` was written.
        negated: bool,
    },
    /// `x [NOT] BETWEEN low AND high`.
    Between {
        /// The tested expression.
        operand: Box<Expr>,
        /// The lower bound, inclusive.
        low: Box<Expr>,
        /// The upper bound, inclusive.
        high: Box<Expr>,
        /// `NOT` was written.
        negated: bool,
    },
    /// `x [NOT] IN (item, ...)`.
    InList {
        /// The tested expression.
        operand: Box<Expr>,
        /// The list items.
        list: Vec<Expr>,
        /// `NOT` was written.
        negated: bool,
    },
    /// `x [NOT] LIKE pattern`.
    Like {
        /// The tested text.
        operand: Box<Expr>,
        /// The pattern.
        pattern: Box<Expr>,
        /// `NOT` was written.
        negated: bool,
    },
    /// `CASE WHEN ... THEN ... [ELSE ...] END`.
    Case {
        /// The `WHEN` branches in order.
        branches: Vec<Spanned<CaseBranch>>,
        /// The `ELSE` result, if written.
        else_result: Option<Box<Expr>>,
    },
    /// A function call.
    Function {
        /// The function name.
        name: Ident,
        /// The arguments.
        args: FunctionArgs,
    },
}

/// A literal value. Numbers are never negative: `-1` is unary minus applied to `1`.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// An integer that fits `i64`.
    Integer(i64),
    /// A finite real.
    Real(f64),
    /// A string, with `''` escapes resolved.
    String(String),
    /// `TRUE` or `FALSE`.
    Boolean(bool),
    /// `NULL`.
    Null,
}

/// A prefix operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    /// `-`
    Neg,
    /// `NOT`
    Not,
}

/// `<>` and `!=` are both `NotEq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    /// `OR`
    Or,
    /// `AND`
    And,
    /// `=`
    Eq,
    /// `<>` or `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `||`
    Concat,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
}

/// `WHEN condition THEN result`.
#[derive(Debug, Clone, PartialEq)]
pub struct CaseBranch {
    /// The `WHEN` condition.
    pub condition: Expr,
    /// The `THEN` result.
    pub result: Expr,
}

/// The arguments of a call.
#[derive(Debug, Clone, PartialEq)]
pub enum FunctionArgs {
    /// `name(*)`
    Star,
    /// `name()`, `name(a, b)` or `name(DISTINCT a)`.
    List {
        /// `DISTINCT` was written.
        distinct: bool,
        /// The arguments in order.
        args: Vec<Expr>,
    },
}

/// Binding levels shared by the parser and the printer, lowest first.
pub(crate) mod precedence {
    pub const OR: u8 = 1;
    pub const AND: u8 = 2;
    pub const NOT: u8 = 3;
    pub const COMPARISON: u8 = 4;
    /// `IS [NOT] NULL`, `[NOT] BETWEEN`, `[NOT] IN`, `[NOT] LIKE`.
    pub const PREDICATE: u8 = 5;
    pub const CONCAT: u8 = 6;
    pub const ADDITIVE: u8 = 7;
    pub const MULTIPLICATIVE: u8 = 8;
    pub const NEGATION: u8 = 9;
    pub const PRIMARY: u8 = 10;
}

impl BinaryOp {
    pub(crate) fn precedence(self) -> u8 {
        match self {
            BinaryOp::Or => precedence::OR,
            BinaryOp::And => precedence::AND,
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq => precedence::COMPARISON,
            BinaryOp::Concat => precedence::CONCAT,
            BinaryOp::Add | BinaryOp::Sub => precedence::ADDITIVE,
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => precedence::MULTIPLICATIVE,
        }
    }
}

impl ExprKind {
    pub(crate) fn precedence(&self) -> u8 {
        match self {
            ExprKind::Binary { op, .. } => op.precedence(),
            ExprKind::Unary {
                op: UnaryOp::Not, ..
            } => precedence::NOT,
            ExprKind::Unary {
                op: UnaryOp::Neg, ..
            } => precedence::NEGATION,
            ExprKind::IsNull { .. }
            | ExprKind::Between { .. }
            | ExprKind::InList { .. }
            | ExprKind::Like { .. } => precedence::PREDICATE,
            ExprKind::Literal(_)
            | ExprKind::Column { .. }
            | ExprKind::Case { .. }
            | ExprKind::Function { .. } => precedence::PRIMARY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: usize) -> Span {
        Span {
            start,
            end: start + 1,
            line: 1,
            column: start + 1,
        }
    }

    fn column(name: &str, start: usize) -> Expr {
        let name = Spanned::new(Name(name.to_string()), at(start));
        Spanned::new(ExprKind::Column { table: None, name }, at(start))
    }

    #[test]
    fn equality_ignores_spans() {
        assert_eq!(column("a", 0), column("a", 7));
    }

    #[test]
    fn equality_compares_every_field() {
        assert_ne!(column("a", 0), column("b", 0));
        let negated = |negated| {
            Spanned::new(
                ExprKind::IsNull {
                    operand: Box::new(column("a", 0)),
                    negated,
                },
                at(0),
            )
        };
        assert_ne!(negated(true), negated(false));
        let real = |value| Spanned::new(ExprKind::Literal(Literal::Real(value)), at(0));
        assert_ne!(real(1.0), real(1.5));
    }

    #[test]
    fn spans_compare_exactly() {
        assert_ne!(at(0), at(1));
    }
}
