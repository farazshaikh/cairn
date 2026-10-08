//! SELECT, following the README pipeline: sources and joins, WHERE,
//! grouping and HAVING, ORDER BY, the select list, DISTINCT, LIMIT.
//!
//! The README "Planner and EXPLAIN" rules decide which rows of the first
//! table are read and in what order, and when a join probes the inner
//! table instead of scanning it. Results never depend on them, but row
//! order without ORDER BY does, and so does whether an expression that
//! fails on some row is ever evaluated on that row. The constants of
//! indexable conditions are evaluated while planning, even on an empty
//! table.

use std::cmp::Ordering;

use cairn_sql::{BinaryOp, Expr, ExprKind, JoinKind, Literal, Name, Select, SelectItem, Spanned};

use crate::RErrorKind;
use crate::expr::{
    Agg, AggCall, Cmp, Context, Groups, Op, Scope, Typed, check, check_condition, compare, eval,
    has_aggregate, passes,
};
use crate::state::{State, Table};
use crate::value::{RValue, SType, order, order_rows};

pub(crate) type Rows = Vec<Vec<RValue>>;
pub(crate) type KeyedRows = Vec<(i64, Vec<RValue>)>;

/// The top-level AND conjuncts of a condition.
pub(crate) fn conjuncts(expr: &Typed) -> Vec<&Typed> {
    match &expr.op {
        Op::And(a, b) => {
            let mut all = conjuncts(a);
            all.extend(conjuncts(b));
            all
        }
        _ => vec![expr],
    }
}

fn ast_conjuncts(expr: &Expr) -> Vec<&Expr> {
    match &expr.node {
        ExprKind::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => {
            let mut all = ast_conjuncts(left);
            all.extend(ast_conjuncts(right));
            all
        }
        _ => vec![expr],
    }
}

/// How the rows of one table are read.
#[derive(Debug)]
pub(crate) enum Path {
    Scan,
    KeyEquals(RValue),
    KeyRange(Vec<(Cmp, RValue)>),
    IndexEquals(usize, RValue),
    IndexRange(usize, Vec<(Cmp, RValue)>),
}

/// Chooses the access path for a table whose columns start at position 0
/// of the row, evaluating the constants of its indexable conjuncts.
pub(crate) fn choose_path(
    state: &State,
    table: &Table,
    conds: &[&Typed],
) -> Result<Path, RErrorKind> {
    let width = table.columns.len();
    let column_of = |e: &Typed| match e.op {
        Op::Column(c) if c < width => Some(c),
        _ => None,
    };
    let mut sargs: Vec<(usize, Cmp, RValue)> = Vec::new();
    for cond in conds {
        match &cond.op {
            Op::Compare(cmp, left, right) if *cmp != Cmp::Ne => {
                if let (Some(column), true) = (column_of(left), right.constant()) {
                    sargs.push((column, *cmp, eval(right, &[])?));
                } else if let (Some(column), true) = (column_of(right), left.constant()) {
                    sargs.push((column, cmp.reversed(), eval(left, &[])?));
                }
            }
            Op::Between(operand, low, high, false) => {
                if let (Some(column), true, true) =
                    (column_of(operand), low.constant(), high.constant())
                {
                    let low = eval(low, &[])?;
                    let high = eval(high, &[])?;
                    sargs.push((column, Cmp::Ge, low));
                    sargs.push((column, Cmp::Le, high));
                }
            }
            _ => {}
        }
    }
    let on_column = |column: usize| -> Vec<(Cmp, RValue)> {
        let ty = table.columns.get(column).map_or(SType::Null, |c| c.ty);
        sargs
            .iter()
            .filter(|(c, _, _)| *c == column)
            .filter_map(|(_, cmp, value)| usable(value, ty).map(|v| (*cmp, v)))
            .collect()
    };
    let equality = |preds: &[(Cmp, RValue)]| {
        preds
            .iter()
            .find(|(cmp, _)| *cmp == Cmp::Eq)
            .map(|(_, v)| v.clone())
    };
    let ranges = |preds: Vec<(Cmp, RValue)>| -> Option<Vec<(Cmp, RValue)>> {
        let ranges: Vec<_> = preds.into_iter().filter(|(c, _)| *c != Cmp::Eq).collect();
        (!ranges.is_empty()).then_some(ranges)
    };
    if let Some(pk) = table.int_pk {
        let preds = on_column(pk);
        if let Some(value) = equality(&preds) {
            return Ok(Path::KeyEquals(value));
        }
        if let Some(range) = ranges(preds) {
            return Ok(Path::KeyRange(range));
        }
    }
    let indexed: Vec<usize> = state
        .indexes_of(&table.name)
        .iter()
        .map(|index| index.column)
        .collect();
    for &column in &indexed {
        if let Some(value) = equality(&on_column(column)) {
            return Ok(Path::IndexEquals(column, value));
        }
    }
    for &column in &indexed {
        if let Some(range) = ranges(on_column(column)) {
            return Ok(Path::IndexRange(column, range));
        }
    }
    Ok(Path::Scan)
}

