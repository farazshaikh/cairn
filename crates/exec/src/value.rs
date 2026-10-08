//! SQL values, static types and the total order shared by sorting,
//! grouping, `DISTINCT`, `MIN`/`MAX` and the index key codec.

use std::cmp::Ordering;
use std::fmt;

use cairn_sql::DataType;

/// One SQL value. A `Real` is always finite and never `-0.0`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// A 64-bit integer.
    Integer(i64),
    /// A finite double; never `-0.0`.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// `TRUE` or `FALSE`.
    Boolean(bool),
}

impl Value {
    /// Builds a `Real`, normalising `-0.0` to `0.0`. Callers guarantee the
    /// value is finite.
    pub fn real(value: f64) -> Value {
        if value == 0.0 {
            Value::Real(0.0)
        } else {
            Value::Real(value)
        }
    }

    /// Whether the value is `NULL`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The type of the value; `SqlType::Null` for `NULL`.
    pub fn sql_type(&self) -> SqlType {
        match self {
            Value::Null => SqlType::Null,
            Value::Integer(_) => SqlType::Integer,
            Value::Real(_) => SqlType::Real,
            Value::Text(_) => SqlType::Text,
            Value::Boolean(_) => SqlType::Boolean,
        }
    }

    /// The value written as a SQL literal, as `EXPLAIN` prints constants.
    pub fn to_sql_literal(&self) -> String {
        match self {
            Value::Text(text) => format!("'{}'", text.replace('\'', "''")),
            other => other.to_string(),
        }
    }
}

/// Plain rendering: `NULL`, decimal integers, reals in the cairn-sql
/// canonical form (always with a `.` or exponent), raw text, `TRUE`/`FALSE`.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Integer(value) => write!(f, "{value}"),
            Value::Real(value) => {
                let text = format!("{value:?}");
                f.write_str(&text)?;
                if !text.contains(['.', 'e', 'E']) {
                    f.write_str(".0")?;
                }
                Ok(())
            }
            Value::Text(text) => f.write_str(text),
            Value::Boolean(true) => f.write_str("TRUE"),
            Value::Boolean(false) => f.write_str("FALSE"),
        }
    }
}

/// The static type of an expression or column. `Null` is the type of the
/// literal `NULL` and is compatible with every other type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlType {
    Integer,
    Real,
    Text,
    Boolean,
    Null,
}

impl SqlType {
    pub fn from_data_type(data_type: DataType) -> SqlType {
        match data_type {
            DataType::Integer => SqlType::Integer,
            DataType::Real => SqlType::Real,
            DataType::Text => SqlType::Text,
            DataType::Boolean => SqlType::Boolean,
        }
    }

    /// The declared column type this type came from; `None` for `Null`,
    /// which no column has.
    pub fn data_type(self) -> Option<DataType> {
        match self {
            SqlType::Integer => Some(DataType::Integer),
            SqlType::Real => Some(DataType::Real),
            SqlType::Text => Some(DataType::Text),
            SqlType::Boolean => Some(DataType::Boolean),
            SqlType::Null => None,
        }
    }

    pub fn is_numeric(self) -> bool {
        matches!(self, SqlType::Integer | SqlType::Real)
    }

    /// Whether `=` and `<` may compare values of the two types.
    pub fn comparable(self, other: SqlType) -> bool {
        self == SqlType::Null
            || other == SqlType::Null
            || self == other
            || (self.is_numeric() && other.is_numeric())
    }

    /// The common type of CASE branches or COALESCE arguments.
    pub fn unify(self, other: SqlType) -> Option<SqlType> {
        match (self, other) {
            (a, b) if a == b => Some(a),
            (SqlType::Null, b) => Some(b),
            (a, SqlType::Null) => Some(a),
            (a, b) if a.is_numeric() && b.is_numeric() => Some(SqlType::Real),
            _ => None,
        }
    }
}

impl fmt::Display for SqlType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SqlType::Integer => "INTEGER",
            SqlType::Real => "REAL",
            SqlType::Text => "TEXT",
            SqlType::Boolean => "BOOLEAN",
            SqlType::Null => "NULL",
        })
    }
}

