//! The operator tree, rule-based access-path selection and execution.
//!
//! Access paths for one table, first match wins: primary-key equality,
//! primary-key range, index equality, index range (indexes in name order),
//! full scan. Only `col op constant` and `col BETWEEN c1 AND c2` conjuncts
//! of the WHERE clause qualify, with `op` one of `= < <= > >=`. Every
//! conjunct stays in the residual filter, so an access path only has to
//! return a superset of the matching rows.
//!
//! Operators are materialised (`Vec` of rows): a storage range borrows the
//! pager, so it is collected before any other tree is read.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use cairn_storage::Pager;

use crate::bind::{AggCall, BoundExpr, BoundKind, CmpOp};
use crate::catalog::{IndexDef, TableDef};
use crate::error::ExecError;
use crate::eval::{eval, eval_condition};
use crate::functions::Accumulator;
use crate::table::{
    KeyedRow, get_row, index_lookup_keys, index_range_keys, rows_by_keys, rows_in_range, scan,
};
use crate::value::{OrdRow, SqlType, Value, cmp_values};

#[derive(Debug, Clone)]
pub enum Access {
    Scan,
    PkLookup(Value),
    PkRange(Bound<Value>, Bound<Value>),
    IndexLookup(IndexDef, Value),
    IndexRange(IndexDef, Bound<Value>, Bound<Value>),
}

/// How one table is read: its definition, its EXPLAIN label (`t` or
/// `t AS a`) and the access path.
#[derive(Debug, Clone)]
pub struct TableAccess {
    pub table: TableDef,
    pub label: String,
    pub access: Access,
}