/// A constant an index can use against a column of type `ty`: the README
/// rule "an index is not used when the constant's type does not match the
/// column". INTEGER constants within ±2^53 match REAL columns.
fn usable(value: &RValue, ty: SType) -> Option<RValue> {
    const LIMIT: i64 = 1 << 53;
    match (value, ty) {
        (RValue::Null, _) => Some(RValue::Null),
        (RValue::Integer(v), SType::Real) if (-LIMIT..=LIMIT).contains(v) => {
            Some(RValue::real(*v as f64))
        }
        (v, ty) if v.sql_type() == ty => Some(v.clone()),
        _ => None,
    }
}

fn in_range(value: &RValue, range: &[(Cmp, RValue)]) -> bool {
    range
        .iter()
        .all(|(cmp, bound)| compare(*cmp, value, bound) == Some(true))
}

/// The rows a path reads, with their keys, in the order it reads them. A
/// NULL constant selects nothing.
pub(crate) fn fetch(table: &Table, path: &Path) -> KeyedRows {
    let value_of = |row: &[RValue], column: usize| row.get(column).cloned().unwrap_or(RValue::Null);
    match path {
        Path::Scan => table.rows.clone(),
        Path::KeyEquals(value) => table
            .rows
            .iter()
            .filter(|(key, _)| *value == RValue::Integer(*key))
            .cloned()
            .collect(),
        Path::KeyRange(range) => table
            .rows
            .iter()
            .filter(|(key, _)| in_range(&RValue::Integer(*key), range))
            .cloned()
            .collect(),
        Path::IndexEquals(column, value) => table
            .rows
            .iter()
            .filter(|(_, row)| compare(Cmp::Eq, &value_of(row, *column), value) == Some(true))
            .cloned()
            .collect(),
        Path::IndexRange(column, range) => {
            let mut rows: KeyedRows = table
                .rows
                .iter()
                .filter(|(_, row)| in_range(&value_of(row, *column), range))
                .cloned()
                .collect();
            rows.sort_by(|(ka, a), (kb, b)| {
                order(&value_of(a, *column), &value_of(b, *column)).then(ka.cmp(kb))
            });
            rows
        }
    }
}

/// How a join reads its inner table for each outer row.
enum Probe {
    /// Every inner row, in key order.
    Scan,
    /// Inner rows whose `column` equals `value` evaluated on the outer row.
    Equals { column: usize, value: Typed },
}

/// The README INDEX JOIN rule. The first ON conjunct of the form
/// `inner.col = <expression over earlier tables>` with equal types
/// decides: it probes when `col` is the INTEGER PRIMARY KEY or indexed,
/// and otherwise the join scans.
fn find_probe(state: &State, table: &Table, offset: usize, on: &Typed, on_ast: &Expr) -> Probe {
    let width = table.columns.len();
    for (cond, _) in conjuncts(on).into_iter().zip(ast_conjuncts(on_ast)) {
        let Op::Compare(Cmp::Eq, left, right) = &cond.op else {
            continue;
        };
        for (side, other) in [(left, right), (right, left)] {
            let Op::Column(c) = side.op else {
                continue;
            };
            if c < offset || c >= offset + width {
                continue;
            }
            if other.columns().iter().any(|&o| o >= offset) {
                continue;
            }
            let column = c - offset;
            let Some(def) = table.columns.get(column) else {
                continue;
            };
            if other.ty != def.ty {
                continue;
            }
            let indexed = table.int_pk == Some(column)
                || state
                    .indexes_of(&table.name)
                    .iter()
                    .any(|i| i.column == column);
            if !indexed {
                return Probe::Scan;
            }
            return Probe::Equals {
                column,
                value: (**other).clone(),
            };
        }
    }
    Probe::Scan
}

