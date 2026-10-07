//! INSERT, UPDATE and DELETE.
//!
//! Each statement computes every new row and checks types, NOT NULL,
//! PRIMARY KEY, UNIQUE and encoded sizes before writing anything, so a
//! failing statement leaves the database unchanged. Uniqueness is judged
//! against the state after the statement: a value conflicts only with
//! another new value or with a stored row the statement does not rewrite,
//! so `UPDATE t SET id = id + 1` succeeds.

use std::collections::{BTreeMap, BTreeSet};

use cairn_sql::{Delete, Expr, Insert, Span, Update};
use cairn_storage::{MAX_KEY_LEN, MAX_VALUE_LEN};

use crate::bind::{Mode, Scope, bind, bind_condition};
use crate::catalog::{IndexDef, TableDef};
use crate::codec::key::index_key;
use crate::codec::record::encode_record;
use crate::database::{Database, QueryResult};
use crate::error::{ErrorKind, ExecError};
use crate::eval::{eval, eval_condition};
use crate::plan::{TableAccess, choose_access, conjuncts, fetch};
use crate::table::{KeyedRow, WriteSet, get_row, index_lookup_keys};
use crate::value::{OrdRow, SqlType, Value};

/// A validated new row: key, values and the span each column's errors
/// point at.
struct NewRow {
    key: i64,
    values: Vec<Value>,
    spans: Vec<Span>,
}

fn table_scope(table: &TableDef) -> Scope {
    let mut scope = Scope::default();
    let columns = table
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.ty))
        .collect();
    scope.push(table.name.clone(), columns);
    scope
}

fn lookup_table(db: &Database, name: &cairn_sql::Ident) -> Result<TableDef, ExecError> {
    db.catalog.tables.get(&name.node.0).cloned().ok_or_else(|| {
        ExecError::at(
            ErrorKind::NotFound,
            format!("no such table: {}", name.node.0),
            name.span,
        )
    })
}

/// Checks statically that a value of type `ty` may be stored in the
/// column: equal types, NULL, or INTEGER into REAL.
fn check_assignable(
    table: &TableDef,
    column: usize,
    ty: SqlType,
    span: Span,
) -> Result<(), ExecError> {
    let Some(def) = table.columns.get(column) else {
        return Err(ExecError::corrupt("column out of range"));
    };
    if ty == def.ty || ty == SqlType::Null || (ty == SqlType::Integer && def.ty == SqlType::Real) {
        return Ok(());
    }
    Err(ExecError::at(
        ErrorKind::Type,
        format!(
            "type mismatch: column {}.{} is {} but the value is {ty}",
            table.name, def.name, def.ty
        ),
        span,
    ))
}

/// Converts a value to the column's storage type (INTEGER into REAL).
fn coerce(value: Value, ty: SqlType) -> Value {
    match (value, ty) {
        (Value::Integer(v), SqlType::Real) => Value::real(v as f64),
        (value, _) => value,
    }
}

fn constraint(message: String, span: Span) -> ExecError {
    ExecError::at(ErrorKind::Constraint, message, span)
}

fn check_not_null(table: &TableDef, row: &NewRow) -> Result<(), ExecError> {
    for ((column, value), span) in table.columns.iter().zip(&row.values).zip(&row.spans) {
        if column.required() && value.is_null() {
            return Err(constraint(
                format!("NOT NULL constraint failed: {}.{}", table.name, column.name),
                *span,
            ));
        }
    }
    Ok(())
}

fn span_of(row: &NewRow, column: usize) -> Span {
    row.spans.get(column).copied().unwrap_or(Span {
        start: 0,
        end: 0,
        line: 1,
        column: 1,
    })
}