/// The total order: NULL first (NULLs equal each other), numbers compared
/// exactly across INTEGER and REAL, text by bytes, `FALSE < TRUE`. Mixed
/// non-numeric types, which typing keeps apart, order NULL < numeric <
/// TEXT < BOOLEAN.
pub fn cmp_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Integer(a), Value::Integer(b)) => a.cmp(b),
        (Value::Real(a), Value::Real(b)) => a.total_cmp(b),
        (Value::Integer(a), Value::Real(b)) => cmp_int_real(*a, *b),
        (Value::Real(a), Value::Integer(b)) => cmp_int_real(*b, *a).reverse(),
        (Value::Text(a), Value::Text(b)) => a.as_bytes().cmp(b.as_bytes()),
        (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
        _ => type_rank(left).cmp(&type_rank(right)),
    }
}

fn type_rank(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Integer(_) | Value::Real(_) => 1,
        Value::Text(_) => 2,
        Value::Boolean(_) => 3,
    }
}

const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;

/// Compares an integer with a finite real without losing precision.
fn cmp_int_real(int: i64, real: f64) -> Ordering {
    if real >= TWO_POW_63 {
        return Ordering::Less;
    }
    if real < -TWO_POW_63 {
        return Ordering::Greater;
    }
    let truncated = real.trunc() as i64;
    match int.cmp(&truncated) {
        Ordering::Equal => (truncated as f64).total_cmp(&real),
        other => other,
    }
}

/// A row ordered lexicographically by [`cmp_values`], for sorted maps and
/// sets in grouping and `DISTINCT`.
#[derive(Debug, Clone)]
pub struct OrdRow(pub Vec<Value>);

impl PartialEq for OrdRow {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for OrdRow {}

impl PartialOrd for OrdRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrdRow {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp_rows(&self.0, &other.0)
    }
}

pub fn cmp_rows(left: &[Value], right: &[Value]) -> Ordering {
    for (a, b) in left.iter().zip(right) {
        let ordering = cmp_values(a, b);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_sorts_first_and_equals_null() {
        assert_eq!(
            cmp_values(&Value::Null, &Value::Integer(i64::MIN)),
            Ordering::Less
        );
        assert_eq!(cmp_values(&Value::Null, &Value::Null), Ordering::Equal);
    }

    #[test]
    fn integers_and_reals_compare_exactly() {
        let big = 9_007_199_254_740_993_i64; // 2^53 + 1, not representable
        assert_eq!(
            cmp_values(&Value::Integer(big), &Value::Real(9_007_199_254_740_992.0)),
            Ordering::Greater
        );
        assert_eq!(
            cmp_values(&Value::Integer(i64::MAX), &Value::Real(TWO_POW_63)),
            Ordering::Less
        );
        assert_eq!(
            cmp_values(&Value::Integer(i64::MIN), &Value::Real(-TWO_POW_63)),
            Ordering::Equal
        );
        assert_eq!(
            cmp_values(&Value::Integer(i64::MIN), &Value::Real(-1e300)),
            Ordering::Greater
        );
        assert_eq!(
            cmp_values(&Value::Integer(1), &Value::Real(1.5)),
            Ordering::Less
        );
        assert_eq!(
            cmp_values(&Value::Real(-1.5), &Value::Integer(-1)),
            Ordering::Less
        );
        assert_eq!(
            cmp_values(&Value::Integer(2), &Value::Real(2.0)),
            Ordering::Equal
        );
    }

    #[test]
    fn display_matches_the_canonical_real_form() {
        assert_eq!(Value::Real(1.0).to_string(), "1.0");
        assert_eq!(Value::Real(0.1).to_string(), "0.1");
        assert_eq!(Value::Real(1e300).to_string(), "1e300");
        assert_eq!(Value::real(-0.0).to_string(), "0.0");
        assert_eq!(Value::Text("it's".into()).to_sql_literal(), "'it''s'");
    }

    #[test]
    fn unify_widens_numbers_only() {
        assert_eq!(SqlType::Integer.unify(SqlType::Real), Some(SqlType::Real));
        assert_eq!(SqlType::Null.unify(SqlType::Text), Some(SqlType::Text));
        assert_eq!(SqlType::Text.unify(SqlType::Integer), None);
    }
}