struct Join<'a> {
    table: &'a Table,
    left: bool,
    on: Typed,
    probe: Probe,
}

/// One select-list entry before checking: its expression, output name and
/// alias.
struct Item {
    expr: Expr,
    name: String,
    alias: Option<String>,
}

/// A planned query, ready to run.
pub(crate) struct Planned<'a> {
    first: Option<(&'a Table, Path)>,
    joins: Vec<Join<'a>>,
    filter: Option<Typed>,
    grouping: Option<(Vec<Typed>, Vec<AggCall>)>,
    having: Option<Typed>,
    sort: Vec<(Typed, bool)>,
    exprs: Vec<Typed>,
    pub columns: Vec<String>,
    distinct: bool,
    limit: Option<(i64, i64)>,
}

/// Checks a SELECT and plans it: every static error and every planning
/// time evaluation happens here.
pub(crate) fn plan<'a>(state: &'a State, select: &Select) -> Result<Planned<'a>, RErrorKind> {
    let mut scope = Scope::default();
    let mut tables: Vec<&Table> = Vec::new();
    let mut join_asts = Vec::new();
    if let Some(from) = &select.from {
        let refs = std::iter::once((&from.node.table, None)).chain(
            from.node
                .joins
                .iter()
                .map(|j| (&j.node.table, Some(&j.node))),
        );
        for (table_ref, join) in refs {
            let table = state
                .tables
                .get(&table_ref.node.name.node.0)
                .ok_or(RErrorKind::NotFound)?;
            let binding = table_ref
                .node
                .alias
                .as_ref()
                .map_or(&table_ref.node.name.node.0, |a| &a.node.0)
                .clone();
            if scope.bindings.iter().any(|b| b.name == binding) {
                return Err(RErrorKind::AlreadyExists);
            }
            scope.add(binding, table.column_types());
            tables.push(table);
            if let Some(join) = join {
                join_asts.push(join);
            }
        }
    }
    let filter = match &select.where_clause {
        Some(w) => Some(check_condition(w, &scope, &mut Context::Plain)?),
        None => None,
    };
    let first = match tables.first() {
        Some(table) => {
            let conds = filter.as_ref().map(conjuncts).unwrap_or_default();
            Some((*table, choose_path(state, table, &conds)?))
        }
        None => None,
    };
    let mut joins = Vec::new();
    for (i, join) in join_asts.iter().enumerate() {
        let position = i + 1;
        let on = check_condition(&join.on, &scope.first(position + 1), &mut Context::Plain)?;
        let (Some(table), Some(binding)) = (tables.get(position), scope.bindings.get(position))
        else {
            return Err(RErrorKind::NotFound);
        };
        let probe = find_probe(state, table, binding.offset, &on, &join.on);
        joins.push(Join {
            table,
            left: join.kind == JoinKind::Left,
            on,
            probe,
        });
    }
    let items = expand(select, &scope)?;
    let aggregating = !select.group_by.is_empty()
        || select.having.is_some()
        || select.items.iter().any(|item| match &item.node {
            SelectItem::Expr { expr, .. } => has_aggregate(expr),
            _ => false,
        })
        || select.order_by.iter().any(|o| has_aggregate(&o.node.expr));
    let mut groups = Groups::default();
    let mut keys = Vec::with_capacity(select.group_by.len());
    for key in &select.group_by {
        let typed = check(key, &scope, &mut Context::Plain)?;
        groups.keys.push((key.clone(), typed.ty));
        keys.push(typed);
    }
    let mut context = if aggregating {
        Context::Grouped(&mut groups)
    } else {
        Context::Plain
    };
    let mut exprs = Vec::with_capacity(items.len());
    for item in &items {
        exprs.push(check(&item.expr, &scope, &mut context)?);
    }
    let having = match &select.having {
        Some(h) => Some(check_condition(h, &scope, &mut context)?),
        None => None,
    };
    let sort = order_keys(select, &scope, &mut context, &items, &exprs)?;
    let grouping = aggregating.then(|| (keys, std::mem::take(&mut groups.aggs)));
    Ok(Planned {
        first,
        joins,
        filter,
        grouping,
        having,
        sort,
        exprs,
        columns: items.into_iter().map(|i| i.name).collect(),
        distinct: select.distinct,
        limit: select.limit.as_ref().map(|l| {
            (
                l.node.count.node,
                l.node.offset.as_ref().map_or(0, |o| o.node),
            )
        }),
    })
}

