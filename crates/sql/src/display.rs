//! Canonical SQL text for every syntax tree type.
//!
//! The canonical form uses upper-case keywords, `, ` separators, single
//! spaces around binary operators and only the parentheses the tree needs.
//! Parsing the printed text gives back an equal tree.

use std::fmt::{self, Display, Formatter};

use crate::ast::{
    Assignment, BinaryOp, CaseBranch, ColumnDef, CreateIndex, CreateTable, DataType, Delete,
    DropTable, Expr, ExprKind, FromClause, FunctionArgs, Insert, Join, JoinKind, Limit, Literal,
    Name, OrderItem, Row, Select, SelectItem, Spanned, StatementKind, TableRef, UnaryOp, Update,
    precedence,
};
use crate::token::Keyword;

impl<T: Display> Display for Spanned<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.node.fmt(f)
    }
}

/// Prints bare when the name re-lexes to itself, otherwise double-quoted.
impl Display for Name {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if is_bare_identifier(&self.0) {
            return f.write_str(&self.0);
        }
        write!(f, "\"{}\"", self.0.replace('"', "\"\""))
    }
}

fn is_bare_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let valid_start = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_');
    valid_start
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && Keyword::lookup(name).is_none()
}

impl Display for StatementKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            StatementKind::CreateTable(statement) => statement.fmt(f),
            StatementKind::DropTable(statement) => statement.fmt(f),
            StatementKind::CreateIndex(statement) => statement.fmt(f),
            StatementKind::Insert(statement) => statement.fmt(f),
            StatementKind::Update(statement) => statement.fmt(f),
            StatementKind::Delete(statement) => statement.fmt(f),
            StatementKind::Select(statement) => statement.fmt(f),
            StatementKind::Begin => f.write_str("BEGIN"),
            StatementKind::Commit => f.write_str("COMMIT"),
            StatementKind::Rollback => f.write_str("ROLLBACK"),
        }
    }
}

impl Display for CreateTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE TABLE ")?;
        if self.if_not_exists {
            f.write_str("IF NOT EXISTS ")?;
        }
        write!(f, "{} (", self.name)?;
        write_list(f, &self.columns)?;
        f.write_str(")")
    }
}

impl Display for ColumnDef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.data_type)?;
        if self.primary_key {
            f.write_str(" PRIMARY KEY")?;
        }
        if self.not_null {
            f.write_str(" NOT NULL")?;
        }
        if self.unique {
            f.write_str(" UNIQUE")?;
        }
        Ok(())
    }
}

impl Display for DataType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DataType::Integer => "INTEGER",
            DataType::Real => "REAL",
            DataType::Text => "TEXT",
            DataType::Boolean => "BOOLEAN",
        })
    }
}

impl Display for DropTable {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("DROP TABLE ")?;
        if self.if_exists {
            f.write_str("IF EXISTS ")?;
        }
        self.name.fmt(f)
    }
}

impl Display for CreateIndex {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("CREATE ")?;
        if self.unique {
            f.write_str("UNIQUE ")?;
        }
        write!(f, "INDEX {} ON {} ({})", self.name, self.table, self.column)
    }
}

impl Display for Insert {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "INSERT INTO {}", self.table)?;
        if let Some(columns) = &self.columns {
            f.write_str(" (")?;
            write_list(f, columns)?;
            f.write_str(")")?;
        }
        f.write_str(" VALUES ")?;
        write_list(f, &self.rows)
    }
}

impl Display for Row {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("(")?;
        write_list(f, &self.0)?;
        f.write_str(")")
    }
}

impl Display for Update {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "UPDATE {} SET ", self.table)?;
        write_list(f, &self.assignments)?;
        write_where(f, self.where_clause.as_ref())
    }
}

impl Display for Assignment {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} = {}", self.column, self.value)
    }
}

impl Display for Delete {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "DELETE FROM {}", self.table)?;
        write_where(f, self.where_clause.as_ref())
    }
}

impl Display for Select {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("SELECT ")?;
        if self.distinct {
            f.write_str("DISTINCT ")?;
        }
        write_list(f, &self.items)?;
        if let Some(from) = &self.from {
            write!(f, " {from}")?;
        }
        write_where(f, self.where_clause.as_ref())?;
        if !self.group_by.is_empty() {
            f.write_str(" GROUP BY ")?;
            write_list(f, &self.group_by)?;
        }
        if let Some(having) = &self.having {
            write!(f, " HAVING {having}")?;
        }
        if !self.order_by.is_empty() {
            f.write_str(" ORDER BY ")?;
            write_list(f, &self.order_by)?;
        }
        if let Some(limit) = &self.limit {
            write!(f, " {limit}")?;
        }
        Ok(())
    }
}

