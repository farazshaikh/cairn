//! Scalar functions and aggregate accumulators.

use std::collections::BTreeSet;

use cairn_sql::Span;

use crate::error::{ErrorKind, ExecError};
use crate::value::{OrdRow, Value, cmp_values};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScalarFn {
    Lower,
    Upper,
    Length,
    Abs,
    Coalesce,
}

impl ScalarFn {
    pub fn from_name(name: &str) -> Option<ScalarFn> {
        match name.to_ascii_lowercase().as_str() {
            "lower" => Some(ScalarFn::Lower),
            "upper" => Some(ScalarFn::Upper),
            "length" => Some(ScalarFn::Length),
            "abs" => Some(ScalarFn::Abs),
            "coalesce" => Some(ScalarFn::Coalesce),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFn {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl AggFn {
    pub fn from_name(name: &str) -> Option<AggFn> {
        match name.to_ascii_lowercase().as_str() {
            "count" => Some(AggFn::Count),
            "sum" => Some(AggFn::Sum),
            "avg" => Some(AggFn::Avg),
            "min" => Some(AggFn::Min),
            "max" => Some(AggFn::Max),
            _ => None,
        }
    }
}

pub fn overflow(span: Span) -> ExecError {
    ExecError::at(ErrorKind::Arithmetic, "integer overflow", span)
}

pub fn real_overflow(span: Span) -> ExecError {
    ExecError::at(ErrorKind::Arithmetic, "real overflow", span)
}

/// Applies a one-argument scalar function to a non-NULL value. The binder
/// has already checked the argument type.
pub fn apply_scalar(func: ScalarFn, value: Value, span: Span) -> Result<Value, ExecError> {
    match (func, value) {
        (ScalarFn::Lower, Value::Text(text)) => Ok(Value::Text(text.to_lowercase())),
        (ScalarFn::Upper, Value::Text(text)) => Ok(Value::Text(text.to_uppercase())),
        (ScalarFn::Length, Value::Text(text)) => Ok(Value::Integer(
            i64::try_from(text.chars().count()).unwrap_or(i64::MAX),
        )),
        (ScalarFn::Abs, Value::Integer(v)) => v
            .checked_abs()
            .map(Value::Integer)
            .ok_or_else(|| overflow(span)),
        (ScalarFn::Abs, Value::Real(v)) => Ok(Value::real(v.abs())),
        (_, Value::Null) => Ok(Value::Null),
        (_, other) => Err(ExecError::at(
            ErrorKind::Type,
            format!("type mismatch: unexpected {} argument", other.sql_type()),
            span,
        )),
    }
}

/// Folds the values of one aggregate call within one group. NULL inputs
/// are skipped; `DISTINCT` drops repeated values before folding.
#[derive(Debug)]
pub struct Accumulator {
    func: AggFn,
    real: bool,
    seen: Option<BTreeSet<OrdRow>>,
    count: i64,
    /// Exact integer total for SUM and AVG; SUM checks that it fits `i64`
    /// only at the end, so the row order cannot cause an overflow.
    wide_sum: i128,
    real_sum: f64,
    best: Option<Value>,
    span: Span,
}

impl Accumulator {
    /// `real` is whether the argument's static type is REAL.
    pub fn new(func: AggFn, distinct: bool, real: bool, span: Span) -> Accumulator {
        Accumulator {
            func,
            real,
            seen: distinct.then(BTreeSet::new),
            count: 0,
            wide_sum: 0,
            real_sum: 0.0,
            best: None,
            span,
        }
    }

    /// Adds one input; `None` is a `COUNT(*)` row.
    pub fn add(&mut self, value: Option<Value>) -> Result<(), ExecError> {
        let Some(value) = value else {
            self.count = self
                .count
                .checked_add(1)
                .ok_or_else(|| overflow(self.span))?;
            return Ok(());
        };
        if value.is_null() {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen
            && !seen.insert(OrdRow(vec![value.clone()]))
        {
            return Ok(());
        }
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| overflow(self.span))?;
        match self.func {
            AggFn::Count => {}
            AggFn::Sum | AggFn::Avg => self.add_number(&value)?,
            AggFn::Min | AggFn::Max => {
                let replace = self.best.as_ref().is_none_or(|best| {
                    let ordering = cmp_values(&value, best);
                    if self.func == AggFn::Min {
                        ordering.is_lt()
                    } else {
                        ordering.is_gt()
                    }
                });
                if replace {
                    self.best = Some(value);
                }
            }
        }
        Ok(())
    }

    fn add_number(&mut self, value: &Value) -> Result<(), ExecError> {
        let number = match value {
            Value::Integer(v) if !self.real => {
                self.wide_sum = self
                    .wide_sum
                    .checked_add(i128::from(*v))
                    .ok_or_else(|| overflow(self.span))?;
                return Ok(());
            }
            Value::Integer(v) => *v as f64,
            Value::Real(v) => *v,
            _ => return Ok(()),
        };
        self.real_sum += number;
        if !self.real_sum.is_finite() {
            return Err(real_overflow(self.span));
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Value, ExecError> {
        if self.func == AggFn::Count {
            return Ok(Value::Integer(self.count));
        }
        if self.count == 0 {
            return Ok(Value::Null);
        }
        match self.func {
            AggFn::Sum if self.real => Ok(Value::real(self.real_sum)),
            AggFn::Sum => i64::try_from(self.wide_sum)
                .map(Value::Integer)
                .map_err(|_| overflow(self.span)),
            AggFn::Avg => {
                let total = if self.real {
                    self.real_sum
                } else {
                    self.wide_sum as f64
                };
                Ok(Value::real(total / self.count as f64))
            }
            _ => Ok(self.best.unwrap_or(Value::Null)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> Span {
        Span {
            start: 0,
            end: 1,
            line: 1,
            column: 1,
        }
    }

    fn fold(func: AggFn, distinct: bool, real: bool, values: &[Value]) -> Result<Value, ExecError> {
        let mut acc = Accumulator::new(func, distinct, real, span());
        for value in values {
            acc.add(Some(value.clone()))?;
        }
        acc.finish()
    }

    #[test]
    fn aggregates_skip_nulls() {
        let values = [
            Value::Integer(3),
            Value::Null,
            Value::Integer(1),
            Value::Integer(3),
        ];
        assert_eq!(
            fold(AggFn::Count, false, false, &values).ok(),
            Some(Value::Integer(3))
        );
        assert_eq!(
            fold(AggFn::Count, true, false, &values).ok(),
            Some(Value::Integer(2))
        );
        assert_eq!(
            fold(AggFn::Sum, false, false, &values).ok(),
            Some(Value::Integer(7))
        );
        assert_eq!(
            fold(AggFn::Sum, true, false, &values).ok(),
            Some(Value::Integer(4))
        );
        assert_eq!(
            fold(AggFn::Min, false, false, &values).ok(),
            Some(Value::Integer(1))
        );
        assert_eq!(
            fold(AggFn::Max, false, false, &values).ok(),
            Some(Value::Integer(3))
        );
        let avg = fold(AggFn::Avg, false, false, &values).ok();
        assert_eq!(avg, Some(Value::Real(7.0 / 3.0)));
    }

    #[test]
    fn empty_aggregates() {
        assert_eq!(
            fold(AggFn::Count, false, false, &[Value::Null]).ok(),
            Some(Value::Integer(0))
        );
        for func in [AggFn::Sum, AggFn::Avg, AggFn::Min, AggFn::Max] {
            assert_eq!(fold(func, false, false, &[]).ok(), Some(Value::Null));
        }
    }

    #[test]
    fn sum_overflow_is_an_error() {
        let values = [Value::Integer(i64::MAX), Value::Integer(1)];
        let error = fold(AggFn::Sum, false, false, &values).expect_err("overflow");
        assert_eq!(error.message(), "integer overflow");
        let average = fold(AggFn::Avg, false, false, &values);
        assert!(average.is_ok());
        let reals = [Value::Real(f64::MAX), Value::Real(f64::MAX)];
        assert_eq!(
            fold(AggFn::Sum, false, true, &reals)
                .expect_err("overflow")
                .message(),
            "real overflow"
        );
    }

    #[test]
    fn scalar_functions() {
        let s = span();
        assert_eq!(
            apply_scalar(ScalarFn::Lower, Value::Text("ÀB".into()), s).ok(),
            Some(Value::Text("àb".into()))
        );
        assert_eq!(
            apply_scalar(ScalarFn::Length, Value::Text("é世".into()), s).ok(),
            Some(Value::Integer(2))
        );
        assert_eq!(
            apply_scalar(ScalarFn::Abs, Value::Integer(-3), s).ok(),
            Some(Value::Integer(3))
        );
        assert!(apply_scalar(ScalarFn::Abs, Value::Integer(i64::MIN), s).is_err());
        assert_eq!(
            apply_scalar(ScalarFn::Upper, Value::Null, s).ok(),
            Some(Value::Null)
        );
    }
}