fn expand(select: &Select, scope: &Scope) -> Result<Vec<Item>, RErrorKind> {
    let mut items = Vec::new();
    for item in &select.items {
        match &item.node {
            SelectItem::Wildcard => {
                if scope.bindings.is_empty() {
                    return Err(RErrorKind::Type);
                }
                for binding in &scope.bindings {
                    expand_table(&mut items, &binding.name, &binding.columns, item.span);
                }
            }
            SelectItem::QualifiedWildcard(qualifier) => {
                let binding = scope
                    .bindings
                    .iter()
                    .find(|b| b.name == qualifier.node.0)
                    .ok_or(RErrorKind::NotFound)?;
                expand_table(&mut items, &binding.name, &binding.columns, item.span);
            }
            SelectItem::Expr { expr, alias } => {
                let alias = alias.as_ref().map(|a| a.node.0.clone());
                let name = match (&alias, &expr.node) {
                    (Some(alias), _) => alias.clone(),
                    (None, ExprKind::Column { name, .. }) => name.node.0.clone(),
                    (None, _) => expr.to_string(),
                };
                items.push(Item {
                    expr: expr.clone(),
                    name,
                    alias,
                });
            }
        }
    }
    Ok(items)
}

fn expand_table(
    items: &mut Vec<Item>,
    binding: &str,
    columns: &[(String, SType)],
    span: cairn_sql::Span,
) {
    for (column, _) in columns {
        let ident = |name: &str| Spanned::new(Name(name.to_string()), span);
        let expr = Spanned::new(
            ExprKind::Column {
                table: Some(ident(binding)),
                name: ident(column),
            },
            span,
        );
        items.push(Item {
            expr,
            name: column.clone(),
            alias: None,
        });
    }
}

/// README ORDER BY: an integer literal is an output position, a bare name
/// equal to an output alias is that item, anything else is an expression.
/// With DISTINCT every key must be a select item.
fn order_keys(
    select: &Select,
    scope: &Scope,
    context: &mut Context,
    items: &[Item],
    exprs: &[Typed],
) -> Result<Vec<(Typed, bool)>, RErrorKind> {
    let mut keys = Vec::with_capacity(select.order_by.len());
    for order_item in &select.order_by {
        let expr = &order_item.node.expr;
        let alias = match &expr.node {
            ExprKind::Column { table: None, name } => items
                .iter()
                .position(|i| i.alias.as_deref() == Some(name.node.0.as_str())),
            _ => None,
        };
        let (typed, is_item) = match (&expr.node, alias) {
            (ExprKind::Literal(Literal::Integer(n)), _) => {
                let position = usize::try_from(*n).ok().filter(|p| *p >= 1);
                let typed = position
                    .and_then(|p| exprs.get(p - 1))
                    .ok_or(RErrorKind::Type)?;
                (typed.clone(), true)
            }
            (_, Some(index)) => (exprs.get(index).cloned().ok_or(RErrorKind::Type)?, true),
            _ => {
                let typed = check(expr, scope, context)?;
                let same_expr = items.iter().any(|i| i.expr == *expr);
                let same_column = match typed.op {
                    Op::Column(c) => exprs
                        .iter()
                        .any(|e| matches!(e.op, Op::Column(d) if d == c)),
                    _ => false,
                };
                (typed, same_expr || same_column)
            }
        };
        if select.distinct && !is_item {
            return Err(RErrorKind::Type);
        }
        keys.push((typed, order_item.node.descending));
    }
    Ok(keys)
}