impl Database {
    /// Validates the post-statement state and builds the inserts for
    /// `rows`. `rewritten` holds the keys of stored rows the statement
    /// deletes or replaces.
    fn add_new_rows(
        &mut self,
        table: &TableDef,
        indexes: &[IndexDef],
        rewritten: &BTreeSet<i64>,
        rows: &[NewRow],
        writes: &mut WriteSet,
    ) -> Result<(), ExecError> {
        let mut new_keys = BTreeSet::new();
        let mut new_values: BTreeMap<&str, BTreeSet<OrdRow>> = BTreeMap::new();
        for row in rows {
            if let Some(pk) = table.pk {
                let duplicate = !new_keys.insert(row.key)
                    || (get_row(&mut self.pager, table, row.key)?.is_some()
                        && !rewritten.contains(&row.key));
                if duplicate {
                    return Err(constraint(
                        format!(
                            "PRIMARY KEY constraint failed: {}.{}",
                            table.name,
                            table.column_name(pk)
                        ),
                        span_of(row, pk),
                    ));
                }
            }
            for index in indexes.iter().filter(|i| i.unique) {
                let value = row.values.get(index.column).cloned().unwrap_or(Value::Null);
                if value.is_null() {
                    continue;
                }
                let seen = new_values.entry(index.name.as_str()).or_default();
                let duplicate = !seen.insert(OrdRow(vec![value.clone()]))
                    || index_lookup_keys(&mut self.pager, index, &value)?
                        .iter()
                        .any(|key| !rewritten.contains(key));
                if duplicate {
                    let column = table.columns.get(index.column);
                    let kind = if column.is_some_and(|c| c.primary_key) {
                        "PRIMARY KEY"
                    } else {
                        "UNIQUE"
                    };
                    return Err(constraint(
                        format!(
                            "{kind} constraint failed: {}.{}",
                            table.name,
                            table.column_name(index.column)
                        ),
                        span_of(row, index.column),
                    ));
                }
            }
        }
        for row in rows {
            let span = span_of(row, 0);
            let record = encode_record(&row.values);
            if record.len() > MAX_VALUE_LEN {
                return Err(ExecError::at(
                    ErrorKind::TooLarge,
                    format!(
                        "row too large: {} bytes, maximum {MAX_VALUE_LEN}",
                        record.len()
                    ),
                    span,
                ));
            }
            for index in indexes {
                let value = row.values.get(index.column).cloned().unwrap_or(Value::Null);
                let len = index_key(&value, row.key).len();
                if len > MAX_KEY_LEN {
                    return Err(ExecError::at(
                        ErrorKind::TooLarge,
                        format!(
                            "index key too large for index {}: {len} bytes, maximum {MAX_KEY_LEN}",
                            index.name
                        ),
                        span_of(row, index.column),
                    ));
                }
            }
            writes.insert_row(table, indexes, row.key, record, &row.values);
        }
        Ok(())
    }

    pub(crate) fn insert(&mut self, insert: &Insert) -> Result<QueryResult, ExecError> {
        let table = lookup_table(self, &insert.table)?;
        let targets: Vec<usize> = match &insert.columns {
            None => (0..table.columns.len()).collect(),
            Some(names) => {
                let mut targets = Vec::with_capacity(names.len());
                for name in names {
                    let column = table.column(&name.node.0).ok_or_else(|| {
                        ExecError::at(
                            ErrorKind::NotFound,
                            format!("no such column: {}", name.node.0),
                            name.span,
                        )
                    })?;
                    if targets.contains(&column) {
                        return Err(ExecError::at(
                            ErrorKind::Type,
                            format!("column {} is listed more than once", name.node.0),
                            name.span,
                        ));
                    }
                    targets.push(column);
                }
                targets
            }
        };
        let scope = Scope::default();
        let mut next_rowid = table.next_rowid;
        let mut rows = Vec::with_capacity(insert.rows.len());
        for row in &insert.rows {
            let exprs = &row.node.0;
            if exprs.len() != targets.len() {
                let message = match insert.columns {
                    None => format!(
                        "table {} has {} columns but {} values were supplied",
                        table.name,
                        targets.len(),
                        exprs.len()
                    ),
                    Some(_) => format!(
                        "{} columns were named but {} values were supplied",
                        targets.len(),
                        exprs.len()
                    ),
                };
                return Err(ExecError::at(ErrorKind::Type, message, row.span));
            }
            let mut values = vec![Value::Null; table.columns.len()];
            for (expr, &column) in exprs.iter().zip(&targets) {
                let bound = bind(expr, &scope, &mut Mode::Plain("VALUES"))?;
                check_assignable(&table, column, bound.ty, expr.span)?;
                let ty = table.columns.get(column).map_or(SqlType::Null, |c| c.ty);
                let value = coerce(eval(&bound, &[])?, ty);
                if let Some(slot) = values.get_mut(column) {
                    *slot = value;
                }
            }
            let new_row = NewRow {
                key: 0,
                spans: vec![row.span; table.columns.len()],
                values,
            };
            check_not_null(&table, &new_row)?;
            let key = match table.pk {
                Some(pk) => match new_row.values.get(pk) {
                    Some(Value::Integer(key)) => *key,
                    _ => return Err(ExecError::corrupt("primary key is not an integer")),
                },
                None => {
                    let key = next_rowid;
                    next_rowid = key.checked_add(1).ok_or_else(|| {
                        constraint(
                            format!("row id space exhausted for table {}", table.name),
                            row.span,
                        )
                    })?;
                    key
                }
            };
            rows.push(NewRow { key, ..new_row });
        }
        let indexes = self.catalog.indexes_of(&table.name);
        let mut writes = WriteSet::default();
        self.add_new_rows(&table, &indexes, &BTreeSet::new(), &rows, &mut writes)?;
        if table.pk.is_none() && next_rowid != table.next_rowid {
            writes.next_rowid = Some((table.name.clone(), next_rowid));
        }
        self.apply(writes)?;
        Ok(QueryResult::Affected(rows.len() as u64))
    }

