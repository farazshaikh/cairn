//! SQL tokenizer, parser and syntax tree for cairn.
//!
//! [`tokenize`] splits source text into [`Token`]s and [`parse`] turns it
//! into [`Statement`]s. Every token and syntax node carries a [`Span`], every
//! error is a [`SqlError`] whose `Display` points at the offending column, and
//! every syntax tree type prints canonical SQL through `Display` that parses
//! back to an equal tree. The README "SQL" section lists the grammar.
#![warn(missing_docs)]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod ast;
mod display;
mod error;
mod expr;
mod lexer;
mod parser;
mod span;
mod token;

pub use ast::{
    Assignment, BinaryOp, CaseBranch, ColumnDef, CreateIndex, CreateTable, DataType, Delete,
    DropTable, Expr, ExprKind, FromClause, FunctionArgs, Ident, Insert, Join, JoinKind, Limit,
    Literal, Name, OrderItem, Row, Select, SelectItem, Spanned, Statement, StatementKind, TableRef,
    UnaryOp, Update,
};
pub use display::FullyParenthesized;
pub use error::SqlError;
pub use lexer::tokenize;
pub use span::Span;
pub use token::{Keyword, Token, TokenKind};

/// The deepest expression nesting, and the tallest expression tree, that
/// the parser accepts. Deeper input is an error rather than a stack overflow.
pub const MAX_DEPTH: usize = 200;

/// Parses one or more statements separated by `;`; a trailing `;` is allowed.
pub fn parse(src: &str) -> Result<Vec<Statement>, SqlError> {
    parser::Parser::new(src)
        .and_then(parser::Parser::statements)
        .map_err(|error| *error)
}

/// Parses exactly one expression.
pub fn parse_expr(src: &str) -> Result<Expr, SqlError> {
    parser::Parser::new(src)
        .and_then(parser::Parser::single_expr)
        .map_err(|error| *error)
}