impl Planned<'_> {
    /// Runs the plan and returns the output rows.
    pub(crate) fn run(&self) -> Result<Rows, RErrorKind> {
        let mut rows: Rows = match &self.first {
            Some((table, path)) => fetch(table, path).into_iter().map(|(_, r)| r).collect(),
            None => vec![Vec::new()],
        };
        for join in &self.joins {
            rows = run_join(join, rows)?;
        }
        if let Some(filter) = &self.filter {
            rows = keep(rows, filter)?;
        }
        if let Some((keys, aggs)) = &self.grouping {
            rows = aggregate(rows, keys, aggs)?;
            if let Some(having) = &self.having {
                rows = keep(rows, having)?;
            }
        }
        if !self.sort.is_empty() {
            rows = sort(rows, &self.sort)?;
        }
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            out.push(
                self.exprs
                    .iter()
                    .map(|e| eval(e, row))
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        if self.distinct {
            let mut seen: Vec<Vec<RValue>> = Vec::new();
            out.retain(|row| {
                let duplicate = seen.iter().any(|s| order_rows(s, row).is_eq());
                if !duplicate {
                    seen.push(row.clone());
                }
                !duplicate
            });
        }
        if let Some((count, offset)) = self.limit {
            let skip = usize::try_from(offset).unwrap_or(usize::MAX);
            let take = usize::try_from(count).unwrap_or(usize::MAX);
            out = out.into_iter().skip(skip).take(take).collect();
        }
        Ok(out)
    }
}

fn keep(rows: Rows, cond: &Typed) -> Result<Rows, RErrorKind> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if passes(cond, &row)? {
            out.push(row);
        }
    }
    Ok(out)
}

fn run_join(join: &Join, outer: Rows) -> Result<Rows, RErrorKind> {
    let width = join.table.columns.len();
    let mut out = Vec::new();
    for outer_row in outer {
        let candidates: Vec<&Vec<RValue>> = match &join.probe {
            Probe::Scan => join.table.rows.iter().map(|(_, r)| r).collect(),
            Probe::Equals { column, value } => {
                let value = eval(value, &outer_row)?;
                if value.is_null() {
                    Vec::new()
                } else {
                    join.table
                        .rows
                        .iter()
                        .filter(|(_, r)| {
                            r.get(*column)
                                .is_some_and(|v| compare(Cmp::Eq, v, &value) == Some(true))
                        })
                        .map(|(_, r)| r)
                        .collect()
                }
            }
        };
        let mut matched = false;
        for inner in candidates {
            let mut row = outer_row.clone();
            row.extend(inner.iter().cloned());
            if passes(&join.on, &row)? {
                matched = true;
                out.push(row);
            }
        }
        if join.left && !matched {
            let mut row = outer_row;
            row.resize(row.len() + width, RValue::Null);
            out.push(row);
        }
    }
    Ok(out)
}

/// The running state of one aggregate within one group.
struct Fold {
    func: Agg,
    real: bool,
    seen: Option<Vec<RValue>>,
    count: i64,
    int_total: i128,
    real_total: f64,
    best: Option<RValue>,
}

impl Fold {
    fn new(call: &AggCall) -> Fold {
        Fold {
            func: call.func,
            real: call.arg.as_ref().is_some_and(|a| a.ty == SType::Real),
            seen: call.distinct.then(Vec::new),
            count: 0,
            int_total: 0,
            real_total: 0.0,
            best: None,
        }
    }

