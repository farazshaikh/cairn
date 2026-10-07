//! Evaluation of bound expressions over one input row, with SQL
//! three-valued logic. Integer arithmetic is checked: overflow and
//! division by zero are errors spanned to the operator, never panics.

use std::cmp::Ordering;

use crate::bind::{ArithOp, BoundExpr, BoundKind, CmpOp};
use crate::error::{ErrorKind, ExecError};
use crate::functions::{ScalarFn, apply_scalar, overflow, real_overflow};
use crate::like::like;
use crate::value::{SqlType, Value, cmp_values};

/// Evaluates a condition; only TRUE passes (FALSE and NULL do not).
pub fn eval_condition(expr: &BoundExpr, row: &[Value]) -> Result<bool, ExecError> {
    Ok(matches!(eval(expr, row)?, Value::Boolean(true)))
}

pub fn eval(expr: &BoundExpr, row: &[Value]) -> Result<Value, ExecError> {
    let value = match &expr.kind {
        BoundKind::Literal(value) => value.clone(),
        BoundKind::Column(i) => column(*i, row, expr)?,
        BoundKind::Neg(operand) => negate(eval(operand, row)?, expr)?,
        BoundKind::Not(operand) => not3(truth(&eval(operand, row)?)),
        BoundKind::And(left, right) => eval_and(left, right, row)?,
        BoundKind::Or(left, right) => eval_or(left, right, row)?,
        BoundKind::Compare(op, left, right) => {
            let l = eval(left, row)?;
            from_truth(compare(*op, &l, &eval(right, row)?))
        }
        BoundKind::Arith(op, left, right) => {
            let l = eval(left, row)?;
            arith(*op, l, eval(right, row)?, expr)?
        }
        BoundKind::Concat(left, right) => {
            let l = eval(left, row)?;
            concat(l, eval(right, row)?)
        }
        BoundKind::IsNull { operand, negated } => {
            Value::Boolean(eval(operand, row)?.is_null() != *negated)
        }
        BoundKind::Between {
            operand,
            low,
            high,
            negated,
        } => eval_between([operand, low, high], *negated, row)?,
        BoundKind::InList {
            operand,
            list,
            negated,
        } => eval_in_list(operand, list, *negated, row)?,
        BoundKind::Like {
            operand,
            pattern,
            negated,
        } => {
            let text = eval(operand, row)?;
            eval_like(text, eval(pattern, row)?, *negated)
        }
        BoundKind::Case {
            branches,
            else_result,
        } => eval_case(branches, else_result.as_deref(), row)?,
        BoundKind::Scalar(ScalarFn::Coalesce, args) => coalesce(args, row)?,
        BoundKind::Scalar(func, args) => {
            let arg = match args.first() {
                Some(arg) => eval(arg, row)?,
                None => Value::Null,
            };
            apply_scalar(*func, arg, expr.span)?
        }
    };
    Ok(widen(value, expr.ty))
}

fn column(i: usize, row: &[Value], expr: &BoundExpr) -> Result<Value, ExecError> {
    row.get(i).cloned().ok_or_else(|| {
        ExecError::at(
            ErrorKind::Corrupt,
            "column position out of range",
            expr.span,
        )
    })
}

fn negate(value: Value, expr: &BoundExpr) -> Result<Value, ExecError> {
    match value {
        Value::Integer(v) => v
            .checked_neg()
            .map(Value::Integer)
            .ok_or_else(|| overflow(expr.span)),
        Value::Real(v) => Ok(Value::real(-v)),
        _ => Ok(Value::Null),
    }
}

fn eval_and(left: &BoundExpr, right: &BoundExpr, row: &[Value]) -> Result<Value, ExecError> {
    let l = truth(&eval(left, row)?);
    if l == Some(false) {
        return Ok(Value::Boolean(false));
    }
    Ok(and3(l, truth(&eval(right, row)?)))
}

fn eval_or(left: &BoundExpr, right: &BoundExpr, row: &[Value]) -> Result<Value, ExecError> {
    let l = truth(&eval(left, row)?);
    if l == Some(true) {
        return Ok(Value::Boolean(true));
    }
    Ok(or3(l, truth(&eval(right, row)?)))
}

fn concat(left: Value, right: Value) -> Value {
    match (left, right) {
        (Value::Text(mut a), Value::Text(b)) => {
            a.push_str(&b);
            Value::Text(a)
        }
        _ => Value::Null,
    }
}

fn eval_between(
    [operand, low, high]: [&BoundExpr; 3],
    negated: bool,
    row: &[Value],
) -> Result<Value, ExecError> {
    let v = eval(operand, row)?;
    let lo = eval(low, row)?;
    let hi = eval(high, row)?;
    let inside = and3(compare(CmpOp::GtEq, &v, &lo), compare(CmpOp::LtEq, &v, &hi));
    Ok(negate_if(inside, negated))
}