    /// The rows of `table` matching `where_clause`, with their keys, read
    /// through the planner's access path.
    fn matching_rows(
        &mut self,
        table: &TableDef,
        scope: &Scope,
        where_clause: Option<&Expr>,
    ) -> Result<Vec<KeyedRow>, ExecError> {
        let cond = match where_clause {
            Some(w) => Some(bind_condition(
                w,
                scope,
                &mut Mode::Plain("WHERE"),
                "WHERE",
            )?),
            None => None,
        };
        let conds = cond.as_ref().map(conjuncts).unwrap_or_default();
        let indexes = self.catalog.indexes_of(&table.name);
        let access = TableAccess {
            table: table.clone(),
            label: table.name.clone(),
            access: choose_access(table, &indexes, &conds, 0)?,
        };
        let mut matched = Vec::new();
        for (key, row) in fetch(&mut self.pager, &access)? {
            let keep = match &cond {
                Some(cond) => eval_condition(cond, &row)?,
                None => true,
            };
            if keep {
                matched.push((key, row));
            }
        }
        Ok(matched)
    }

    pub(crate) fn update(&mut self, update: &Update) -> Result<QueryResult, ExecError> {
        let table = lookup_table(self, &update.table)?;
        let scope = table_scope(&table);
        let mut assignments = Vec::with_capacity(update.assignments.len());
        let mut spans = vec![update.table.span; table.columns.len()];
        for assignment in &update.assignments {
            let name = &assignment.node.column;
            let column = table.column(&name.node.0).ok_or_else(|| {
                ExecError::at(
                    ErrorKind::NotFound,
                    format!("no such column: {}", name.node.0),
                    name.span,
                )
            })?;
            if assignments.iter().any(|(c, _)| *c == column) {
                return Err(ExecError::at(
                    ErrorKind::Type,
                    format!("column {} is assigned more than once", name.node.0),
                    assignment.span,
                ));
            }
            let value = &assignment.node.value;
            let bound = bind(value, &scope, &mut Mode::Plain("SET"))?;
            check_assignable(&table, column, bound.ty, value.span)?;
            if let Some(slot) = spans.get_mut(column) {
                *slot = assignment.span;
            }
            assignments.push((column, bound));
        }
        let matched = self.matching_rows(&table, &scope, update.where_clause.as_ref())?;
        let mut rows = Vec::with_capacity(matched.len());
        for (key, old) in &matched {
            let mut values = old.clone();
            for (column, expr) in &assignments {
                let ty = table.columns.get(*column).map_or(SqlType::Null, |c| c.ty);
                let value = coerce(eval(expr, old)?, ty);
                if let Some(slot) = values.get_mut(*column) {
                    *slot = value;
                }
            }
            let new_key = match table.pk.and_then(|pk| values.get(pk)) {
                Some(Value::Integer(k)) => *k,
                _ => *key,
            };
            let row = NewRow {
                key: new_key,
                values,
                spans: spans.clone(),
            };
            check_not_null(&table, &row)?;
            rows.push(row);
        }
        let indexes = self.catalog.indexes_of(&table.name);
        let rewritten: BTreeSet<i64> = matched.iter().map(|(key, _)| *key).collect();
        let mut writes = WriteSet::default();
        for (key, old) in &matched {
            writes.delete_row(&table, &indexes, *key, old);
        }
        self.add_new_rows(&table, &indexes, &rewritten, &rows, &mut writes)?;
        self.apply(writes)?;
        Ok(QueryResult::Affected(matched.len() as u64))
    }

    pub(crate) fn delete(&mut self, delete: &Delete) -> Result<QueryResult, ExecError> {
        let table = lookup_table(self, &delete.table)?;
        let scope = table_scope(&table);
        let matched = self.matching_rows(&table, &scope, delete.where_clause.as_ref())?;
        let indexes = self.catalog.indexes_of(&table.name);
        let mut writes = WriteSet::default();
        for (key, row) in &matched {
            writes.delete_row(&table, &indexes, *key, row);
        }
        self.apply(writes)?;
        Ok(QueryResult::Affected(matched.len() as u64))
    }
}
