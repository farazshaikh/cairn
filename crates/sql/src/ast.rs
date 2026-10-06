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
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
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

pub type Ident = Spanned<Name>;
pub type Expr = Spanned<ExprKind>;
pub type Statement = Spanned<StatementKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum StatementKind {
    CreateTable(CreateTable),
    DropTable(DropTable),
    CreateIndex(CreateIndex),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    Select(Box<Select>),
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub if_not_exists: bool,
    pub name: Ident,
    pub columns: Vec<Spanned<ColumnDef>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: Ident,
    pub data_type: Spanned<DataType>,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Integer,
    Real,
    Text,
    Boolean,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropTable {
    pub if_exists: bool,
    pub name: Ident,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    pub unique: bool,
    pub name: Ident,
    pub table: Ident,
    pub column: Ident,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: Ident,
    pub columns: Option<Vec<Ident>>,
    pub rows: Vec<Spanned<Row>>,
}

/// One parenthesised `VALUES` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Row(pub Vec<Expr>);

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub table: Ident,
    pub assignments: Vec<Spanned<Assignment>>,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: Ident,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: Ident,
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<Spanned<SelectItem>>,
    pub from: Option<FromClause>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<Spanned<OrderItem>>,
    pub limit: Option<Limit>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*`
    Wildcard,
    /// `t.*`
    QualifiedWildcard(Ident),
    Expr {
        expr: Expr,
        alias: Option<Ident>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FromClause {
    pub table: Spanned<TableRef>,
    pub joins: Vec<Spanned<Join>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    pub name: Ident,
    pub alias: Option<Ident>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: Spanned<TableRef>,
    pub on: Expr,
}

/// `JOIN` and `INNER JOIN` are both `Inner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JoinKind {
    Inner,
    Left,
}

/// `ORDER BY x` and `ORDER BY x ASC` are the same item.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Limit {
    pub count: Spanned<i64>,
    pub offset: Option<Spanned<i64>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Literal(Literal),
    Column {
        table: Option<Ident>,
        name: Ident,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    IsNull {
        operand: Box<Expr>,
        negated: bool,
    },
    Between {
        operand: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    InList {
        operand: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Like {
        operand: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
    },
    Case {
        branches: Vec<CaseBranch>,
        else_result: Option<Box<Expr>>,
    },
    Function {
        name: Ident,
        args: FunctionArgs,
    },
}

/// A literal value. Numbers are never negative: `-1` is unary minus applied to `1`.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Integer(i64),
    Real(f64),
    String(String),
    Boolean(bool),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnaryOp {
    Neg,
    Not,
}

/// `<>` and `!=` are both `NotEq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BinaryOp {
    Or,
    And,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Concat,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CaseBranch {
    pub condition: Expr,
    pub result: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FunctionArgs {
    /// `name(*)`
    Star,
    /// `name()`, `name(a, b)` or `name(DISTINCT a)`.
    List { distinct: bool, args: Vec<Expr> },
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
