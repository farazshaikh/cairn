//! Static typing and evaluation of expressions (README "Expressions",
//! "Types", "NULL" and "Functions").
//!
//! [`check`] resolves names against a [`Scope`] and gives every node a
//! static type, so type errors are found before any row is read. [`eval`]
//! then computes a value for one input row with three-valued logic.
//! `AND`, `OR`, `CASE`, `COALESCE` and `IN` stop at the first deciding
//! operand, so errors in operands they skip never happen.

use cairn_sql::{BinaryOp, Expr, ExprKind, FunctionArgs, Ident, UnaryOp};

use crate::RErrorKind;
use crate::value::{RValue, SType, order};

/// One table in scope: its name for qualified references, its columns and
/// the position of its first column in the input row.
#[derive(Debug, Clone)]
pub(crate) struct Binding {
    pub name: String,
    pub columns: Vec<(String, SType)>,
    pub offset: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub bindings: Vec<Binding>,
}

impl Scope {
    pub fn width(&self) -> usize {
        self.bindings.iter().map(|b| b.columns.len()).sum()
    }

    pub fn add(&mut self, name: String, columns: Vec<(String, SType)>) {
        let offset = self.width();
        self.bindings.push(Binding {
            name,
            columns,
            offset,
        });
    }

    pub fn first(&self, count: usize) -> Scope {
        Scope {
            bindings: self.bindings.iter().take(count).cloned().collect(),
        }
    }