fn eval_in_list(
    operand: &BoundExpr,
    list: &[BoundExpr],
    negated: bool,
    row: &[Value],
) -> Result<Value, ExecError> {
    let v = eval(operand, row)?;
    let mut saw_null = v.is_null();
    for item in list {
        match compare(CmpOp::Eq, &v, &eval(item, row)?) {
            Some(true) => return Ok(negate_if(Value::Boolean(true), negated)),
            Some(false) => {}
            None => saw_null = true,
        }
    }
    let result = if saw_null {
        Value::Null
    } else {
        Value::Boolean(false)
    };
    Ok(negate_if(result, negated))
}

fn eval_like(text: Value, pattern: Value, negated: bool) -> Value {
    match (text, pattern) {
        (Value::Text(text), Value::Text(pattern)) => {
            Value::Boolean(like(&text, &pattern) != negated)
        }
        _ => Value::Null,
    }
}

fn eval_case(
    branches: &[(BoundExpr, BoundExpr)],
    else_result: Option<&BoundExpr>,
    row: &[Value],
) -> Result<Value, ExecError> {
    for (condition, result) in branches {
        if eval_condition(condition, row)? {
            return eval(result, row);
        }
    }
    match else_result {
        Some(result) => eval(result, row),
        None => Ok(Value::Null),
    }
}

fn coalesce(args: &[BoundExpr], row: &[Value]) -> Result<Value, ExecError> {
    for arg in args {
        let value = eval(arg, row)?;
        if !value.is_null() {
            return Ok(value);
        }
    }
    Ok(Value::Null)
}

/// Integer results of a REAL-typed expression (a CASE or COALESCE mixing
/// INTEGER and REAL) become REAL.
fn widen(value: Value, ty: SqlType) -> Value {
    match (value, ty) {
        (Value::Integer(v), SqlType::Real) => Value::real(v as f64),
        (value, _) => value,
    }
}

fn truth(value: &Value) -> Option<bool> {
    match value {
        Value::Boolean(b) => Some(*b),
        _ => None,
    }
}

fn from_truth(truth: Option<bool>) -> Value {
    truth.map_or(Value::Null, Value::Boolean)
}

fn not3(value: Option<bool>) -> Value {
    from_truth(value.map(|b| !b))
}

fn and3(left: Option<bool>, right: Option<bool>) -> Value {
    match (left, right) {
        (Some(false), _) | (_, Some(false)) => Value::Boolean(false),
        (Some(true), Some(true)) => Value::Boolean(true),
        _ => Value::Null,
    }
}

fn or3(left: Option<bool>, right: Option<bool>) -> Value {
    match (left, right) {
        (Some(true), _) | (_, Some(true)) => Value::Boolean(true),
        (Some(false), Some(false)) => Value::Boolean(false),
        _ => Value::Null,
    }
}

fn negate_if(value: Value, negated: bool) -> Value {
    if negated { not3(truth(&value)) } else { value }
}

/// A comparison in three-valued logic: `None` when either side is NULL.
pub fn compare(op: CmpOp, left: &Value, right: &Value) -> Option<bool> {
    if left.is_null() || right.is_null() {
        return None;
    }
    let ordering = cmp_values(left, right);
    Some(match op {
        CmpOp::Eq => ordering == Ordering::Equal,
        CmpOp::NotEq => ordering != Ordering::Equal,
        CmpOp::Lt => ordering == Ordering::Less,
        CmpOp::LtEq => ordering != Ordering::Greater,
        CmpOp::Gt => ordering == Ordering::Greater,
        CmpOp::GtEq => ordering != Ordering::Less,
    })
}

fn division_by_zero(expr: &BoundExpr) -> ExecError {
    ExecError::at(ErrorKind::Arithmetic, "division by zero", expr.span)
}

fn arith(op: ArithOp, left: Value, right: Value, expr: &BoundExpr) -> Result<Value, ExecError> {
    let span = expr.span;
    match (left, right) {
        (Value::Integer(a), Value::Integer(b)) => {
            let result = match op {
                ArithOp::Add => a.checked_add(b),
                ArithOp::Sub => a.checked_sub(b),
                ArithOp::Mul => a.checked_mul(b),
                ArithOp::Div if b == 0 => return Err(division_by_zero(expr)),
                ArithOp::Div => a.checked_div(b),
                ArithOp::Mod if b == 0 => return Err(division_by_zero(expr)),
                ArithOp::Mod => Some(a.wrapping_rem(b)),
            };
            result.map(Value::Integer).ok_or_else(|| overflow(span))
        }
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (left, right) => {
            let (Some(a), Some(b)) = (as_real(&left), as_real(&right)) else {
                return Ok(Value::Null);
            };
            let result = match op {
                ArithOp::Add => a + b,
                ArithOp::Sub => a - b,
                ArithOp::Mul => a * b,
                ArithOp::Div if b == 0.0 => return Err(division_by_zero(expr)),
                ArithOp::Div => a / b,
                ArithOp::Mod if b == 0.0 => return Err(division_by_zero(expr)),
                ArithOp::Mod => a % b,
            };
            if !result.is_finite() {
                return Err(real_overflow(span));
            }
            Ok(Value::real(result))
        }
    }
}