/// An index nested-loop join probe: for each outer row, evaluate `expr`
/// and look it up in the inner table's primary key (`index` is `None`) or
/// in `index`.
#[derive(Debug, Clone)]
pub struct Probe {
    pub index: Option<IndexDef>,
    pub expr: BoundExpr,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum Plan {
    /// One empty row, for SELECT without FROM.
    Values,
    Access(TableAccess),
    NestedLoop {
        left: bool,
        outer: Box<Plan>,
        inner: Box<Plan>,
        on: BoundExpr,
        inner_width: usize,
    },
    IndexJoin {
        left: bool,
        outer: Box<Plan>,
        table: TableDef,
        label: String,
        probe: Probe,
        on: BoundExpr,
    },
    Filter {
        input: Box<Plan>,
        cond: BoundExpr,
        text: String,
    },
    Aggregate {
        input: Box<Plan>,
        groups: Vec<BoundExpr>,
        aggs: Vec<AggCall>,
        text: String,
    },
    Sort {
        input: Box<Plan>,
        keys: Vec<(BoundExpr, bool)>,
        text: String,
    },
    Project {
        input: Box<Plan>,
        exprs: Vec<BoundExpr>,
        names: Vec<String>,
    },
    Distinct {
        input: Box<Plan>,
    },
    Limit {
        input: Box<Plan>,
        count: i64,
        offset: i64,
    },
}

/// Splits a condition into its top-level AND conjuncts.
pub fn conjuncts(expr: &BoundExpr) -> Vec<&BoundExpr> {
    match &expr.kind {
        BoundKind::And(left, right) => {
            let mut all = conjuncts(left);
            all.extend(conjuncts(right));
            all
        }
        _ => vec![expr],
    }
}

/// A sargable predicate `column op value` on one table.
struct Sarg {
    column: usize,
    op: CmpOp,
    value: Value,
}

/// Extracts the sargable conjuncts on the table whose columns occupy
/// `offset..offset + width` of the row, evaluating constants once.
fn sargs(conds: &[&BoundExpr], offset: usize, width: usize) -> Result<Vec<Sarg>, ExecError> {
    let column_of = |e: &BoundExpr| match e.kind {
        BoundKind::Column(c) if c >= offset && c < offset + width => Some(c - offset),
        _ => None,
    };
    let mut found = Vec::new();
    for cond in conds {
        match &cond.kind {
            BoundKind::Compare(op, left, right) if *op != CmpOp::NotEq => {
                if let (Some(column), true) = (column_of(left), right.is_constant()) {
                    let value = eval(right, &[])?;
                    found.push(Sarg {
                        column,
                        op: *op,
                        value,
                    });
                } else if let (Some(column), true) = (column_of(right), left.is_constant()) {
                    let value = eval(left, &[])?;
                    found.push(Sarg {
                        column,
                        op: op.flipped(),
                        value,
                    });
                }
            }
            BoundKind::Between {
                operand,
                low,
                high,
                negated: false,
            } => {
                if let (Some(column), true, true) =
                    (column_of(operand), low.is_constant(), high.is_constant())
                {
                    let (low, high) = (eval(low, &[])?, eval(high, &[])?);
                    found.push(Sarg {
                        column,
                        op: CmpOp::GtEq,
                        value: low,
                    });
                    found.push(Sarg {
                        column,
                        op: CmpOp::LtEq,
                        value: high,
                    });
                }
            }
            _ => {}
        }
    }
    Ok(found)
}

/// A constant usable against a column of type `ty`, converted to the
/// column's type, or `None` when the index cannot be used for it.
fn usable(value: &Value, ty: SqlType) -> Option<Value> {
    const LIMIT: i64 = 1 << 53;
    match (value, ty) {
        (Value::Null, _) => Some(Value::Null),
        (Value::Integer(v), SqlType::Real) if (-LIMIT..=LIMIT).contains(v) => {
            Some(Value::real(*v as f64))
        }
        (v, ty) if v.sql_type() == ty => Some(v.clone()),
        _ => None,
    }
}

/// Chooses the access path for `table`, given the WHERE conjuncts and where
/// the table's columns start in the row.
pub fn choose_access(
    table: &TableDef,
    indexes: &[IndexDef],
    conds: &[&BoundExpr],
    offset: usize,
) -> Result<Access, ExecError> {
    let all = sargs(conds, offset, table.columns.len())?;
    let on_column = |column: usize| -> Vec<(CmpOp, Value)> {
        let ty = table.columns.get(column).map_or(SqlType::Null, |c| c.ty);
        all.iter()
            .filter(|s| s.column == column)
            .filter_map(|s| usable(&s.value, ty).map(|v| (s.op, v)))
            .collect()
    };
    let equality = |preds: &[(CmpOp, Value)]| {
        preds
            .iter()
            .find(|(op, _)| *op == CmpOp::Eq)
            .map(|(_, v)| v.clone())
    };
    if let Some(pk) = table.pk {
        let preds = on_column(pk);
        if let Some(value) = equality(&preds) {
            return Ok(Access::PkLookup(value));
        }
        if let Some((low, high)) = intersect(&preds) {
            return Ok(Access::PkRange(low, high));
        }
    }
    let candidates: Vec<(&IndexDef, Vec<(CmpOp, Value)>)> = indexes
        .iter()
        .map(|index| (index, on_column(index.column)))
        .collect();
    for (index, preds) in &candidates {
        if let Some(value) = equality(preds) {
            return Ok(Access::IndexLookup((*index).clone(), value));
        }
    }
    for (index, preds) in &candidates {
        if let Some((low, high)) = intersect(preds) {
            return Ok(Access::IndexRange((*index).clone(), low, high));
        }
    }
    Ok(Access::Scan)
}

/// The tightest range implied by the range predicates, or `None` without
/// any. A NULL bound makes the range empty and is kept so EXPLAIN shows it.
fn intersect(preds: &[(CmpOp, Value)]) -> Option<(Bound<Value>, Bound<Value>)> {
    let mut low: Bound<Value> = Bound::Unbounded;
    let mut high: Bound<Value> = Bound::Unbounded;
    let mut any = false;
    for (op, value) in preds {
        let (slot, candidate, lower) = match op {
            CmpOp::Gt => (&mut low, Bound::Excluded(value.clone()), true),
            CmpOp::GtEq => (&mut low, Bound::Included(value.clone()), true),
            CmpOp::Lt => (&mut high, Bound::Excluded(value.clone()), false),
            CmpOp::LtEq => (&mut high, Bound::Included(value.clone()), false),
            CmpOp::Eq | CmpOp::NotEq => continue,
        };
        any = true;
        if tighter(&candidate, slot, lower) {
            *slot = candidate;
        }
    }
    any.then_some((low, high))
}

fn bound_value(bound: &Bound<Value>) -> Option<&Value> {
    match bound {
        Bound::Included(v) | Bound::Excluded(v) => Some(v),
        Bound::Unbounded => None,
    }
}

fn tighter(candidate: &Bound<Value>, current: &Bound<Value>, lower: bool) -> bool {
    let (Some(new), Some(old)) = (bound_value(candidate), bound_value(current)) else {
        return bound_value(current).is_none_or(|old| !old.is_null());
    };
    if old.is_null() {
        return false;
    }
    if new.is_null() {
        return true;
    }
    match cmp_values(new, old) {
        std::cmp::Ordering::Equal => matches!(candidate, Bound::Excluded(_)),
        std::cmp::Ordering::Greater => lower,
        std::cmp::Ordering::Less => !lower,
    }
}

fn has_null(bounds: [&Bound<Value>; 2]) -> bool {
    bounds
        .iter()
        .any(|b| bound_value(b).is_some_and(Value::is_null))
}

fn int_bound(bound: &Bound<Value>) -> Bound<i64> {
    match bound {
        Bound::Included(Value::Integer(v)) => Bound::Included(*v),
        Bound::Excluded(Value::Integer(v)) => Bound::Excluded(*v),
        _ => Bound::Unbounded,
    }
}

/// Reads the rows an access path selects, with their keys.
pub fn fetch(pager: &mut Pager, access: &TableAccess) -> Result<Vec<KeyedRow>, ExecError> {
    let table = &access.table;
    match &access.access {
        Access::Scan => scan(pager, table),
        Access::PkLookup(Value::Integer(key)) => Ok(get_row(pager, table, *key)?
            .map(|row| (*key, row))
            .into_iter()
            .collect()),
        Access::PkLookup(_) => Ok(Vec::new()),
        Access::PkRange(low, high) => {
            if has_null([low, high]) {
                return Ok(Vec::new());
            }
            rows_in_range(pager, table, int_bound(low), int_bound(high))
        }
        Access::IndexLookup(index, value) => {
            let keys = index_lookup_keys(pager, index, value)?;
            rows_by_keys(pager, table, keys)
        }
        Access::IndexRange(index, low, high) => {
            if has_null([low, high]) {
                return Ok(Vec::new());
            }
            let keys = index_range_keys(pager, index, low.as_ref(), high.as_ref())?;
            rows_by_keys(pager, table, keys)
        }
    }
}

pub fn run(plan: &Plan, pager: &mut Pager) -> Result<Vec<Vec<Value>>, ExecError> {
    match plan {
        Plan::Values => Ok(vec![Vec::new()]),
        Plan::Access(access) => Ok(fetch(pager, access)?
            .into_iter()
            .map(|(_, row)| row)
            .collect()),
        Plan::NestedLoop {
            left,
            outer,
            inner,
            on,
            inner_width,
        } => {
            let outer = run(outer, pager)?;
            let inner = run(inner, pager)?;
            let mut out = Vec::new();
            for outer_row in outer {
                let candidates = inner.iter().map(|r| r.as_slice());
                join_row(&mut out, outer_row, candidates, on, *left, *inner_width)?;
            }
            Ok(out)
        }
        Plan::IndexJoin {
            left,
            outer,
            table,
            probe,
            on,
            ..
        } => {
            let outer = run(outer, pager)?;
            let mut out = Vec::new();
            for outer_row in outer {
                let value = eval(&probe.expr, &outer_row)?;
                let matches = probe_rows(pager, table, probe, &value)?;
                let candidates = matches.iter().map(|(_, r)| r.as_slice());
                join_row(
                    &mut out,
                    outer_row,
                    candidates,
                    on,
                    *left,
                    table.columns.len(),
                )?;
            }
            Ok(out)
        }
        Plan::Filter { input, cond, .. } => {
            let mut out = Vec::new();
            for row in run(input, pager)? {
                if eval_condition(cond, &row)? {
                    out.push(row);
                }
            }
            Ok(out)
        }
        Plan::Aggregate {
            input,
            groups,
            aggs,
            ..
        } => aggregate(run(input, pager)?, groups, aggs),
        Plan::Sort { input, keys, .. } => sort(run(input, pager)?, keys),
        Plan::Project { input, exprs, .. } => run(input, pager)?
            .iter()
            .map(|row| exprs.iter().map(|e| eval(e, row)).collect())
            .collect(),
        Plan::Distinct { input } => {
            let mut seen = BTreeSet::new();
            Ok(run(input, pager)?
                .into_iter()
                .filter(|row| seen.insert(OrdRow(row.clone())))
                .collect())
        }
        Plan::Limit {
            input,
            count,
            offset,
        } => {
            let skip = usize::try_from(*offset).unwrap_or(usize::MAX);
            let take = usize::try_from(*count).unwrap_or(usize::MAX);
            Ok(run(input, pager)?
                .into_iter()
                .skip(skip)
                .take(take)
                .collect())
        }
    }
}

fn probe_rows(
    pager: &mut Pager,
    table: &TableDef,
    probe: &Probe,
    value: &Value,
) -> Result<Vec<KeyedRow>, ExecError> {
    match (&probe.index, value) {
        (_, Value::Null) => Ok(Vec::new()),
        (None, Value::Integer(key)) => Ok(get_row(pager, table, *key)?
            .map(|row| (*key, row))
            .into_iter()
            .collect()),
        (None, _) => Ok(Vec::new()),
        (Some(index), value) => {
            let keys = index_lookup_keys(pager, index, value)?;
            rows_by_keys(pager, table, keys)
        }
    }
}

/// Emits `outer ++ inner` for each candidate passing ON; for a LEFT join
/// with no match, `outer` padded with NULLs.
fn join_row<'a>(
    out: &mut Vec<Vec<Value>>,
    outer: Vec<Value>,
    candidates: impl Iterator<Item = &'a [Value]>,
    on: &BoundExpr,
    left: bool,
    inner_width: usize,
) -> Result<(), ExecError> {
    let mut matched = false;
    for inner in candidates {
        let mut row = outer.clone();
        row.extend_from_slice(inner);
        if eval_condition(on, &row)? {
            matched = true;
            out.push(row);
        }
    }
    if left && !matched {
        let mut row = outer;
        row.resize(row.len() + inner_width, Value::Null);
        out.push(row);
    }
    Ok(())
}

