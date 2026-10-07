//! Name resolution and static typing. The binder turns `cairn_sql::Expr`
//! into [`BoundExpr`], with column references resolved to positions in the
//! input row and a static [`SqlType`] on every node. Every type error is
//! reported here, before any row is read, spanned to the offending node.

use cairn_sql::{
    BinaryOp, CaseBranch, Expr, ExprKind, FunctionArgs, Ident, Literal, Span, Spanned, UnaryOp,
};

use crate::error::{ErrorKind, ExecError};
use crate::functions::{AggFn, ScalarFn};
use crate::value::{SqlType, Value};

/// One table in scope: its binding name (alias or table name), its columns
/// and where they start in the input row.
#[derive(Debug, Clone)]
pub struct Binding {
    pub name: String,
    pub columns: Vec<(String, SqlType)>,
    pub offset: usize,
}

#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub bindings: Vec<Binding>,
}

impl Scope {
    pub fn width(&self) -> usize {
        self.bindings.iter().map(|b| b.columns.len()).sum()
    }

    pub fn push(&mut self, name: String, columns: Vec<(String, SqlType)>) {
        let offset = self.width();
        self.bindings.push(Binding {
            name,
            columns,
            offset,
        });
    }

    /// The first `count` bindings.
    pub fn prefix(&self, count: usize) -> Scope {
        Scope {
            bindings: self.bindings.iter().take(count).cloned().collect(),
        }
    }