    fn add(&mut self, input: Option<RValue>) -> Result<(), RErrorKind> {
        let Some(value) = input else {
            self.count += 1;
            return Ok(());
        };
        if value.is_null() {
            return Ok(());
        }
        if let Some(seen) = &mut self.seen {
            if seen.iter().any(|s| order(s, &value).is_eq()) {
                return Ok(());
            }
            seen.push(value.clone());
        }
        self.count += 1;
        match self.func {
            Agg::Count => {}
            Agg::Sum | Agg::Avg => match (&value, self.real) {
                (RValue::Integer(v), false) => {
                    self.int_total = self
                        .int_total
                        .checked_add(i128::from(*v))
                        .ok_or(RErrorKind::Arithmetic)?;
                }
                _ => {
                    self.real_total += value.as_f64().unwrap_or(0.0);
                    if !self.real_total.is_finite() {
                        return Err(RErrorKind::Arithmetic);
                    }
                }
            },
            Agg::Min | Agg::Max => {
                let better = self.best.as_ref().is_none_or(|best| {
                    let ordering = order(&value, best);
                    if self.func == Agg::Min {
                        ordering.is_lt()
                    } else {
                        ordering.is_gt()
                    }
                });
                if better {
                    self.best = Some(value);
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<RValue, RErrorKind> {
        if self.func == Agg::Count {
            return Ok(RValue::Integer(self.count));
        }
        if self.count == 0 {
            return Ok(RValue::Null);
        }
        match self.func {
            Agg::Sum if self.real => Ok(RValue::real(self.real_total)),
            Agg::Sum => i64::try_from(self.int_total)
                .map(RValue::Integer)
                .map_err(|_| RErrorKind::Arithmetic),
            Agg::Avg => {
                let total = if self.real {
                    self.real_total
                } else {
                    self.int_total as f64
                };
                Ok(RValue::real(total / self.count as f64))
            }
            _ => Ok(self.best.unwrap_or(RValue::Null)),
        }
    }
}

/// Groups rows by their GROUP BY values (NULLs equal), in key order, and
/// emits the key values followed by the aggregate results. Without GROUP
/// BY there is exactly one group, even over no rows.
fn aggregate(rows: Rows, keys: &[Typed], aggs: &[AggCall]) -> Result<Rows, RErrorKind> {
    let fresh = || aggs.iter().map(Fold::new).collect::<Vec<_>>();
    let mut groups: Vec<(Vec<RValue>, Vec<Fold>)> = Vec::new();
    if keys.is_empty() {
        groups.push((Vec::new(), fresh()));
    }
    for row in rows {
        let key = keys
            .iter()
            .map(|k| eval(k, &row))
            .collect::<Result<Vec<_>, _>>()?;
        let slot = match groups.binary_search_by(|(k, _)| order_rows(k, &key)) {
            Ok(slot) => slot,
            Err(slot) => {
                groups.insert(slot, (key, fresh()));
                slot
            }
        };
        let Some((_, folds)) = groups.get_mut(slot) else {
            continue;
        };
        for (fold, call) in folds.iter_mut().zip(aggs) {
            let input = match &call.arg {
                Some(arg) => Some(eval(arg, &row)?),
                None => None,
            };
            fold.add(input)?;
        }
    }
    let mut out = Vec::with_capacity(groups.len());
    for (mut key, folds) in groups {
        for fold in folds {
            key.push(fold.finish()?);
        }
        out.push(key);
    }
    Ok(out)
}

/// Stable sort; ascending puts NULLs first, descending last.
fn sort(rows: Rows, keys: &[(Typed, bool)]) -> Result<Rows, RErrorKind> {
    let mut keyed = Vec::with_capacity(rows.len());
    for row in rows {
        let values = keys
            .iter()
            .map(|(k, _)| eval(k, &row))
            .collect::<Result<Vec<_>, _>>()?;
        keyed.push((values, row));
    }
    keyed.sort_by(|(a, _), (b, _)| {
        for ((x, y), (_, descending)) in a.iter().zip(b).zip(keys) {
            let ordering = order(x, y);
            if ordering != Ordering::Equal {
                return if *descending {
                    ordering.reverse()
                } else {
                    ordering
                };
            }
        }
        Ordering::Equal
    });
    Ok(keyed.into_iter().map(|(_, row)| row).collect())
}