/// Groups rows by the GROUP BY values (NULLs equal) in group-key order and
/// emits group values followed by aggregate results. Without GROUP BY
/// there is exactly one group, even over no rows.
fn aggregate(
    rows: Vec<Vec<Value>>,
    groups: &[BoundExpr],
    aggs: &[AggCall],
) -> Result<Vec<Vec<Value>>, ExecError> {
    let new_accumulators = || -> Vec<Accumulator> {
        aggs.iter()
            .map(|a| {
                let real = a.arg.as_ref().is_some_and(|e| e.ty == SqlType::Real);
                Accumulator::new(a.func, a.distinct, real, a.span)
            })
            .collect()
    };
    let mut table: BTreeMap<OrdRow, Vec<Accumulator>> = BTreeMap::new();
    if groups.is_empty() {
        table.insert(OrdRow(Vec::new()), new_accumulators());
    }
    for row in rows {
        let key = groups
            .iter()
            .map(|g| eval(g, &row))
            .collect::<Result<Vec<_>, _>>()?;
        let accumulators = table.entry(OrdRow(key)).or_insert_with(new_accumulators);
        for (acc, call) in accumulators.iter_mut().zip(aggs) {
            let input = match &call.arg {
                Some(arg) => Some(eval(arg, &row)?),
                None => None,
            };
            acc.add(input)?;
        }
    }
    let mut out = Vec::with_capacity(table.len());
    for (OrdRow(mut key), accumulators) in table {
        for acc in accumulators {
            key.push(acc.finish()?);
        }
        out.push(key);
    }
    Ok(out)
}

/// Stable sort; ascending puts NULLs first, descending puts them last.
fn sort(rows: Vec<Vec<Value>>, keys: &[(BoundExpr, bool)]) -> Result<Vec<Vec<Value>>, ExecError> {
    let mut keyed = Vec::with_capacity(rows.len());
    for row in rows {
        let values = keys
            .iter()
            .map(|(e, _)| eval(e, &row))
            .collect::<Result<Vec<_>, _>>()?;
        keyed.push((values, row));
    }
    keyed.sort_by(|(a, _), (b, _)| {
        for ((x, y), (_, descending)) in a.iter().zip(b).zip(keys) {
            let ordering = cmp_values(x, y);
            if ordering.is_ne() {
                return if *descending {
                    ordering.reverse()
                } else {
                    ordering
                };
            }
        }
        std::cmp::Ordering::Equal
    });
    Ok(keyed.into_iter().map(|(_, row)| row).collect())
}