impl Display for SelectItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Wildcard => f.write_str("*"),
            SelectItem::QualifiedWildcard(table) => write!(f, "{table}.*"),
            SelectItem::Expr { expr, alias: None } => expr.fmt(f),
            SelectItem::Expr {
                expr,
                alias: Some(alias),
            } => write!(f, "{expr} AS {alias}"),
        }
    }
}

impl Display for FromClause {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "FROM {}", self.table)?;
        for join in &self.joins {
            write!(f, " {join}")?;
        }
        Ok(())
    }
}

impl Display for TableRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.name.fmt(f)?;
        if let Some(alias) = &self.alias {
            write!(f, " AS {alias}")?;
        }
        Ok(())
    }
}

impl Display for Join {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} ON {}", self.kind, self.table, self.on)
    }
}

impl Display for JoinKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            JoinKind::Inner => "JOIN",
            JoinKind::Left => "LEFT JOIN",
        })
    }
}

impl Display for OrderItem {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.expr.fmt(f)?;
        if self.descending {
            f.write_str(" DESC")?;
        }
        Ok(())
    }
}

impl Display for Limit {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "LIMIT {}", self.count)?;
        if let Some(offset) = &self.offset {
            write!(f, " OFFSET {offset}")?;
        }
        Ok(())
    }
}

impl Display for Literal {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Literal::Integer(value) => write!(f, "{value}"),
            Literal::Real(value) => write_real(f, *value),
            Literal::String(text) => write!(f, "'{}'", text.replace('\'', "''")),
            Literal::Boolean(true) => f.write_str("TRUE"),
            Literal::Boolean(false) => f.write_str("FALSE"),
            Literal::Null => f.write_str("NULL"),
        }
    }
}

/// Uses the shortest text that parses back to the same `f64`, and keeps a
/// `.` or exponent so the text lexes as a real rather than an integer.
fn write_real(f: &mut Formatter<'_>, value: f64) -> fmt::Result {
    let text = format!("{value:?}");
    f.write_str(&text)?;
    if !text.contains(['.', 'e', 'E']) {
        f.write_str(".0")?;
    }
    Ok(())
}

impl Display for UnaryOp {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UnaryOp::Neg => "-",
            UnaryOp::Not => "NOT",
        })
    }
}

impl Display for BinaryOp {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BinaryOp::Or => "OR",
            BinaryOp::And => "AND",
            BinaryOp::Eq => "=",
            BinaryOp::NotEq => "<>",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::Concat => "||",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
        })
    }
}

impl Display for CaseBranch {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "WHEN {} THEN {}", self.condition, self.result)
    }
}

impl Display for FunctionArgs {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write_function_args(f, self, Mode::Minimal)
    }
}

impl Display for ExprKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write_expr(f, self, Mode::Minimal)
    }
}

impl Spanned<ExprKind> {
    /// Prints the expression with every operator node in parentheses, for
    /// example `(a + (b * c))`, which makes the tree shape visible.
    pub fn fully_parenthesized(&self) -> FullyParenthesized<'_> {
        FullyParenthesized(self)
    }
}

/// The `Display` wrapper returned by [`Expr::fully_parenthesized`].
pub struct FullyParenthesized<'a>(&'a Expr);

impl Display for FullyParenthesized<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write_operand(f, self.0, false, Mode::Full)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Parentheses only where the tree differs from what precedence implies.
    Minimal,
    /// Parentheses around every node that is not a primary expression.
    Full,
}

