//! Normalised statement outcomes and the differential comparison rules.
//!
//! - Rows: column names exactly; without ORDER BY the rows as a multiset;
//!   with ORDER BY the runs of rows with equal sort keys in order, each run
//!   as a multiset (row order among ties is unspecified).
//! - Values exactly, REALs by their bits.
//! - `affected N` exactly.
//! - Errors by kind, unless the statement has more than one possible
//!   runtime error, when only "both failed" is required. Messages and
//!   positions are never compared.

use cairn_exec::{ErrorKind, ExecError, QueryResult, Value};
use cairn_reference::{Outcome, RErrorKind, RValue};

/// A value in comparable form.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum NValue {
    /// `NULL`.
    Null,
    /// An INTEGER.
    Int(i64),
    /// A REAL, by its bits.
    Real(u64),
    /// TEXT.
    Text(String),
    /// A BOOLEAN.
    Bool(bool),
}

/// One statement's outcome in comparable form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Norm {
    /// Rows with column names.
    Rows {
        /// Output column names.
        columns: Vec<String>,
        /// Output rows.
        rows: Vec<Vec<NValue>>,
    },
    /// An affected-row count.
    Affected(u64),
    /// An error, by kind name.
    Error(String),
    /// An EXPLAIN or PRAGMA, whose rows only cairn produces.
    Skipped,
}

fn real_bits(v: f64) -> u64 {
    if v == 0.0 {
        0.0_f64.to_bits()
    } else {
        v.to_bits()
    }
}

fn from_value(value: &Value) -> NValue {
    match value {
        Value::Null => NValue::Null,
        Value::Integer(v) => NValue::Int(*v),
        Value::Real(v) => NValue::Real(real_bits(*v)),
        Value::Text(t) => NValue::Text(t.clone()),
        Value::Boolean(b) => NValue::Bool(*b),
    }
}

fn from_rvalue(value: &RValue) -> NValue {
    match value {
        RValue::Null => NValue::Null,
        RValue::Integer(v) => NValue::Int(*v),
        RValue::Real(v) => NValue::Real(real_bits(*v)),
        RValue::Text(t) => NValue::Text(t.clone()),
        RValue::Boolean(b) => NValue::Bool(*b),
    }
}

/// The name of a cairn error kind.
pub fn kind_name(kind: ErrorKind) -> String {
    format!("{kind:?}")
}

/// The name of a reference error kind; the names match cairn's.
pub fn reference_kind_name(kind: RErrorKind) -> String {
    format!("{kind:?}")
}

/// cairn's outcomes of one `execute_each` call.
pub fn from_cairn(outcome: &Result<Vec<Result<QueryResult, ExecError>>, ExecError>) -> Vec<Norm> {
    match outcome {
        Err(error) => vec![Norm::Error(kind_name(error.kind()))],
        Ok(results) => results
            .iter()
            .map(|result| match result {
                Ok(QueryResult::Rows { columns, rows }) => Norm::Rows {
                    columns: columns.clone(),
                    rows: rows
                        .iter()
                        .map(|row| row.iter().map(from_value).collect())
                        .collect(),
                },
                Ok(QueryResult::Affected(n)) => Norm::Affected(*n),
                Err(error) => Norm::Error(kind_name(error.kind())),
            })
            .collect(),
    }
}

/// The reference's outcomes of one `run_script` call.
pub fn from_reference(outcome: &Result<Vec<Outcome>, RErrorKind>) -> Vec<Norm> {
    match outcome {
        Err(kind) => vec![Norm::Error(reference_kind_name(*kind))],
        Ok(outcomes) => outcomes
            .iter()
            .map(|outcome| match outcome {
                Outcome::Rows { columns, rows } => Norm::Rows {
                    columns: columns.clone(),
                    rows: rows
                        .iter()
                        .map(|row| row.iter().map(from_rvalue).collect())
                        .collect(),
                },
                Outcome::Affected(n) => Norm::Affected(*n),
                Outcome::Error(kind) => Norm::Error(reference_kind_name(*kind)),
                Outcome::Skipped => Norm::Skipped,
            })
            .collect(),
    }
}