fn as_real(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(v) => Some(*v as f64),
        Value::Real(v) => Some(*v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bind::{Mode, Scope, bind};

    fn scope() -> Scope {
        let mut scope = Scope::default();
        scope.push(
            "t".into(),
            vec![
                ("i".into(), SqlType::Integer),
                ("r".into(), SqlType::Real),
                ("s".into(), SqlType::Text),
                ("b".into(), SqlType::Boolean),
                ("n".into(), SqlType::Integer),
            ],
        );
        scope
    }

    fn row() -> Vec<Value> {
        vec![
            Value::Integer(7),
            Value::Real(2.5),
            Value::Text("héllo".into()),
            Value::Boolean(true),
            Value::Null,
        ]
    }

    fn run(sql: &str) -> Result<Value, ExecError> {
        let expr = cairn_sql::parse_expr(sql).expect("parse");
        let bound = bind(&expr, &scope(), &mut Mode::Plain("WHERE"))?;
        eval(&bound, &row())
    }

    fn value(sql: &str) -> Value {
        run(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    fn error(sql: &str) -> ExecError {
        match run(sql) {
            Ok(v) => panic!("{sql} gave {v:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn truth_tables() {
        let names = ["TRUE", "FALSE", "NULL"];
        let truth = [Some(true), Some(false), None];
        for (a, ta) in names.iter().zip(truth) {
            for (b, tb) in names.iter().zip(truth) {
                let expected_and = match (ta, tb) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                };
                let expected_or = match (ta, tb) {
                    (Some(true), _) | (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                };
                assert_eq!(
                    value(&format!("{a} AND {b}")),
                    from_truth(expected_and),
                    "{a} AND {b}"
                );
                assert_eq!(
                    value(&format!("{a} OR {b}")),
                    from_truth(expected_or),
                    "{a} OR {b}"
                );
            }
            assert_eq!(value(&format!("NOT {a}")), from_truth(ta.map(|t| !t)));
        }
    }

    #[test]
    fn null_propagates() {
        for sql in [
            "n = 1",
            "n < 1",
            "n + 1",
            "-n",
            "s || NULL",
            "n BETWEEN 1 AND 2",
            "s LIKE NULL",
            "n IN (1, 2)",
            "7 IN (1, NULL)",
            "LOWER(NULL)",
            "ABS(n)",
        ] {
            assert_eq!(value(sql), Value::Null, "{sql}");
        }
        assert_eq!(value("7 IN (7, NULL)"), Value::Boolean(true));
        assert_eq!(value("7 NOT IN (1, NULL)"), Value::Null);
        assert_eq!(value("n IS NULL"), Value::Boolean(true));
        assert_eq!(value("i IS NOT NULL"), Value::Boolean(true));
        assert_eq!(value("COALESCE(n, NULL, i, 3)"), Value::Integer(7));
        assert_eq!(value("COALESCE(n, 1.5)"), Value::Real(1.5));
        assert_eq!(value("COALESCE(i, 1.5)"), Value::Real(7.0));
    }

    #[test]
    fn arithmetic_rules() {
        assert_eq!(value("-7 / 2"), Value::Integer(-3));
        assert_eq!(value("-7 % 2"), Value::Integer(-1));
        assert_eq!(value("7 % -2"), Value::Integer(1));
        assert_eq!(value("i / 2.0"), Value::Real(3.5));
        assert_eq!(value("i + r"), Value::Real(9.5));
        assert_eq!(value("(-9223372036854775807 - 1) % -1"), Value::Integer(0));
        assert_eq!(value("i > r"), Value::Boolean(true));
        assert_eq!(value("1 = 1.0"), Value::Boolean(true));
    }

    #[test]
    fn arithmetic_errors_have_spans() {
        let cases = [
            ("9223372036854775807 + 1", "integer overflow", 1),
            ("i - 9223372036854775807 - 9", "integer overflow", 1),
            ("4611686018427387904 * 2", "integer overflow", 1),
            ("-(-9223372036854775807 - 1)", "integer overflow", 1),
            ("(-9223372036854775807 - 1) / -1", "integer overflow", 1),
            ("ABS(-9223372036854775807 - 1)", "integer overflow", 1),
            ("i / 0", "division by zero", 1),
            ("i % 0", "division by zero", 1),
            ("r / 0", "division by zero", 1),
            ("1e308 * 10", "real overflow", 1),
        ];
        for (sql, message, column) in cases {
            let e = error(sql);
            assert_eq!(
                (e.kind(), e.message()),
                (ErrorKind::Arithmetic, message),
                "{sql}"
            );
            assert_eq!(e.span().map(|s| s.column), Some(column), "{sql}");
        }
    }

    #[test]
    fn type_errors_are_reported_at_bind_time() {
        let cases = [
            ("s + 1", "type mismatch: cannot apply + to TEXT and INTEGER"),
            ("s = 1", "type mismatch: cannot compare TEXT with INTEGER"),
            ("r % 2", "type mismatch: cannot apply % to REAL and INTEGER"),
            ("-s", "type mismatch: cannot apply - to TEXT"),
            ("NOT i", "type mismatch: NOT needs BOOLEAN, found INTEGER"),
            ("i AND b", "type mismatch: AND needs BOOLEAN, found INTEGER"),
            (
                "i || s",
                "type mismatch: cannot apply || to INTEGER and TEXT",
            ),
            (
                "i LIKE 'x'",
                "type mismatch: cannot apply LIKE to INTEGER and TEXT",
            ),
            (
                "CASE WHEN b THEN 1 ELSE 'x' END",
                "type mismatch: CASE branches have types INTEGER and TEXT",
            ),
            (
                "CASE WHEN i THEN 1 END",
                "type mismatch: CASE condition needs BOOLEAN, found INTEGER",
            ),
            (
                "COALESCE(i, s)",
                "type mismatch: COALESCE arguments have types INTEGER and TEXT",
            ),
            ("LOWER(i)", "type mismatch: cannot apply LOWER to INTEGER"),
            (
                "b IN (1)",
                "type mismatch: cannot compare BOOLEAN with INTEGER",
            ),
            ("nope(1)", "no such function: nope"),
            (
                "LOWER(s, s)",
                "wrong number of arguments to LOWER: expected 1, found 2",
            ),
            (
                "COALESCE()",
                "wrong number of arguments to COALESCE: expected at least 1, found 0",
            ),
            ("LOWER(DISTINCT s)", "LOWER does not accept * or DISTINCT"),
            (
                "COUNT(*) > 1",
                "aggregate functions are not allowed in WHERE",
            ),
            ("zz", "no such column: zz"),
            ("u.i", "no such table or alias: u"),
            ("t.zz", "no such column: t.zz"),
        ];
        for (sql, message) in cases {
            assert_eq!(error(sql).message(), message, "{sql}");
        }
    }

    #[test]
    fn case_like_and_functions() {
        assert_eq!(
            value("CASE WHEN NULL THEN 1 WHEN b THEN 2 ELSE 3 END"),
            Value::Integer(2)
        );
        assert_eq!(value("CASE WHEN FALSE THEN 1 END"), Value::Null);
        assert_eq!(value("CASE WHEN b THEN 1 ELSE 2.5 END"), Value::Real(1.0));
        assert_eq!(value("s LIKE 'h_llo'"), Value::Boolean(true));
        assert_eq!(value("s LIKE 'H%'"), Value::Boolean(false));
        assert_eq!(value("s NOT LIKE '%x%'"), Value::Boolean(true));
        assert_eq!(value("UPPER(s)"), Value::Text("HÉLLO".into()));
        assert_eq!(value("LENGTH(s)"), Value::Integer(5));
        assert_eq!(value("ABS(-r)"), Value::Real(2.5));
        assert_eq!(value("i BETWEEN 7 AND 8"), Value::Boolean(true));
        assert_eq!(value("i NOT BETWEEN 1 AND 6"), Value::Boolean(true));
    }

    #[test]
    fn deep_nesting_evaluates_without_overflow() {
        let depth = cairn_sql::MAX_DEPTH - 1;
        let sql = format!("{}1{}", "(".repeat(depth), ")".repeat(depth));
        assert_eq!(value(&sql), Value::Integer(1));
        let chain = format!("1{}", " + 1".repeat(cairn_sql::MAX_DEPTH - 1));
        assert_eq!(
            value(&chain),
            Value::Integer(i64::try_from(cairn_sql::MAX_DEPTH).unwrap_or(0))
        );
        let negations = format!("{}1", "- ".repeat(depth));
        assert!(run(&negations).is_ok());
    }
}
