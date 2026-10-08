//! Generators of SQL for the fuzz targets.
//!
//! [`case`] builds one differential case: a schema, data and a sequence of
//! statements over it, each with what the comparison needs to know (output
//! positions that ORDER BY sorts on, and whether the statement has more
//! than one way to fail). The generator rules of the M6 design keep
//! results independent of row order, so that cairn and the reference must
//! agree exactly:
//!
//! - R-LIMIT: LIMIT and OFFSET only when ORDER BY covers every output
//!   column, so tied rows are identical.
//! - R-SUM: SUM only over REAL columns or integer columns whose values are
//!   small or all of one sign, so overflow does not depend on order.
//! - R-REAL: REAL values are multiples of 1/4 (exact sums), 0.0, -0.0 or
//!   1e300.
//! - R-ERR: a statement with two or more possible runtime errors is marked,
//!   and only error-or-not is compared for it.
//! - R-CONST: conditions never contain constant expressions that can fail.
//! - R-SIZE: at most 8 columns and short text, so storage size limits never
//!   apply.

mod expr;
pub mod sql_text;
mod stmt;

use crate::minimize::Step;
use crate::rng::Rng;

pub use stmt::case;

/// One generated statement and what the comparison needs to know about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseStep {
    /// The statement, or a reopen.
    pub step: Step,
    /// For a SELECT with ORDER BY: the output positions of its sort keys.
    pub order_keys: Option<Vec<usize>>,
    /// Two or more possible runtime errors (R-ERR): compare only whether
    /// both sides fail, not the error kind.
    pub multi_error: bool,
    /// A single-table SELECT whose WHERE has a condition an index or the
    /// primary key can serve, for the index-use counter.
    pub indexable: bool,
}

impl CaseStep {
    fn sql(sql: String) -> CaseStep {
        CaseStep {
            step: Step::Sql(sql),
            order_keys: None,
            multi_error: false,
            indexable: false,
        }
    }
}

/// Feature and rule counters of a run: how many cases used each.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    /// Feature or rule name and the number of cases that used it.
    pub counts: Vec<(&'static str, u32)>,
}

impl Coverage {
    /// Counts `name` once.
    pub fn add(&mut self, name: &'static str) {
        match self.counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => self.counts.push((name, 1)),
        }
    }

    /// Adds another coverage's counts.
    pub fn merge(&mut self, other: &Coverage) {
        for (name, count) in &other.counts {
            for _ in 0..*count {
                self.add(name);
            }
        }
    }

    /// How many times `name` was counted.
    pub fn get(&self, name: &str) -> u32 {
        self.counts
            .iter()
            .find(|(n, _)| *n == name)
            .map_or(0, |(_, c)| *c)
    }
}

/// The SQL type of a generated column or expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ty {
    Int,
    Real,
    Text,
    Bool,
}

impl Ty {
    fn sql(self) -> &'static str {
        match self {
            Ty::Int => "INTEGER",
            Ty::Real => "REAL",
            Ty::Text => "TEXT",
            Ty::Bool => "BOOLEAN",
        }
    }
}

/// The values an INTEGER column holds (R-SUM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntClass {
    /// Absolute values at most 2^40.
    Small,
    /// Values of one sign, including the i64 bounds.
    Positive,
    Negative,
    /// Anything.
    Wild,
}

impl IntClass {
    /// Whether SUM over the column fails the same way in any order.
    fn summable(self) -> bool {
        self != IntClass::Wild
    }

    /// Whether arithmetic on the column can overflow.
    fn risky(self) -> bool {
        self != IntClass::Small
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Col {
    pub name: String,
    pub ty: Ty,
    pub class: IntClass,
    pub pk: bool,
    pub not_null: bool,
    pub unique: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Tab {
    pub name: String,
    pub cols: Vec<Col>,
    /// Columns with an explicit or implicit index, or the INTEGER PRIMARY
    /// KEY.
    pub indexed: Vec<usize>,
}

impl Tab {
    fn constrained(&self) -> bool {
        self.cols.iter().any(|c| c.pk || c.not_null || c.unique)
    }
}

/// A literal of type `ty` for a column of class `class`, as SQL text.
pub(crate) fn literal(rng: &mut Rng, ty: Ty, class: IntClass) -> String {
    match ty {
        Ty::Int => int_literal(rng, class),
        Ty::Real => real_literal(rng),
        Ty::Text => text_literal(rng),
        Ty::Bool => (if rng.chance(1, 2) { "TRUE" } else { "FALSE" }).to_string(),
    }
}

fn int_text(v: i64) -> String {
    if v == i64::MIN {
        "-9223372036854775807 - 1".to_string()
    } else if v < 0 {
        format!("-{}", v.unsigned_abs())
    } else {
        v.to_string()
    }
}

pub(crate) fn int_literal(rng: &mut Rng, class: IntClass) -> String {
    let v = match class {
        IntClass::Small => match rng.below(10) {
            0 => rng.range(-(1 << 40), 1 << 40),
            _ => rng.range(-20, 20),
        },
        IntClass::Positive => match rng.below(6) {
            0 => i64::MAX,
            1 => i64::MAX - rng.range(0, 3),
            _ => rng.range(0, 50),
        },
        IntClass::Negative => match rng.below(6) {
            0 => i64::MIN,
            1 => i64::MIN + rng.range(0, 3),
            _ => rng.range(-50, 0),
        },
        IntClass::Wild => match rng.below(6) {
            0 => i64::MAX,
            1 => i64::MIN,
            2 => rng.range(i64::MIN, i64::MAX),
            _ => rng.range(-50, 50),
        },
    };
    int_text(v)
}

/// R-REAL: multiples of 1/4, 0.0, -0.0 or 1e300.
pub(crate) fn real_literal(rng: &mut Rng) -> String {
    match rng.below(12) {
        0 => "0.0".to_string(),
        1 => "-0.0".to_string(),
        2 => "1e300".to_string(),
        _ => {
            let k = rng.range(-(1 << 20), 1 << 20);
            let text = format!("{:?}", (k.unsigned_abs() as f64) / 4.0);
            if k < 0 { format!("-{text}") } else { text }
        }
    }
}

const TEXT_CHARS: [&str; 10] = ["a", "b", "c", "A", "é", "''", "%", "_", " ", "\t"];

pub(crate) fn text_literal(rng: &mut Rng) -> String {
    let len = rng.below(5);
    let body: String = (0..len).map(|_| *rng.pick(&TEXT_CHARS)).collect();
    format!("'{body}'")
}