    fn lookup(&self, table: Option<&Ident>, name: &Ident) -> Result<(usize, SType), RErrorKind> {
        let column = &name.node.0;
        if let Some(table) = table {
            let binding = self
                .bindings
                .iter()
                .find(|b| b.name == table.node.0)
                .ok_or(RErrorKind::NotFound)?;
            let index = binding
                .columns
                .iter()
                .position(|(n, _)| n == column)
                .ok_or(RErrorKind::NotFound)?;
            let ty = binding.columns.get(index).map_or(SType::Null, |c| c.1);
            return Ok((binding.offset + index, ty));
        }
        let mut found = None;
        for binding in &self.bindings {
            for (index, (n, ty)) in binding.columns.iter().enumerate() {
                if n == column {
                    if found.is_some() {
                        return Err(RErrorKind::Type);
                    }
                    found = Some((binding.offset + index, *ty));
                }
            }
        }
        found.ok_or(RErrorKind::NotFound)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    pub fn reversed(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Gt => Cmp::Lt,
            Cmp::Ge => Cmp::Le,
            same => same,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scalar {
    Lower,
    Upper,
    Length,
    Abs,
    Coalesce,
}

impl Scalar {
    fn named(name: &str) -> Option<Scalar> {
        match name.to_ascii_lowercase().as_str() {
            "lower" => Some(Scalar::Lower),
            "upper" => Some(Scalar::Upper),
            "length" => Some(Scalar::Length),
            "abs" => Some(Scalar::Abs),
            "coalesce" => Some(Scalar::Coalesce),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Agg {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl Agg {
    pub fn named(name: &str) -> Option<Agg> {
        match name.to_ascii_lowercase().as_str() {
            "count" => Some(Agg::Count),
            "sum" => Some(Agg::Sum),
            "avg" => Some(Agg::Avg),
            "min" => Some(Agg::Min),
            "max" => Some(Agg::Max),
            _ => None,
        }
    }
}

/// A checked expression: its operation and static type.
#[derive(Debug, Clone)]
pub(crate) struct Typed {
    pub op: Op,
    pub ty: SType,
}

#[derive(Debug, Clone)]
pub(crate) enum Op {
    Const(RValue),
    Column(usize),
    Neg(Box<Typed>),
    Not(Box<Typed>),
    And(Box<Typed>, Box<Typed>),
    Or(Box<Typed>, Box<Typed>),
    Compare(Cmp, Box<Typed>, Box<Typed>),
    Arith(Arith, Box<Typed>, Box<Typed>),
    Concat(Box<Typed>, Box<Typed>),
    IsNull(Box<Typed>, bool),
    Between(Box<Typed>, Box<Typed>, Box<Typed>, bool),
    In(Box<Typed>, Vec<Typed>, bool),
    Like(Box<Typed>, Box<Typed>, bool),
    Case(Vec<(Typed, Typed)>, Option<Box<Typed>>),
    Call(Scalar, Vec<Typed>),
}

impl Typed {
    fn new(op: Op, ty: SType) -> Typed {
        Typed { op, ty }
    }

    /// Whether the expression reads no column.
    pub fn constant(&self) -> bool {
        self.columns().is_empty()
    }

    /// Every column position the expression reads.
    pub fn columns(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns(&self, out: &mut Vec<usize>) {
        match &self.op {
            Op::Const(_) => {}
            Op::Column(c) => out.push(*c),
            Op::Neg(a) | Op::Not(a) | Op::IsNull(a, _) => a.collect_columns(out),
            Op::And(a, b)
            | Op::Or(a, b)
            | Op::Compare(_, a, b)
            | Op::Arith(_, a, b)
            | Op::Concat(a, b)
            | Op::Like(a, b, _) => {
                a.collect_columns(out);
                b.collect_columns(out);
            }
            Op::Between(a, b, c, _) => {
                for e in [a, b, c] {
                    e.collect_columns(out);
                }
            }
            Op::In(a, list, _) => {
                a.collect_columns(out);
                list.iter().for_each(|e| e.collect_columns(out));
            }
            Op::Case(branches, otherwise) => {
                for (when, then) in branches {
                    when.collect_columns(out);
                    then.collect_columns(out);
                }
                if let Some(e) = otherwise {
                    e.collect_columns(out);
                }
            }
            Op::Call(_, args) => args.iter().for_each(|e| e.collect_columns(out)),
        }
    }
}

/// One aggregate call of a grouped query.
#[derive(Debug, Clone)]
pub(crate) struct AggCall {
    pub func: Agg,
    /// `None` for `COUNT(*)`.
    pub arg: Option<Typed>,
    pub distinct: bool,
    ast: Expr,
}

/// GROUP BY expressions and the aggregate calls found so far. The rows of
/// a grouped query are the group values followed by the aggregate results.
#[derive(Debug, Default)]
pub(crate) struct Groups {
    pub keys: Vec<(Expr, SType)>,
    pub aggs: Vec<AggCall>,
}

/// Where an expression appears, which decides how columns and aggregates
/// are treated.
pub(crate) enum Context<'a> {
    /// Aggregates are not allowed (WHERE, ON, GROUP BY, SET, ...).
    Plain,
    /// VALUES: no column references and no aggregates.
    Values,
    /// A grouped query's select list, HAVING or ORDER BY.
    Grouped(&'a mut Groups),
    /// An aggregate's argument: no nested aggregates.
    InAggregate,
}

/// Checks a condition, which must be BOOLEAN or NULL.
pub(crate) fn check_condition(
    expr: &Expr,
    scope: &Scope,
    context: &mut Context,
) -> Result<Typed, RErrorKind> {
    let typed = check(expr, scope, context)?;
    boolean(&typed)?;
    Ok(typed)
}

fn boolean(typed: &Typed) -> Result<(), RErrorKind> {
    match typed.ty {
        SType::Boolean | SType::Null => Ok(()),
        _ => Err(RErrorKind::Type),
    }
}

/// Whether the expression calls an aggregate function anywhere.
pub(crate) fn has_aggregate(expr: &Expr) -> bool {
    match &expr.node {
        ExprKind::Literal(_) | ExprKind::Column { .. } => false,
        ExprKind::Function { name, args } => {
            Agg::named(&name.node.0).is_some()
                || matches!(args, FunctionArgs::List { args, .. } if args.iter().any(has_aggregate))
        }
        ExprKind::Unary { operand, .. } | ExprKind::IsNull { operand, .. } => {
            has_aggregate(operand)
        }
        ExprKind::Binary { left, right, .. } => has_aggregate(left) || has_aggregate(right),
        ExprKind::Between {
            operand, low, high, ..
        } => [operand, low, high].iter().any(|e| has_aggregate(e)),
        ExprKind::InList { operand, list, .. } => {
            has_aggregate(operand) || list.iter().any(has_aggregate)
        }
        ExprKind::Like {
            operand, pattern, ..
        } => has_aggregate(operand) || has_aggregate(pattern),
        ExprKind::Case {
            branches,
            else_result,
        } => {
            branches
                .iter()
                .any(|b| has_aggregate(&b.node.condition) || has_aggregate(&b.node.result))
                || else_result.as_deref().is_some_and(has_aggregate)
        }
    }
}

pub(crate) fn check(
    expr: &Expr,
    scope: &Scope,
    context: &mut Context,
) -> Result<Typed, RErrorKind> {
    if let Context::Grouped(groups) = context
        && let Some(index) = groups.keys.iter().position(|(key, _)| key == expr)
    {
        let ty = groups.keys.get(index).map_or(SType::Null, |k| k.1);
        return Ok(Typed::new(Op::Column(index), ty));
    }
    match &expr.node {
        ExprKind::Literal(literal) => {
            let value = RValue::from_literal(literal);
            let ty = value.sql_type();
            Ok(Typed::new(Op::Const(value), ty))
        }
        ExprKind::Column { table, name } => {
            if matches!(context, Context::Values) {
                return Err(RErrorKind::Type);
            }
            let (position, ty) = scope.lookup(table.as_ref(), name)?;
            if matches!(context, Context::Grouped(_)) {
                return Err(RErrorKind::Type);
            }
            Ok(Typed::new(Op::Column(position), ty))
        }
        ExprKind::Unary { op, operand } => {
            let operand = check(operand, scope, context)?;
            match op {
                UnaryOp::Not => {
                    boolean(&operand)?;
                    Ok(Typed::new(Op::Not(Box::new(operand)), SType::Boolean))
                }
                UnaryOp::Neg => {
                    if !operand.ty.numeric_or_null() {
                        return Err(RErrorKind::Type);
                    }
                    let ty = operand.ty;
                    Ok(Typed::new(Op::Neg(Box::new(operand)), ty))
                }
            }
        }
        ExprKind::Binary { op, left, right } => {
            let left = check(left, scope, context)?;
            let right = check(right, scope, context)?;
            binary(*op, left, right)
        }
        ExprKind::IsNull { operand, negated } => {
            let operand = check(operand, scope, context)?;
            Ok(Typed::new(
                Op::IsNull(Box::new(operand), *negated),
                SType::Boolean,
            ))
        }
        ExprKind::Between {
            operand,
            low,
            high,
            negated,
        } => {
            let operand = check(operand, scope, context)?;
            let low = check(low, scope, context)?;
            let high = check(high, scope, context)?;
            if !operand.ty.comparable(low.ty) || !operand.ty.comparable(high.ty) {
                return Err(RErrorKind::Type);
            }
            let op = Op::Between(Box::new(operand), Box::new(low), Box::new(high), *negated);
            Ok(Typed::new(op, SType::Boolean))
        }
        ExprKind::InList {
            operand,
            list,
            negated,
        } => {
            let operand = check(operand, scope, context)?;
            let mut items = Vec::with_capacity(list.len());
            for item in list {
                let item = check(item, scope, context)?;
                if !operand.ty.comparable(item.ty) {
                    return Err(RErrorKind::Type);
                }
                items.push(item);
            }
            Ok(Typed::new(
                Op::In(Box::new(operand), items, *negated),
                SType::Boolean,
            ))
        }
        ExprKind::Like {
            operand,
            pattern,
            negated,
        } => {
            let operand = check(operand, scope, context)?;
            let pattern = check(pattern, scope, context)?;
            if !operand.ty.text_or_null() || !pattern.ty.text_or_null() {
                return Err(RErrorKind::Type);
            }
            Ok(Typed::new(
                Op::Like(Box::new(operand), Box::new(pattern), *negated),
                SType::Boolean,
            ))
        }
        ExprKind::Case {
            branches,
            else_result,
        } => {
            let mut ty = SType::Null;
            let mut checked = Vec::with_capacity(branches.len());
            for branch in branches {
                let when = check(&branch.node.condition, scope, context)?;
                boolean(&when)?;
                let then = check(&branch.node.result, scope, context)?;
                ty = ty.unify(then.ty).ok_or(RErrorKind::Type)?;
                checked.push((when, then));
            }
            let otherwise = match else_result {
                Some(e) => {
                    let e = check(e, scope, context)?;
                    ty = ty.unify(e.ty).ok_or(RErrorKind::Type)?;
                    Some(Box::new(e))
                }
                None => None,
            };
            Ok(Typed::new(Op::Case(checked, otherwise), ty))
        }
        ExprKind::Function { name, args } => function(expr, name, args, scope, context),
    }
}

fn binary(op: BinaryOp, left: Typed, right: Typed) -> Result<Typed, RErrorKind> {
    let (lt, rt) = (left.ty, right.ty);
    let (l, r) = (Box::new(left), Box::new(right));
    let compare = |cmp| {
        if lt.comparable(rt) {
            Ok(Typed::new(
                Op::Compare(cmp, l.clone(), r.clone()),
                SType::Boolean,
            ))
        } else {
            Err(RErrorKind::Type)
        }
    };
    match op {
        BinaryOp::And | BinaryOp::Or => {
            boolean(&l)?;
            boolean(&r)?;
            let op = if op == BinaryOp::And {
                Op::And(l, r)
            } else {
                Op::Or(l, r)
            };
            Ok(Typed::new(op, SType::Boolean))
        }
        BinaryOp::Eq => compare(Cmp::Eq),
        BinaryOp::NotEq => compare(Cmp::Ne),
        BinaryOp::Lt => compare(Cmp::Lt),
        BinaryOp::LtEq => compare(Cmp::Le),
        BinaryOp::Gt => compare(Cmp::Gt),
        BinaryOp::GtEq => compare(Cmp::Ge),
        BinaryOp::Concat => {
            if !lt.text_or_null() || !rt.text_or_null() {
                return Err(RErrorKind::Type);
            }
            let ty = if lt == SType::Null && rt == SType::Null {
                SType::Null
            } else {
                SType::Text
            };
            Ok(Typed::new(Op::Concat(l, r), ty))
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            let arith = match op {
                BinaryOp::Add => Arith::Add,
                BinaryOp::Sub => Arith::Sub,
                BinaryOp::Mul => Arith::Mul,
                BinaryOp::Div => Arith::Div,
                _ => Arith::Rem,
            };
            if !lt.numeric_or_null() || !rt.numeric_or_null() {
                return Err(RErrorKind::Type);
            }
            let real = lt == SType::Real || rt == SType::Real;
            if arith == Arith::Rem && real {
                return Err(RErrorKind::Type);
            }
            let ty = if real {
                SType::Real
            } else if lt == SType::Integer || rt == SType::Integer {
                SType::Integer
            } else {
                SType::Null
            };
            Ok(Typed::new(Op::Arith(arith, l, r), ty))
        }
    }
}

fn function(
    expr: &Expr,
    name: &Ident,
    args: &FunctionArgs,
    scope: &Scope,
    context: &mut Context,
) -> Result<Typed, RErrorKind> {
    if let Some(func) = Agg::named(&name.node.0) {
        return aggregate(expr, func, args, scope, context);
    }
    let func = Scalar::named(&name.node.0).ok_or(RErrorKind::NotFound)?;
    let FunctionArgs::List {
        distinct: false,
        args,
    } = args
    else {
        return Err(RErrorKind::Type);
    };
    let arity_ok = if func == Scalar::Coalesce {
        !args.is_empty()
    } else {
        args.len() == 1
    };
    if !arity_ok {
        return Err(RErrorKind::Type);
    }
    let mut checked = Vec::with_capacity(args.len());
    for arg in args {
        checked.push(check(arg, scope, context)?);
    }
    let first = checked.first().map_or(SType::Null, |a| a.ty);
    let ty = match func {
        Scalar::Coalesce => {
            let mut ty = SType::Null;
            for arg in &checked {
                ty = ty.unify(arg.ty).ok_or(RErrorKind::Type)?;
            }
            ty
        }
        Scalar::Abs if first.numeric_or_null() => first,
        Scalar::Length if first.text_or_null() => SType::Integer,
        Scalar::Lower | Scalar::Upper if first.text_or_null() => SType::Text,
        _ => return Err(RErrorKind::Type),
    };
    Ok(Typed::new(Op::Call(func, checked), ty))
}

fn aggregate(
    expr: &Expr,
    func: Agg,
    args: &FunctionArgs,
    scope: &Scope,
    context: &mut Context,
) -> Result<Typed, RErrorKind> {
    let Context::Grouped(groups) = context else {
        return Err(RErrorKind::Type);
    };
    let (arg, distinct) = match args {
        FunctionArgs::Star if func == Agg::Count => (None, false),
        FunctionArgs::Star => return Err(RErrorKind::Type),
        FunctionArgs::List { distinct, args } => {
            let [arg] = args.as_slice() else {
                return Err(RErrorKind::Type);
            };
            (
                Some(check(arg, scope, &mut Context::InAggregate)?),
                *distinct,
            )
        }
    };
    let arg_ty = arg.as_ref().map_or(SType::Null, |a| a.ty);
    let ty = match func {
        Agg::Count => SType::Integer,
        Agg::Avg => SType::Real,
        Agg::Sum | Agg::Min | Agg::Max => arg_ty,
    };
    if matches!(func, Agg::Sum | Agg::Avg) && !arg_ty.numeric_or_null() {
        return Err(RErrorKind::Type);
    }
    let slot = match groups.aggs.iter().position(|a| a.ast == *expr) {
        Some(slot) => slot,
        None => {
            groups.aggs.push(AggCall {
                func,
                arg,
                distinct,
                ast: expr.clone(),
            });
            groups.aggs.len() - 1
        }
    };
    Ok(Typed::new(Op::Column(groups.keys.len() + slot), ty))
}

/// Evaluates a condition: only TRUE passes.
pub(crate) fn passes(expr: &Typed, row: &[RValue]) -> Result<bool, RErrorKind> {
    Ok(eval(expr, row)? == RValue::Boolean(true))
}

pub(crate) fn eval(expr: &Typed, row: &[RValue]) -> Result<RValue, RErrorKind> {
    let value = match &expr.op {
        Op::Const(value) => value.clone(),
        Op::Column(i) => row.get(*i).cloned().unwrap_or(RValue::Null),
        Op::Neg(a) => match eval(a, row)? {
            RValue::Integer(v) => RValue::Integer(v.checked_neg().ok_or(RErrorKind::Arithmetic)?),
            RValue::Real(v) => RValue::real(-v),
            _ => RValue::Null,
        },
        Op::Not(a) => from_truth(truth(&eval(a, row)?).map(|b| !b)),
        Op::And(a, b) => {
            let left = truth(&eval(a, row)?);
            if left == Some(false) {
                RValue::Boolean(false)
            } else {
                from_truth(and3(left, truth(&eval(b, row)?)))
            }
        }
        Op::Or(a, b) => {
            let left = truth(&eval(a, row)?);
            if left == Some(true) {
                RValue::Boolean(true)
            } else {
                from_truth(or3(left, truth(&eval(b, row)?)))
            }
        }
        Op::Compare(cmp, a, b) => {
            let left = eval(a, row)?;
            from_truth(compare(*cmp, &left, &eval(b, row)?))
        }
        Op::Arith(op, a, b) => {
            let left = eval(a, row)?;
            arith(*op, left, eval(b, row)?)?
        }
        Op::Concat(a, b) => match (eval(a, row)?, eval(b, row)?) {
            (RValue::Text(x), RValue::Text(y)) => RValue::Text(x + &y),
            _ => RValue::Null,
        },
        Op::IsNull(a, negated) => RValue::Boolean(eval(a, row)?.is_null() != *negated),
        Op::Between(a, low, high, negated) => {
            let value = eval(a, row)?;
            let low = eval(low, row)?;
            let high = eval(high, row)?;
            let inside = and3(
                compare(Cmp::Ge, &value, &low),
                compare(Cmp::Le, &value, &high),
            );
            from_truth(if *negated { inside.map(|b| !b) } else { inside })
        }
        Op::In(a, list, negated) => {
            let value = eval(a, row)?;
            let mut unknown = value.is_null();
            let mut found = false;
            for item in list {
                match compare(Cmp::Eq, &value, &eval(item, row)?) {
                    Some(true) => {
                        found = true;
                        break;
                    }
                    Some(false) => {}
                    None => unknown = true,
                }
            }
            let result = if found {
                Some(true)
            } else if unknown {
                None
            } else {
                Some(false)
            };
            from_truth(if *negated { result.map(|b| !b) } else { result })
        }
        Op::Like(a, p, negated) => match (eval(a, row)?, eval(p, row)?) {
            (RValue::Text(text), RValue::Text(pattern)) => {
                RValue::Boolean(like(&text, &pattern) != *negated)
            }
            _ => RValue::Null,
        },
        Op::Case(branches, otherwise) => {
            let mut chosen = None;
            for (when, then) in branches {
                if passes(when, row)? {
                    chosen = Some(eval(then, row)?);
                    break;
                }
            }
            match (chosen, otherwise) {
                (Some(value), _) => value,
                (None, Some(e)) => eval(e, row)?,
                (None, None) => RValue::Null,
            }
        }
        Op::Call(Scalar::Coalesce, args) => {
            let mut result = RValue::Null;
            for arg in args {
                let value = eval(arg, row)?;
                if !value.is_null() {
                    result = value;
                    break;
                }
            }
            result
        }
        Op::Call(func, args) => {
            let value = match args.first() {
                Some(arg) => eval(arg, row)?,
                None => RValue::Null,
            };
            scalar(*func, value)?
        }
    };
    Ok(fit(value, expr.ty))
}

/// An INTEGER result of a REAL-typed expression (CASE or COALESCE over
/// mixed numbers) becomes REAL.
pub(crate) fn fit(value: RValue, ty: SType) -> RValue {
    match (value, ty) {
        (RValue::Integer(v), SType::Real) => RValue::real(v as f64),
        (value, _) => value,
    }
}

fn scalar(func: Scalar, value: RValue) -> Result<RValue, RErrorKind> {
    Ok(match (func, value) {
        (_, RValue::Null) => RValue::Null,
        (Scalar::Lower, RValue::Text(t)) => RValue::Text(t.to_lowercase()),
        (Scalar::Upper, RValue::Text(t)) => RValue::Text(t.to_uppercase()),
        (Scalar::Length, RValue::Text(t)) => {
            RValue::Integer(i64::try_from(t.chars().count()).unwrap_or(i64::MAX))
        }
        (Scalar::Abs, RValue::Integer(v)) => {
            RValue::Integer(v.checked_abs().ok_or(RErrorKind::Arithmetic)?)
        }
        (Scalar::Abs, RValue::Real(v)) => RValue::real(v.abs()),
        _ => return Err(RErrorKind::Type),
    })
}

fn truth(value: &RValue) -> Option<bool> {
    match value {
        RValue::Boolean(b) => Some(*b),
        _ => None,
    }
}

fn from_truth(truth: Option<bool>) -> RValue {
    truth.map_or(RValue::Null, RValue::Boolean)
}

fn and3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

fn or3(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

/// A comparison in three-valued logic: unknown when either side is NULL.
pub(crate) fn compare(cmp: Cmp, left: &RValue, right: &RValue) -> Option<bool> {
    if left.is_null() || right.is_null() {
        return None;
    }
    let ordering = order(left, right);
    Some(match cmp {
        Cmp::Eq => ordering.is_eq(),
        Cmp::Ne => ordering.is_ne(),
        Cmp::Lt => ordering.is_lt(),
        Cmp::Le => ordering.is_le(),
        Cmp::Gt => ordering.is_gt(),
        Cmp::Ge => ordering.is_ge(),
    })
}

fn arith(op: Arith, left: RValue, right: RValue) -> Result<RValue, RErrorKind> {
    if left.is_null() || right.is_null() {
        return Ok(RValue::Null);
    }
    if let (RValue::Integer(a), RValue::Integer(b)) = (&left, &right) {
        let (a, b) = (*a, *b);
        if matches!(op, Arith::Div | Arith::Rem) && b == 0 {
            return Err(RErrorKind::Arithmetic);
        }
        let result = match op {
            Arith::Add => a.checked_add(b),
            Arith::Sub => a.checked_sub(b),
            Arith::Mul => a.checked_mul(b),
            Arith::Div => a.checked_div(b),
            // The remainder of i64::MIN by -1 is 0 and fits.
            Arith::Rem => Some(if b == -1 { 0 } else { a % b }),
        };
        return result.map(RValue::Integer).ok_or(RErrorKind::Arithmetic);
    }
    let (Some(a), Some(b)) = (left.as_f64(), right.as_f64()) else {
        return Ok(RValue::Null);
    };
    if matches!(op, Arith::Div | Arith::Rem) && b == 0.0 {
        return Err(RErrorKind::Arithmetic);
    }
    let result = match op {
        Arith::Add => a + b,
        Arith::Sub => a - b,
        Arith::Mul => a * b,
        Arith::Div => a / b,
        Arith::Rem => a % b,
    };
    if result.is_finite() {
        Ok(RValue::real(result))
    } else {
        Err(RErrorKind::Arithmetic)
    }
}

/// README LIKE: `%` matches any run of characters, `_` exactly one, every
/// other character itself (case-sensitively); the whole text must match.
/// Dynamic programming over (pattern position, text position).
pub(crate) fn like(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let mut matches = vec![false; text.len() + 1];
    matches[0] = true;
    for p in pattern.chars() {
        let mut next = vec![false; text.len() + 1];
        match p {
            '%' => {
                let mut seen = false;
                for (i, slot) in next.iter_mut().enumerate() {
                    seen |= matches[i];
                    *slot = seen;
                }
            }
            _ => {
                for (i, c) in text.iter().enumerate() {
                    next[i + 1] = matches[i] && (p == '_' || p == *c);
                }
            }
        }
        matches = next;
    }
    matches[text.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_follows_the_readme_rules() {
        assert!(like("abc", "abc"));
        assert!(like("abc", "a%"));
        assert!(like("abc", "%c"));
        assert!(like("abc", "_b_"));
        assert!(like("", "%"));
        assert!(like("a%c", "a%c"));
        assert!(!like("abc", "A%"));
        assert!(!like("abc", "ab"));
        assert!(!like("", "_"));
        assert!(like("éé", "__"));
        assert!(like("mississippi", "%iss%ppi"));
        assert!(!like("mississippi", "%iss%ppx"));
    }

    #[test]
    fn integer_arithmetic_is_checked() {
        let int = RValue::Integer;
        assert_eq!(
            arith(Arith::Add, int(i64::MAX), int(1)),
            Err(RErrorKind::Arithmetic)
        );
        assert_eq!(
            arith(Arith::Div, int(i64::MIN), int(-1)),
            Err(RErrorKind::Arithmetic)
        );
        assert_eq!(arith(Arith::Rem, int(i64::MIN), int(-1)), Ok(int(0)));
        assert_eq!(arith(Arith::Rem, int(-7), int(2)), Ok(int(-1)));
        assert_eq!(arith(Arith::Div, int(-7), int(2)), Ok(int(-3)));
        assert_eq!(
            arith(Arith::Div, int(1), int(0)),
            Err(RErrorKind::Arithmetic)
        );
        assert_eq!(arith(Arith::Div, int(1), RValue::Null), Ok(RValue::Null));
        assert_eq!(
            arith(Arith::Mul, RValue::Real(1e300), RValue::Real(1e300)),
            Err(RErrorKind::Arithmetic)
        );
        assert_eq!(
            arith(Arith::Mul, RValue::Real(-0.5), int(0)),
            Ok(RValue::Real(0.0))
        );
    }
}