    fn resolve(&self, table: Option<&Ident>, name: &Ident) -> Result<(usize, SqlType), ExecError> {
        let column = name.node.0.as_str();
        if let Some(qualifier) = table {
            let binding = self
                .bindings
                .iter()
                .find(|b| b.name == qualifier.node.0)
                .ok_or_else(|| {
                    ExecError::at(
                        ErrorKind::NotFound,
                        format!("no such table or alias: {}", qualifier.node.0),
                        qualifier.span,
                    )
                })?;
            return binding
                .columns
                .iter()
                .position(|(n, _)| n == column)
                .and_then(|i| {
                    binding
                        .columns
                        .get(i)
                        .map(|(_, ty)| (binding.offset + i, *ty))
                })
                .ok_or_else(|| {
                    ExecError::at(
                        ErrorKind::NotFound,
                        format!("no such column: {}.{column}", qualifier.node.0),
                        name.span,
                    )
                });
        }
        let mut found = None;
        for binding in &self.bindings {
            for (i, (n, ty)) in binding.columns.iter().enumerate() {
                if n != column {
                    continue;
                }
                if found.is_some() {
                    return Err(ExecError::at(
                        ErrorKind::Type,
                        format!("ambiguous column name: {column}"),
                        name.span,
                    ));
                }
                found = Some((binding.offset + i, *ty));
            }
        }
        found.ok_or_else(|| {
            ExecError::at(
                ErrorKind::NotFound,
                format!("no such column: {column}"),
                name.span,
            )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

impl CmpOp {
    /// The operator with its operands swapped (`k < c` is `c > k`).
    pub fn flipped(self) -> CmpOp {
        match self {
            CmpOp::Lt => CmpOp::Gt,
            CmpOp::LtEq => CmpOp::GtEq,
            CmpOp::Gt => CmpOp::Lt,
            CmpOp::GtEq => CmpOp::LtEq,
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone)]
pub struct BoundExpr {
    pub kind: BoundKind,
    pub ty: SqlType,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum BoundKind {
    Literal(Value),
    Column(usize),
    Neg(Box<BoundExpr>),
    Not(Box<BoundExpr>),
    And(Box<BoundExpr>, Box<BoundExpr>),
    Or(Box<BoundExpr>, Box<BoundExpr>),
    Compare(CmpOp, Box<BoundExpr>, Box<BoundExpr>),
    Arith(ArithOp, Box<BoundExpr>, Box<BoundExpr>),
    Concat(Box<BoundExpr>, Box<BoundExpr>),
    IsNull {
        operand: Box<BoundExpr>,
        negated: bool,
    },
    Between {
        operand: Box<BoundExpr>,
        low: Box<BoundExpr>,
        high: Box<BoundExpr>,
        negated: bool,
    },
    InList {
        operand: Box<BoundExpr>,
        list: Vec<BoundExpr>,
        negated: bool,
    },
    Like {
        operand: Box<BoundExpr>,
        pattern: Box<BoundExpr>,
        negated: bool,
    },
    Case {
        branches: Vec<(BoundExpr, BoundExpr)>,
        else_result: Option<Box<BoundExpr>>,
    },
    Scalar(ScalarFn, Vec<BoundExpr>),
}

impl BoundExpr {
    /// Whether the expression reads no column, so it can be evaluated once.
    pub fn is_constant(&self) -> bool {
        let mut constant = true;
        self.visit(&mut |e| constant &= !matches!(e.kind, BoundKind::Column(_)));
        constant
    }

    /// The highest column position the expression reads, if any.
    pub fn max_column(&self) -> Option<usize> {
        let mut max = None;
        self.visit(&mut |e| {
            if let BoundKind::Column(i) = e.kind {
                max = Some(max.map_or(i, |m: usize| m.max(i)));
            }
        });
        max
    }

    fn visit(&self, f: &mut impl FnMut(&BoundExpr)) {
        f(self);
        match &self.kind {
            BoundKind::Literal(_) | BoundKind::Column(_) => {}
            BoundKind::Neg(e) | BoundKind::Not(e) => e.visit(f),
            BoundKind::IsNull { operand, .. } => operand.visit(f),
            BoundKind::And(a, b)
            | BoundKind::Or(a, b)
            | BoundKind::Compare(_, a, b)
            | BoundKind::Arith(_, a, b)
            | BoundKind::Concat(a, b) => {
                a.visit(f);
                b.visit(f);
            }
            BoundKind::Like {
                operand, pattern, ..
            } => {
                operand.visit(f);
                pattern.visit(f);
            }
            BoundKind::Between {
                operand, low, high, ..
            } => {
                operand.visit(f);
                low.visit(f);
                high.visit(f);
            }
            BoundKind::InList { operand, list, .. } => {
                operand.visit(f);
                list.iter().for_each(|e| e.visit(f));
            }
            BoundKind::Case {
                branches,
                else_result,
            } => {
                for (condition, result) in branches {
                    condition.visit(f);
                    result.visit(f);
                }
                if let Some(e) = else_result {
                    e.visit(f);
                }
            }
            BoundKind::Scalar(_, args) => args.iter().for_each(|e| e.visit(f)),
        }
    }
}

/// An aggregate call collected while binding a grouped query. Its result
/// is a column of the aggregate operator's output row.
#[derive(Debug, Clone)]
pub struct AggCall {
    pub func: AggFn,
    /// `None` for `COUNT(*)`.
    pub arg: Option<BoundExpr>,
    pub distinct: bool,
    pub span: Span,
    ast: Expr,
}

/// The state of a grouped query: GROUP BY expressions and aggregate calls.
/// Output rows of the aggregate operator are the group values followed by
/// the aggregate results.
#[derive(Debug, Default)]
pub struct Grouping {
    pub groups: Vec<(Expr, SqlType)>,
    pub aggs: Vec<AggCall>,
}

/// Where an expression is bound, which decides how aggregates are treated.
pub enum Mode<'a> {
    /// Aggregates are an error naming the clause.
    Plain(&'static str),
    /// Grouped query: GROUP BY expressions and aggregates become columns of
    /// the aggregate output; the scope is the pre-aggregation scope.
    Grouped(&'a mut Grouping),
    /// Inside an aggregate's argument: another aggregate is an error.
    InAggregate,
}

pub fn is_aggregate_name(name: &str) -> bool {
    AggFn::from_name(name).is_some()
}

/// Whether the expression contains an aggregate call.
pub fn contains_aggregate(expr: &Expr) -> bool {
    match &expr.node {
        ExprKind::Literal(_) | ExprKind::Column { .. } => false,
        ExprKind::Function { name, args } => {
            is_aggregate_name(&name.node.0)
                || matches!(args, FunctionArgs::List { args, .. } if args.iter().any(contains_aggregate))
        }
        ExprKind::Unary { operand, .. } | ExprKind::IsNull { operand, .. } => {
            contains_aggregate(operand)
        }
        ExprKind::Binary { left, right, .. } => {
            contains_aggregate(left) || contains_aggregate(right)
        }
        ExprKind::Between {
            operand, low, high, ..
        } => contains_aggregate(operand) || contains_aggregate(low) || contains_aggregate(high),
        ExprKind::InList { operand, list, .. } => {
            contains_aggregate(operand) || list.iter().any(contains_aggregate)
        }
        ExprKind::Like {
            operand, pattern, ..
        } => contains_aggregate(operand) || contains_aggregate(pattern),
        ExprKind::Case {
            branches,
            else_result,
        } => {
            branches.iter().any(|b| {
                contains_aggregate(&b.node.condition) || contains_aggregate(&b.node.result)
            }) || else_result.as_deref().is_some_and(contains_aggregate)
        }
    }
}

/// Binds an expression that must be BOOLEAN (or NULL), such as a WHERE.
pub fn bind_condition(
    expr: &Expr,
    scope: &Scope,
    mode: &mut Mode,
    clause: &str,
) -> Result<BoundExpr, ExecError> {
    let bound = bind(expr, scope, mode)?;
    require_boolean(&bound, clause)?;
    Ok(bound)
}

fn require_boolean(bound: &BoundExpr, what: &str) -> Result<(), ExecError> {
    match bound.ty {
        SqlType::Boolean | SqlType::Null => Ok(()),
        other => Err(ExecError::at(
            ErrorKind::Type,
            format!("type mismatch: {what} needs BOOLEAN, found {other}"),
            bound.span,
        )),
    }
}

fn node(kind: BoundKind, ty: SqlType, span: Span) -> BoundExpr {
    BoundExpr { kind, ty, span }
}

pub fn bind(expr: &Expr, scope: &Scope, mode: &mut Mode) -> Result<BoundExpr, ExecError> {
    if let Some(grouped) = group_column(expr, mode) {
        return Ok(grouped);
    }
    let span = expr.span;
    match &expr.node {
        ExprKind::Literal(literal) => Ok(bind_literal(literal, span)),
        ExprKind::Column { table, name } => bind_column(table.as_ref(), name, scope, mode),
        ExprKind::Unary { op, operand } => bind_unary(*op, operand, span, scope, mode),
        ExprKind::Binary { op, left, right } => {
            let left = bind(left, scope, mode)?;
            let right = bind(right, scope, mode)?;
            bind_binary(*op, left, right, span)
        }
        ExprKind::IsNull { operand, negated } => {
            let operand = Box::new(bind(operand, scope, mode)?);
            let kind = BoundKind::IsNull {
                operand,
                negated: *negated,
            };
            Ok(node(kind, SqlType::Boolean, span))
        }
        ExprKind::Between {
            operand,
            low,
            high,
            negated,
        } => bind_between([operand, low, high], *negated, span, scope, mode),
        ExprKind::InList {
            operand,
            list,
            negated,
        } => bind_in_list(operand, list, *negated, span, scope, mode),
        ExprKind::Like {
            operand,
            pattern,
            negated,
        } => bind_like(operand, pattern, *negated, span, scope, mode),
        ExprKind::Case {
            branches,
            else_result,
        } => bind_case(branches, else_result.as_deref(), span, scope, mode),
        ExprKind::Function { name, args } => bind_function(expr, name, args, scope, mode),
    }
}

/// In a grouped query, an expression equal to a GROUP BY expression reads
/// that group column of the aggregate output.
fn group_column(expr: &Expr, mode: &Mode) -> Option<BoundExpr> {
    let Mode::Grouped(grouping) = mode else {
        return None;
    };
    let i = grouping.groups.iter().position(|(g, _)| g == expr)?;
    let ty = grouping.groups.get(i).map_or(SqlType::Null, |(_, ty)| *ty);
    Some(node(BoundKind::Column(i), ty, expr.span))
}

fn bind_literal(literal: &Literal, span: Span) -> BoundExpr {
    let value = literal_value(literal);
    let ty = value.sql_type();
    node(BoundKind::Literal(value), ty, span)
}

fn bind_unary(
    op: UnaryOp,
    operand: &Expr,
    span: Span,
    scope: &Scope,
    mode: &mut Mode,
) -> Result<BoundExpr, ExecError> {
    let operand = bind(operand, scope, mode)?;
    if op == UnaryOp::Not {
        require_boolean(&operand, "NOT")?;
        return Ok(node(
            BoundKind::Not(Box::new(operand)),
            SqlType::Boolean,
            span,
        ));
    }
    if !(operand.ty.is_numeric() || operand.ty == SqlType::Null) {
        return Err(ExecError::at(
            ErrorKind::Type,
            format!("type mismatch: cannot apply - to {}", operand.ty),
            span,
        ));
    }
    let ty = operand.ty;
    Ok(node(BoundKind::Neg(Box::new(operand)), ty, span))
}

fn bind_between(
    [operand, low, high]: [&Expr; 3],
    negated: bool,
    span: Span,
    scope: &Scope,
    mode: &mut Mode,
) -> Result<BoundExpr, ExecError> {
    let operand = bind(operand, scope, mode)?;
    let low = bind(low, scope, mode)?;
    let high = bind(high, scope, mode)?;
    require_comparable(operand.ty, low.ty, span)?;
    require_comparable(operand.ty, high.ty, span)?;
    let kind = BoundKind::Between {
        operand: Box::new(operand),
        low: Box::new(low),
        high: Box::new(high),
        negated,
    };
    Ok(node(kind, SqlType::Boolean, span))
}

fn bind_in_list(
    operand: &Expr,
    list: &[Expr],
    negated: bool,
    span: Span,
    scope: &Scope,
    mode: &mut Mode,
) -> Result<BoundExpr, ExecError> {
    let operand = bind(operand, scope, mode)?;
    let mut bound = Vec::with_capacity(list.len());
    for item in list {
        let item = bind(item, scope, mode)?;
        require_comparable(operand.ty, item.ty, item.span)?;
        bound.push(item);
    }
    let kind = BoundKind::InList {
        operand: Box::new(operand),
        list: bound,
        negated,
    };
    Ok(node(kind, SqlType::Boolean, span))
}

fn bind_like(
    operand: &Expr,
    pattern: &Expr,
    negated: bool,
    span: Span,
    scope: &Scope,
    mode: &mut Mode,
) -> Result<BoundExpr, ExecError> {
    let operand = bind(operand, scope, mode)?;
    let pattern = bind(pattern, scope, mode)?;
    if !text_like(operand.ty) || !text_like(pattern.ty) {
        return Err(ExecError::at(
            ErrorKind::Type,
            format!(
                "type mismatch: cannot apply LIKE to {} and {}",
                operand.ty, pattern.ty
            ),
            span,
        ));
    }
    let kind = BoundKind::Like {
        operand: Box::new(operand),
        pattern: Box::new(pattern),
        negated,
    };
    Ok(node(kind, SqlType::Boolean, span))
}

fn bind_case(
    branches: &[Spanned<CaseBranch>],
    else_result: Option<&Expr>,
    span: Span,
    scope: &Scope,
    mode: &mut Mode,
) -> Result<BoundExpr, ExecError> {
    let mut bound = Vec::with_capacity(branches.len());
    let mut ty = SqlType::Null;
    for branch in branches {
        let condition = bind(&branch.node.condition, scope, mode)?;
        require_boolean(&condition, "CASE condition")?;
        let result = bind(&branch.node.result, scope, mode)?;
        ty = unify(ty, result.ty, "CASE branches", span)?;
        bound.push((condition, result));
    }
    let else_result = match else_result {
        Some(e) => {
            let e = bind(e, scope, mode)?;
            ty = unify(ty, e.ty, "CASE branches", span)?;
            Some(Box::new(e))
        }
        None => None,
    };
    let kind = BoundKind::Case {
        branches: bound,
        else_result,
    };
    Ok(node(kind, ty, span))
}

fn text_like(ty: SqlType) -> bool {
    matches!(ty, SqlType::Text | SqlType::Null)
}

fn literal_value(literal: &Literal) -> Value {
    match literal {
        Literal::Integer(v) => Value::Integer(*v),
        Literal::Real(v) => Value::real(*v),
        Literal::String(s) => Value::Text(s.clone()),
        Literal::Boolean(b) => Value::Boolean(*b),
        Literal::Null => Value::Null,
    }
}

fn bind_column(
    table: Option<&Ident>,
    name: &Ident,
    scope: &Scope,
    mode: &mut Mode,
) -> Result<BoundExpr, ExecError> {
    if let Mode::Plain("VALUES") = mode {
        return Err(ExecError::at(
            ErrorKind::Type,
            "column references are not allowed in VALUES",
            name.span,
        ));
    }
    let (position, ty) = scope.resolve(table, name)?;
    if let Mode::Grouped(_) = mode {
        return Err(ExecError::at(
            ErrorKind::Type,
            format!(
                "column {} must appear in GROUP BY or be used in an aggregate function",
                name.node.0
            ),
            name.span,
        ));
    }
    let span = table.map_or(name.span, |t| t.span.until(name.span));
    Ok(node(BoundKind::Column(position), ty, span))
}

fn require_comparable(left: SqlType, right: SqlType, span: Span) -> Result<(), ExecError> {
    if left.comparable(right) {
        return Ok(());
    }
    Err(ExecError::at(
        ErrorKind::Type,
        format!("type mismatch: cannot compare {left} with {right}"),
        span,
    ))
}

fn unify(a: SqlType, b: SqlType, what: &str, span: Span) -> Result<SqlType, ExecError> {
    a.unify(b).ok_or_else(|| {
        ExecError::at(
            ErrorKind::Type,
            format!("type mismatch: {what} have types {a} and {b}"),
            span,
        )
    })
}

fn bind_binary(
    op: BinaryOp,
    left: BoundExpr,
    right: BoundExpr,
    span: Span,
) -> Result<BoundExpr, ExecError> {
    let mismatch = |symbol: &str, l: SqlType, r: SqlType| {
        ExecError::at(
            ErrorKind::Type,
            format!("type mismatch: cannot apply {symbol} to {l} and {r}"),
            span,
        )
    };
    let (lt, rt) = (left.ty, right.ty);
    let (l, r) = (Box::new(left), Box::new(right));
    let cmp = |op| -> Result<BoundExpr, ExecError> {
        require_comparable(lt, rt, span)?;
        Ok(node(
            BoundKind::Compare(op, l.clone(), r.clone()),
            SqlType::Boolean,
            span,
        ))
    };
    match op {
        BinaryOp::Or | BinaryOp::And => {
            let word = if op == BinaryOp::Or { "OR" } else { "AND" };
            require_boolean(&l, word)?;
            require_boolean(&r, word)?;
            let kind = if op == BinaryOp::Or {
                BoundKind::Or(l, r)
            } else {
                BoundKind::And(l, r)
            };
            Ok(node(kind, SqlType::Boolean, span))
        }
        BinaryOp::Eq => cmp(CmpOp::Eq),
        BinaryOp::NotEq => cmp(CmpOp::NotEq),
        BinaryOp::Lt => cmp(CmpOp::Lt),
        BinaryOp::LtEq => cmp(CmpOp::LtEq),
        BinaryOp::Gt => cmp(CmpOp::Gt),
        BinaryOp::GtEq => cmp(CmpOp::GtEq),
        BinaryOp::Concat => {
            if !text_like(lt) || !text_like(rt) {
                return Err(mismatch("||", lt, rt));
            }
            let ty = if lt == SqlType::Null && rt == SqlType::Null {
                SqlType::Null
            } else {
                SqlType::Text
            };
            Ok(node(BoundKind::Concat(l, r), ty, span))
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            let (arith, symbol) = match op {
                BinaryOp::Add => (ArithOp::Add, "+"),
                BinaryOp::Sub => (ArithOp::Sub, "-"),
                BinaryOp::Mul => (ArithOp::Mul, "*"),
                BinaryOp::Div => (ArithOp::Div, "/"),
                _ => (ArithOp::Mod, "%"),
            };
            let numeric = |t: SqlType| t.is_numeric() || t == SqlType::Null;
            if !numeric(lt) || !numeric(rt) {
                return Err(mismatch(symbol, lt, rt));
            }
            if arith == ArithOp::Mod && (lt == SqlType::Real || rt == SqlType::Real) {
                return Err(mismatch(symbol, lt, rt));
            }
            let ty = if lt == SqlType::Real || rt == SqlType::Real {
                SqlType::Real
            } else if lt == SqlType::Integer || rt == SqlType::Integer {
                SqlType::Integer
            } else {
                SqlType::Null
            };
            Ok(node(BoundKind::Arith(arith, l, r), ty, span))
        }
    }
}

fn bind_function(
    expr: &Expr,
    name: &Ident,
    args: &FunctionArgs,
    scope: &Scope,
    mode: &mut Mode,
) -> Result<BoundExpr, ExecError> {
    let span = expr.span;
    let upper = name.node.0.to_uppercase();
    if let Some(func) = AggFn::from_name(&name.node.0) {
        return bind_aggregate(expr, func, &upper, args, scope, mode);
    }
    let func = ScalarFn::from_name(&name.node.0).ok_or_else(|| {
        ExecError::at(
            ErrorKind::NotFound,
            format!("no such function: {}", name.node.0),
            name.span,
        )
    })?;
    let FunctionArgs::List {
        distinct: false,
        args,
    } = args
    else {
        return Err(ExecError::at(
            ErrorKind::Type,
            format!("{upper} does not accept * or DISTINCT"),
            span,
        ));
    };
    let arity_ok = match func {
        ScalarFn::Coalesce => !args.is_empty(),
        _ => args.len() == 1,
    };
    if !arity_ok {
        let expected = if func == ScalarFn::Coalesce {
            "at least 1"
        } else {
            "1"
        };
        return Err(ExecError::at(
            ErrorKind::Type,
            format!(
                "wrong number of arguments to {upper}: expected {expected}, found {}",
                args.len()
            ),
            name.span,
        ));
    }
    let mut bound = Vec::with_capacity(args.len());
    for arg in args {
        bound.push(bind(arg, scope, mode)?);
    }
    let ty = match func {
        ScalarFn::Coalesce => {
            let mut ty = SqlType::Null;
            for arg in &bound {
                ty = unify(ty, arg.ty, "COALESCE arguments", span)?;
            }
            ty
        }
        _ => {
            let arg_ty = bound.first().map_or(SqlType::Null, |a| a.ty);
            let ok = match func {
                ScalarFn::Abs => arg_ty.is_numeric() || arg_ty == SqlType::Null,
                _ => text_like(arg_ty),
            };
            if !ok {
                return Err(ExecError::at(
                    ErrorKind::Type,
                    format!("type mismatch: cannot apply {upper} to {arg_ty}"),
                    span,
                ));
            }
            match func {
                ScalarFn::Length => SqlType::Integer,
                ScalarFn::Lower | ScalarFn::Upper => SqlType::Text,
                _ => arg_ty,
            }
        }
    };
    Ok(node(BoundKind::Scalar(func, bound), ty, span))
}

fn bind_aggregate(
    expr: &Expr,
    func: AggFn,
    upper: &str,
    args: &FunctionArgs,
    scope: &Scope,
    mode: &mut Mode,
) -> Result<BoundExpr, ExecError> {
    let span = expr.span;
    let grouping = match mode {
        Mode::Plain(clause) => {
            return Err(ExecError::at(
                ErrorKind::Type,
                format!("aggregate functions are not allowed in {clause}"),
                span,
            ));
        }
        Mode::InAggregate => {
            return Err(ExecError::at(
                ErrorKind::Type,
                "aggregate functions cannot be nested",
                span,
            ));
        }
        Mode::Grouped(grouping) => grouping,
    };
    let (arg, distinct) = match args {
        FunctionArgs::Star if func == AggFn::Count => (None, false),
        FunctionArgs::Star => {
            return Err(ExecError::at(
                ErrorKind::Type,
                format!("{upper} does not accept * or DISTINCT"),
                span,
            ));
        }
        FunctionArgs::List { distinct, args } => {
            let [arg] = args.as_slice() else {
                return Err(ExecError::at(
                    ErrorKind::Type,
                    format!(
                        "wrong number of arguments to {upper}: expected 1, found {}",
                        args.len()
                    ),
                    span,
                ));
            };
            (Some(bind(arg, scope, &mut Mode::InAggregate)?), *distinct)
        }
    };
    let arg_ty = arg.as_ref().map_or(SqlType::Null, |a| a.ty);
    let ty = match func {
        AggFn::Count => SqlType::Integer,
        AggFn::Avg => SqlType::Real,
        AggFn::Min | AggFn::Max => arg_ty,
        AggFn::Sum => arg_ty,
    };
    if matches!(func, AggFn::Sum | AggFn::Avg) && !(arg_ty.is_numeric() || arg_ty == SqlType::Null)
    {
        return Err(ExecError::at(
            ErrorKind::Type,
            format!("type mismatch: cannot apply {upper} to {arg_ty}"),
            span,
        ));
    }
    let slot = match grouping.aggs.iter().position(|a| a.ast == *expr) {
        Some(slot) => slot,
        None => {
            grouping.aggs.push(AggCall {
                func,
                arg,
                distinct,
                span,
                ast: expr.clone(),
            });
            grouping.aggs.len() - 1
        }
    };
    Ok(node(
        BoundKind::Column(grouping.groups.len() + slot),
        ty,
        span,
    ))
}