fn write_expr(f: &mut Formatter<'_>, expr: &ExprKind, mode: Mode) -> fmt::Result {
    match expr {
        ExprKind::Literal(literal) => literal.fmt(f),
        ExprKind::Column { table, name } => {
            if let Some(table) = table {
                write!(f, "{table}.")?;
            }
            name.fmt(f)
        }
        ExprKind::Unary { op, operand } => write_unary(f, *op, operand, mode),
        ExprKind::Binary { op, left, right } => {
            let level = op.precedence();
            write_operand(f, left, level_of(left) < level, mode)?;
            write!(f, " {op} ")?;
            write_operand(f, right, level_of(right) <= level, mode)
        }
        ExprKind::IsNull { operand, negated } => {
            write_subject(f, operand, mode)?;
            f.write_str(if *negated { " IS NOT NULL" } else { " IS NULL" })
        }
        ExprKind::Between {
            operand,
            low,
            high,
            negated,
        } => {
            write_subject(f, operand, mode)?;
            write_negatable(f, *negated, "BETWEEN ")?;
            write_bound(f, low, mode)?;
            f.write_str(" AND ")?;
            write_bound(f, high, mode)
        }
        ExprKind::InList {
            operand,
            list,
            negated,
        } => {
            write_subject(f, operand, mode)?;
            write_negatable(f, *negated, "IN (")?;
            write_operands(f, list, mode)?;
            f.write_str(")")
        }
        ExprKind::Like {
            operand,
            pattern,
            negated,
        } => {
            write_subject(f, operand, mode)?;
            write_negatable(f, *negated, "LIKE ")?;
            write_bound(f, pattern, mode)
        }
        ExprKind::Case {
            branches,
            else_result,
        } => {
            f.write_str("CASE")?;
            for branch in branches {
                f.write_str(" WHEN ")?;
                write_operand(f, &branch.node.condition, false, mode)?;
                f.write_str(" THEN ")?;
                write_operand(f, &branch.node.result, false, mode)?;
            }
            if let Some(else_result) = else_result {
                f.write_str(" ELSE ")?;
                write_operand(f, else_result, false, mode)?;
            }
            f.write_str(" END")
        }
        ExprKind::Function { name, args } => {
            write!(f, "{name}(")?;
            write_function_args(f, args, mode)?;
            f.write_str(")")
        }
    }
}

/// Separates two minus signs with a space so they never form a `--` comment.
fn write_unary(f: &mut Formatter<'_>, op: UnaryOp, operand: &Expr, mode: Mode) -> fmt::Result {
    if op == UnaryOp::Not {
        f.write_str("NOT ")?;
        return write_operand(f, operand, level_of(operand) < precedence::NOT, mode);
    }
    let wrap = needs_wrap(operand, level_of(operand) < precedence::NEGATION, mode);
    let separator = if !wrap && starts_with_minus(&operand.node) {
        " "
    } else {
        ""
    };
    write!(f, "-{separator}")?;
    write_wrapped(f, operand, wrap, mode)
}

fn starts_with_minus(expr: &ExprKind) -> bool {
    match expr {
        ExprKind::Unary {
            op: UnaryOp::Neg, ..
        } => true,
        ExprKind::Literal(Literal::Integer(value)) => *value < 0,
        ExprKind::Literal(Literal::Real(value)) => value.is_sign_negative(),
        _ => false,
    }
}

fn write_subject(f: &mut Formatter<'_>, operand: &Expr, mode: Mode) -> fmt::Result {
    write_operand(f, operand, level_of(operand) < precedence::PREDICATE, mode)
}

/// BETWEEN bounds and LIKE patterns bind at the `||` level.
fn write_bound(f: &mut Formatter<'_>, operand: &Expr, mode: Mode) -> fmt::Result {
    write_operand(f, operand, level_of(operand) < precedence::CONCAT, mode)
}

/// Writes ` keyword` or ` NOT keyword`.
fn write_negatable(f: &mut Formatter<'_>, negated: bool, keyword: &str) -> fmt::Result {
    f.write_str(if negated { " NOT " } else { " " })?;
    f.write_str(keyword)
}

fn write_function_args(f: &mut Formatter<'_>, args: &FunctionArgs, mode: Mode) -> fmt::Result {
    match args {
        FunctionArgs::Star => f.write_str("*"),
        FunctionArgs::List { distinct, args } => {
            if *distinct {
                f.write_str("DISTINCT ")?;
            }
            write_operands(f, args, mode)
        }
    }
}

fn write_operands(f: &mut Formatter<'_>, operands: &[Expr], mode: Mode) -> fmt::Result {
    for (index, operand) in operands.iter().enumerate() {
        if index > 0 {
            f.write_str(", ")?;
        }
        write_operand(f, operand, false, mode)?;
    }
    Ok(())
}