/// Compares two outcomes. `order_keys` are the output positions ORDER BY
/// sorts on, if any.
pub fn equal(
    cairn: &Norm,
    reference: &Norm,
    order_keys: Option<&[usize]>,
    multi_error: bool,
) -> Result<(), String> {
    let differ = || Err(format!("cairn: {cairn:?}\nreference: {reference:?}"));
    match (cairn, reference) {
        (Norm::Error(a), Norm::Error(b)) => {
            if a == b || multi_error {
                Ok(())
            } else {
                differ()
            }
        }
        (
            Norm::Rows {
                columns: ca,
                rows: ra,
            },
            Norm::Rows {
                columns: cb,
                rows: rb,
            },
        ) => {
            if ca != cb || ra.len() != rb.len() {
                return differ();
            }
            let same = match order_keys {
                None => sorted(ra) == sorted(rb),
                Some(keys) => {
                    let (ga, gb) = (tie_groups(ra, keys), tie_groups(rb, keys));
                    ga.len() == gb.len()
                        && ga
                            .iter()
                            .zip(&gb)
                            .all(|(a, b)| a.0 == b.0 && sorted(&a.1) == sorted(&b.1))
                }
            };
            if same { Ok(()) } else { differ() }
        }
        (a, b) if a == b => Ok(()),
        _ => differ(),
    }
}

fn sorted(rows: &[Vec<NValue>]) -> Vec<Vec<NValue>> {
    let mut rows = rows.to_vec();
    rows.sort();
    rows
}

type TieGroup = (Vec<NValue>, Vec<Vec<NValue>>);

/// Maximal runs of consecutive rows with equal values at `keys`.
fn tie_groups(rows: &[Vec<NValue>], keys: &[usize]) -> Vec<TieGroup> {
    let mut groups: Vec<TieGroup> = Vec::new();
    for row in rows {
        let key: Vec<NValue> = keys
            .iter()
            .map(|&k| row.get(k).cloned().unwrap_or(NValue::Null))
            .collect();
        match groups.last_mut() {
            Some((last, members)) if *last == key => members.push(row.clone()),
            _ => groups.push((key, vec![row.clone()])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(values: &[&[i64]]) -> Norm {
        Norm::Rows {
            columns: vec!["a".into(), "b".into()],
            rows: values
                .iter()
                .map(|r| r.iter().map(|v| NValue::Int(*v)).collect())
                .collect(),
        }
    }

    #[test]
    fn unordered_rows_compare_as_a_multiset() {
        assert!(
            equal(
                &rows(&[&[1, 2], &[3, 4]]),
                &rows(&[&[3, 4], &[1, 2]]),
                None,
                false
            )
            .is_ok()
        );
        assert!(
            equal(
                &rows(&[&[1, 2], &[1, 2]]),
                &rows(&[&[1, 2], &[3, 4]]),
                None,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn ordered_rows_may_differ_only_within_ties() {
        let a = rows(&[&[1, 9], &[1, 8], &[2, 0]]);
        let b = rows(&[&[1, 8], &[1, 9], &[2, 0]]);
        assert!(equal(&a, &b, Some(&[0]), false).is_ok());
        let c = rows(&[&[2, 0], &[1, 8], &[1, 9]]);
        assert!(equal(&a, &c, Some(&[0]), false).is_err());
        assert!(equal(&a, &b, Some(&[1]), false).is_err());
    }

    #[test]
    fn errors_compare_by_kind_unless_marked() {
        let a = Norm::Error("Arithmetic".into());
        let b = Norm::Error("Type".into());
        assert!(equal(&a, &b, None, false).is_err());
        assert!(equal(&a, &b, None, true).is_ok());
        assert!(equal(&a, &Norm::Affected(0), None, true).is_err());
        assert!(equal(&Norm::Affected(1), &Norm::Affected(2), None, false).is_err());
    }

    #[test]
    fn zero_reals_compare_equal_by_bits() {
        assert_eq!(real_bits(-0.0), real_bits(0.0));
    }
}
