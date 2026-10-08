//! Values, static types and the single total order the README defines:
//! NULL first, numbers by exact value across INTEGER and REAL, text by
//! bytes, `FALSE < TRUE`.

use std::cmp::Ordering;
use std::fmt;

use cairn_sql::{DataType, Literal};

/// One SQL value. A `Real` is finite and never `-0.0`.
#[derive(Debug, Clone, PartialEq)]
pub enum RValue {
    /// SQL `NULL`.
    Null,
    /// A 64-bit integer.
    Integer(i64),
    /// A finite double, with `-0.0` stored as `0.0`.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// `TRUE` or `FALSE`.
    Boolean(bool),
}

impl RValue {
    /// A REAL value; `-0.0` becomes `0.0` (README "Types").
    pub fn real(value: f64) -> RValue {
        RValue::Real(if value == 0.0 { 0.0 } else { value })
    }

    /// Whether the value is `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, RValue::Null)
    }

    pub(crate) fn from_literal(literal: &Literal) -> RValue {
        match literal {
            Literal::Integer(v) => RValue::Integer(*v),
            Literal::Real(v) => RValue::real(*v),
            Literal::String(s) => RValue::Text(s.clone()),
            Literal::Boolean(b) => RValue::Boolean(*b),
            Literal::Null => RValue::Null,
        }
    }

    pub(crate) fn sql_type(&self) -> SType {
        match self {
            RValue::Null => SType::Null,
            RValue::Integer(_) => SType::Integer,
            RValue::Real(_) => SType::Real,
            RValue::Text(_) => SType::Text,
            RValue::Boolean(_) => SType::Boolean,
        }
    }

    /// The value as REAL when it is a number.
    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            RValue::Integer(v) => Some(*v as f64),
            RValue::Real(v) => Some(*v),
            _ => None,
        }
    }
}

/// Golden-format text: `NULL`, `TRUE`/`FALSE`, decimal integers, reals in
/// the cairn-sql canonical literal form, and raw text.
impl fmt::Display for RValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RValue::Null => f.write_str("NULL"),
            RValue::Integer(v) => write!(f, "{v}"),
            RValue::Real(v) => write!(f, "{}", Literal::Real(*v)),
            RValue::Text(text) => f.write_str(text),
            RValue::Boolean(true) => f.write_str("TRUE"),
            RValue::Boolean(false) => f.write_str("FALSE"),
        }
    }
}

/// The static type of an expression. `Null` is the type of the literal
/// `NULL` and fits anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SType {
    Integer,
    Real,
    Text,
    Boolean,
    Null,
}

impl SType {
    pub(crate) fn of(data_type: DataType) -> SType {
        match data_type {
            DataType::Integer => SType::Integer,
            DataType::Real => SType::Real,
            DataType::Text => SType::Text,
            DataType::Boolean => SType::Boolean,
        }
    }

    pub(crate) fn numeric(self) -> bool {
        matches!(self, SType::Integer | SType::Real)
    }

    pub(crate) fn numeric_or_null(self) -> bool {
        self.numeric() || self == SType::Null
    }

    pub(crate) fn text_or_null(self) -> bool {
        matches!(self, SType::Text | SType::Null)
    }

    /// Comparisons work between two numbers, two equal types, or with NULL.
    pub(crate) fn comparable(self, other: SType) -> bool {
        self == SType::Null
            || other == SType::Null
            || self == other
            || (self.numeric() && other.numeric())
    }

    /// The common type of CASE results or COALESCE arguments.
    pub(crate) fn unify(self, other: SType) -> Option<SType> {
        match (self, other) {
            (a, b) if a == b => Some(a),
            (SType::Null, b) => Some(b),
            (a, SType::Null) => Some(a),
            (a, b) if a.numeric() && b.numeric() => Some(SType::Real),
            _ => None,
        }
    }
}

/// The README total order. Values of different non-numeric types never
/// meet after type checking; they are ranked only to keep the order total.
pub(crate) fn order(left: &RValue, right: &RValue) -> Ordering {
    match (left, right) {
        (RValue::Null, RValue::Null) => Ordering::Equal,
        (RValue::Null, _) => Ordering::Less,
        (_, RValue::Null) => Ordering::Greater,
        (RValue::Integer(a), RValue::Integer(b)) => a.cmp(b),
        (RValue::Real(a), RValue::Real(b)) => a.total_cmp(b),
        (RValue::Integer(a), RValue::Real(b)) => int_vs_real(*a, *b),
        (RValue::Real(a), RValue::Integer(b)) => int_vs_real(*b, *a).reverse(),
        (RValue::Text(a), RValue::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
        (RValue::Boolean(a), RValue::Boolean(b)) => a.cmp(b),
        _ => rank(left).cmp(&rank(right)),
    }
}

fn rank(value: &RValue) -> u8 {
    match value {
        RValue::Null => 0,
        RValue::Integer(_) | RValue::Real(_) => 1,
        RValue::Text(_) => 2,
        RValue::Boolean(_) => 3,
    }
}

/// Exact comparison of an integer with a finite real, through `i128` and
/// the real's integer and fractional parts.
fn int_vs_real(int: i64, real: f64) -> Ordering {
    let whole = real.floor();
    if whole >= 9.3e18 {
        return Ordering::Less;
    }
    if whole < -9.3e18 {
        return Ordering::Greater;
    }
    match i128::from(int).cmp(&(whole as i128)) {
        Ordering::Equal if real > whole => Ordering::Less,
        other => other,
    }
}

/// Lexicographic order of two rows of values.
pub(crate) fn order_rows(left: &[RValue], right: &[RValue]) -> Ordering {
    for (a, b) in left.iter().zip(right) {
        let ordering = order(a, b);
        if ordering.is_ne() {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_and_reals_compare_exactly() {
        let big = (1_i64 << 53) + 1;
        assert_eq!(
            order(&RValue::Integer(big), &RValue::Real((1_i64 << 53) as f64)),
            Ordering::Greater
        );
        assert_eq!(
            order(
                &RValue::Integer(i64::MAX),
                &RValue::Real(9_223_372_036_854_775_808.0)
            ),
            Ordering::Less
        );
        assert_eq!(
            order(
                &RValue::Integer(i64::MIN),
                &RValue::Real(-9_223_372_036_854_775_808.0)
            ),
            Ordering::Equal
        );
        assert_eq!(
            order(&RValue::Integer(1), &RValue::Real(1.5)),
            Ordering::Less
        );
        assert_eq!(
            order(&RValue::Integer(-2), &RValue::Real(-1.5)),
            Ordering::Less
        );
        assert_eq!(
            order(&RValue::Integer(-1), &RValue::Real(-1.5)),
            Ordering::Greater
        );
        assert_eq!(
            order(&RValue::Integer(2), &RValue::Real(2.0)),
            Ordering::Equal
        );
    }

    #[test]
    fn null_sorts_first_and_reals_print_canonically() {
        assert_eq!(
            order(&RValue::Null, &RValue::Integer(i64::MIN)),
            Ordering::Less
        );
        assert_eq!(RValue::real(-0.0).to_string(), "0.0");
        assert_eq!(RValue::Real(1e300).to_string(), "1e300");
        assert_eq!(RValue::Real(0.1).to_string(), "0.1");
    }
}