/// Writes a child expression; `needs_parens` says whether minimal mode must
/// wrap it to keep the tree shape.
fn write_operand(
    f: &mut Formatter<'_>,
    operand: &Expr,
    needs_parens: bool,
    mode: Mode,
) -> fmt::Result {
    let wrap = needs_wrap(operand, needs_parens, mode);
    write_wrapped(f, operand, wrap, mode)
}

fn needs_wrap(operand: &Expr, needs_parens: bool, mode: Mode) -> bool {
    match mode {
        Mode::Minimal => needs_parens,
        Mode::Full => level_of(operand) < precedence::PRIMARY,
    }
}

fn write_wrapped(f: &mut Formatter<'_>, operand: &Expr, wrap: bool, mode: Mode) -> fmt::Result {
    if !wrap {
        return write_expr(f, &operand.node, mode);
    }
    f.write_str("(")?;
    write_expr(f, &operand.node, mode)?;
    f.write_str(")")
}

fn level_of(expr: &Expr) -> u8 {
    expr.node.precedence()
}

fn write_list<T: Display>(f: &mut Formatter<'_>, items: &[T]) -> fmt::Result {
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            f.write_str(", ")?;
        }
        item.fmt(f)?;
    }
    Ok(())
}

fn write_where(f: &mut Formatter<'_>, where_clause: Option<&Expr>) -> fmt::Result {
    match where_clause {
        Some(condition) => write!(f, " WHERE {condition}"),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Span;

    const SPAN: Span = Span {
        start: 0,
        end: 0,
        line: 1,
        column: 1,
    };

    fn expr(kind: ExprKind) -> Expr {
        Spanned::new(kind, SPAN)
    }

    fn ident(name: &str) -> crate::ast::Ident {
        Spanned::new(Name(name.to_string()), SPAN)
    }

    fn col(name: &str) -> Expr {
        expr(ExprKind::Column {
            table: None,
            name: ident(name),
        })
    }

    fn lit(literal: Literal) -> Expr {
        expr(ExprKind::Literal(literal))
    }

    fn int(value: i64) -> Expr {
        lit(Literal::Integer(value))
    }

    fn unary(op: UnaryOp, operand: Expr) -> Expr {
        expr(ExprKind::Unary {
            op,
            operand: Box::new(operand),
        })
    }

    fn bin(op: BinaryOp, left: Expr, right: Expr) -> Expr {
        expr(ExprKind::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    fn is_null(operand: Expr, negated: bool) -> Expr {
        expr(ExprKind::IsNull {
            operand: Box::new(operand),
            negated,
        })
    }

    fn function(name: &str, args: FunctionArgs) -> Expr {
        expr(ExprKind::Function {
            name: ident(name),
            args,
        })
    }

    fn case(branches: Vec<(Expr, Expr)>, else_result: Option<Expr>) -> Expr {
        expr(ExprKind::Case {
            branches: branches
                .into_iter()
                .map(|(condition, result)| Spanned::new(CaseBranch { condition, result }, SPAN))
                .collect(),
            else_result: else_result.map(Box::new),
        })
    }

    #[test]
    fn names_are_quoted_only_when_needed() {
        let cases = [
            ("foo", "foo"),
            ("_x9", "_x9"),
            ("text", "text"),
            ("count", "count"),
            ("Foo", "\"Foo\""),
            ("select", "\"select\""),
            ("a b", "\"a b\""),
            ("a\"b", "\"a\"\"b\""),
            ("9x", "\"9x\""),
            ("é", "\"é\""),
        ];
        for (name, printed) in cases {
            assert_eq!(Name(name.to_string()).to_string(), printed, "{name}");
        }
    }

    #[test]
    fn literals_print_canonically() {
        let cases = [
            (Literal::Integer(42), "42"),
            (Literal::Real(1.0), "1.0"),
            (Literal::Real(100.0), "100.0"),
            (Literal::Real(0.1), "0.1"),
            (Literal::Real(1e300), "1e300"),
            (Literal::Real(5e-324), "5e-324"),
            (Literal::String("it's".to_string()), "'it''s'"),
            (Literal::String(String::new()), "''"),
            (Literal::Boolean(true), "TRUE"),
            (Literal::Boolean(false), "FALSE"),
            (Literal::Null, "NULL"),
        ];
        for (literal, printed) in cases {
            assert_eq!(literal.to_string(), printed);
        }
    }

    #[test]
    fn every_expression_form_prints_canonically() {
        let qualified = expr(ExprKind::Column {
            table: Some(ident("t")),
            name: ident("Col"),
        });
        let cases = [
            (qualified, "t.\"Col\""),
            (unary(UnaryOp::Neg, col("a")), "-a"),
            (unary(UnaryOp::Not, col("a")), "NOT a"),
            (bin(BinaryOp::NotEq, col("a"), int(1)), "a <> 1"),
            (bin(BinaryOp::Concat, col("a"), col("b")), "a || b"),
            (is_null(col("a"), false), "a IS NULL"),
            (is_null(col("a"), true), "a IS NOT NULL"),
            (
                expr(ExprKind::Between {
                    operand: Box::new(col("x")),
                    low: Box::new(int(1)),
                    high: Box::new(int(2)),
                    negated: true,
                }),
                "x NOT BETWEEN 1 AND 2",
            ),
            (
                expr(ExprKind::InList {
                    operand: Box::new(col("x")),
                    list: vec![int(1), int(2)],
                    negated: false,
                }),
                "x IN (1, 2)",
            ),
            (
                expr(ExprKind::Like {
                    operand: Box::new(col("x")),
                    pattern: Box::new(lit(Literal::String("a%".to_string()))),
                    negated: true,
                }),
                "x NOT LIKE 'a%'",
            ),
            (
                case(vec![(col("a"), int(1)), (col("b"), int(2))], Some(int(3))),
                "CASE WHEN a THEN 1 WHEN b THEN 2 ELSE 3 END",
            ),
            (
                case(vec![(col("a"), int(1))], None),
                "CASE WHEN a THEN 1 END",
            ),
            (function("count", FunctionArgs::Star), "count(*)"),
            (
                function(
                    "count",
                    FunctionArgs::List {
                        distinct: true,
                        args: vec![col("x")],
                    },
                ),
                "count(DISTINCT x)",
            ),
            (
                function(
                    "f",
                    FunctionArgs::List {
                        distinct: false,
                        args: Vec::new(),
                    },
                ),
                "f()",
            ),
        ];
        for (tree, printed) in cases {
            assert_eq!(tree.to_string(), printed);
        }
    }

    #[test]
    fn minimal_mode_adds_parentheses_only_where_the_tree_needs_them() {
        let cases = [
            (
                bin(BinaryOp::Eq, col("a"), unary(UnaryOp::Not, col("b"))),
                "a = (NOT b)",
            ),
            (
                unary(UnaryOp::Neg, bin(BinaryOp::Add, col("a"), col("b"))),
                "-(a + b)",
            ),
            (
                bin(
                    BinaryOp::Sub,
                    col("a"),
                    bin(BinaryOp::Sub, col("b"), col("c")),
                ),
                "a - (b - c)",
            ),
            (
                bin(
                    BinaryOp::Sub,
                    bin(BinaryOp::Sub, col("a"), col("b")),
                    col("c"),
                ),
                "a - b - c",
            ),
            (
                bin(
                    BinaryOp::Mul,
                    bin(BinaryOp::Add, col("a"), col("b")),
                    col("c"),
                ),
                "(a + b) * c",
            ),
            (
                bin(BinaryOp::Concat, is_null(col("a"), false), col("b")),
                "(a IS NULL) || b",
            ),
            (
                bin(
                    BinaryOp::And,
                    bin(BinaryOp::Or, col("a"), col("b")),
                    col("c"),
                ),
                "(a OR b) AND c",
            ),
            (
                unary(UnaryOp::Not, bin(BinaryOp::And, col("a"), col("b"))),
                "NOT (a AND b)",
            ),
        ];
        for (tree, printed) in cases {
            assert_eq!(tree.to_string(), printed);
        }
    }

    #[test]
    fn minus_signs_never_form_a_comment() {
        let double = unary(UnaryOp::Neg, unary(UnaryOp::Neg, col("x")));
        assert_eq!(double.to_string(), "- -x");
        let negative_literal = unary(UnaryOp::Neg, int(-5));
        assert_eq!(negative_literal.to_string(), "- -5");
        assert_eq!(negative_literal.fully_parenthesized().to_string(), "(- -5)");
        let subtraction = bin(BinaryOp::Sub, col("a"), unary(UnaryOp::Neg, col("b")));
        assert_eq!(subtraction.to_string(), "a - -b");
    }

    #[test]
    fn fully_parenthesized_wraps_every_operator_node() {
        let cases = [
            (
                bin(
                    BinaryOp::Add,
                    col("a"),
                    bin(BinaryOp::Mul, col("b"), col("c")),
                ),
                "(a + (b * c))",
            ),
            (
                bin(BinaryOp::Mul, unary(UnaryOp::Neg, col("a")), col("b")),
                "((-a) * b)",
            ),
            (
                unary(UnaryOp::Not, bin(BinaryOp::Eq, col("a"), col("b"))),
                "(NOT (a = b))",
            ),
            (
                unary(UnaryOp::Neg, unary(UnaryOp::Neg, col("a"))),
                "(-(-a))",
            ),
            (is_null(col("x"), true), "(x IS NOT NULL)"),
            (
                case(
                    vec![(bin(BinaryOp::Eq, col("a"), int(1)), int(1))],
                    Some(int(2)),
                ),
                "CASE WHEN (a = 1) THEN 1 ELSE 2 END",
            ),
            (
                function(
                    "count",
                    FunctionArgs::List {
                        distinct: false,
                        args: vec![bin(BinaryOp::Add, col("a"), col("b"))],
                    },
                ),
                "count((a + b))",
            ),
            (col("a"), "a"),
        ];
        for (tree, printed) in cases {
            assert_eq!(tree.fully_parenthesized().to_string(), printed);
        }
    }

    #[test]
    fn statements_print_canonically() {
        let column = |name: &str, data_type, primary_key, not_null, unique| {
            Spanned::new(
                ColumnDef {
                    name: ident(name),
                    data_type: Spanned::new(data_type, SPAN),
                    primary_key,
                    not_null,
                    unique,
                },
                SPAN,
            )
        };
        let create = StatementKind::CreateTable(CreateTable {
            if_not_exists: true,
            name: ident("t"),
            columns: vec![
                column("id", DataType::Integer, true, true, true),
                column("score", DataType::Real, false, false, false),
            ],
        });
        assert_eq!(
            create.to_string(),
            "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY NOT NULL UNIQUE, score REAL)"
        );
        let insert = StatementKind::Insert(Insert {
            table: ident("t"),
            columns: Some(vec![ident("a"), ident("b")]),
            rows: vec![
                Spanned::new(Row(vec![int(1), int(2)]), SPAN),
                Spanned::new(Row(vec![int(3), int(4)]), SPAN),
            ],
        });
        assert_eq!(
            insert.to_string(),
            "INSERT INTO t (a, b) VALUES (1, 2), (3, 4)"
        );
        let select = Select {
            distinct: true,
            items: vec![
                Spanned::new(SelectItem::Wildcard, SPAN),
                Spanned::new(SelectItem::QualifiedWildcard(ident("u")), SPAN),
                Spanned::new(
                    SelectItem::Expr {
                        expr: col("a"),
                        alias: Some(ident("x")),
                    },
                    SPAN,
                ),
            ],
            from: Some(Spanned::new(
                FromClause {
                    table: Spanned::new(
                        TableRef {
                            name: ident("t"),
                            alias: None,
                        },
                        SPAN,
                    ),
                    joins: vec![Spanned::new(
                        Join {
                            kind: JoinKind::Left,
                            table: Spanned::new(
                                TableRef {
                                    name: ident("u"),
                                    alias: Some(ident("v")),
                                },
                                SPAN,
                            ),
                            on: bin(BinaryOp::Eq, col("a"), col("b")),
                        },
                        SPAN,
                    )],
                },
                SPAN,
            )),
            where_clause: Some(col("c")),
            group_by: vec![col("a"), col("b")],
            having: Some(col("d")),
            order_by: vec![Spanned::new(
                OrderItem {
                    expr: col("a"),
                    descending: true,
                },
                SPAN,
            )],
            limit: Some(Spanned::new(
                Limit {
                    count: Spanned::new(10, SPAN),
                    offset: Some(Spanned::new(5, SPAN)),
                },
                SPAN,
            )),
        };
        assert_eq!(
            select.to_string(),
            "SELECT DISTINCT *, u.*, a AS x FROM t LEFT JOIN u AS v ON a = b WHERE c \
             GROUP BY a, b HAVING d ORDER BY a DESC LIMIT 10 OFFSET 5"
        );
    }
}
