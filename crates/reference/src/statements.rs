//! CREATE TABLE, DROP TABLE, CREATE INDEX, INSERT, UPDATE and DELETE
//! (README "Statements" and "Keys, constraints and atomicity").
//!
//! Every statement works on a copy of the state that replaces the real
//! one only when the whole statement succeeds, so a failing statement
//! changes nothing. Uniqueness is judged on the state after the statement.

use cairn_sql::{CreateIndex, CreateTable, Delete, DropTable, Expr, Insert, Update};

use crate::RErrorKind;
use crate::expr::{Context, Scope, Typed, check, check_condition, eval, fit};
use crate::select::{choose_path, conjuncts, fetch};
use crate::state::{Column, Index, State, Table, implicit_index_name, valid_name};
use crate::value::{RValue, SType, order};

fn name_ok(name: &str) -> Result<(), RErrorKind> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(RErrorKind::Type)
    }
}

pub(crate) fn create_table(state: &mut State, create: &CreateTable) -> Result<u64, RErrorKind> {
    let name = &create.name.node.0;
    name_ok(name)?;
    if state.tables.contains_key(name) {
        return if create.if_not_exists {
            Ok(0)
        } else {
            Err(RErrorKind::AlreadyExists)
        };
    }
    let mut columns: Vec<Column> = Vec::new();
    for def in &create.columns {
        let column = &def.node;
        name_ok(&column.name.node.0)?;
        if columns.iter().any(|c| c.name == column.name.node.0) {
            return Err(RErrorKind::AlreadyExists);
        }
        if column.primary_key && columns.iter().any(|c| c.primary_key) {
            return Err(RErrorKind::Type);
        }
        columns.push(Column {
            name: column.name.node.0.clone(),
            ty: SType::of(column.data_type.node),
            primary_key: column.primary_key,
            not_null: column.not_null,
            unique: column.unique,
        });
    }
    if columns.len() > 255 {
        return Err(RErrorKind::TooLarge);
    }
    let int_pk = columns
        .iter()
        .position(|c| c.primary_key && c.ty == SType::Integer);
    for (ordinal, column) in columns.iter().enumerate() {
        if Some(ordinal) != int_pk && (column.unique || column.primary_key) {
            let index_name = implicit_index_name(name, &column.name, state);
            state.indexes.insert(
                index_name,
                Index {
                    table: name.clone(),
                    column: ordinal,
                    unique: true,
                },
            );
        }
    }
    state.tables.insert(
        name.clone(),
        Table {
            name: name.clone(),
            columns,
            int_pk,
            rows: Vec::new(),
            next_rowid: 1,
        },
    );
    Ok(0)
}

pub(crate) fn drop_table(state: &mut State, drop: &DropTable) -> Result<u64, RErrorKind> {
    let name = &drop.name.node.0;
    if state.tables.remove(name).is_none() {
        return if drop.if_exists {
            Ok(0)
        } else {
            Err(RErrorKind::NotFound)
        };
    }
    state.indexes.retain(|_, index| index.table != *name);
    Ok(0)
}

pub(crate) fn create_index(state: &mut State, create: &CreateIndex) -> Result<u64, RErrorKind> {
    let name = &create.name.node.0;
    name_ok(name)?;
    if name.starts_with("cairn_") {
        return Err(RErrorKind::Type);
    }
    if state.indexes.contains_key(name) {
        return Err(RErrorKind::AlreadyExists);
    }
    let table = state
        .tables
        .get(&create.table.node.0)
        .ok_or(RErrorKind::NotFound)?;
    let column = table
        .column(&create.column.node.0)
        .ok_or(RErrorKind::NotFound)?;
    if create.unique {
        let values: Vec<&RValue> = table
            .rows
            .iter()
            .filter_map(|(_, row)| row.get(column))
            .filter(|v| !v.is_null())
            .collect();
        if has_duplicate(&values) {
            return Err(RErrorKind::Constraint);
        }
    }
    let table = table.name.clone();
    state.indexes.insert(
        name.clone(),
        Index {
            table,
            column,
            unique: create.unique,
        },
    );
    Ok(0)
}

fn has_duplicate(values: &[&RValue]) -> bool {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| order(a, b));
    sorted.windows(2).any(|pair| match pair {
        [a, b] => order(a, b).is_eq(),
        _ => false,
    })
}

fn find_table<'a>(state: &'a State, name: &str) -> Result<&'a Table, RErrorKind> {
    state.tables.get(name).ok_or(RErrorKind::NotFound)
}

/// INTEGER into a REAL column, equal types, or NULL.
fn assignable(column: &Column, ty: SType) -> Result<(), RErrorKind> {
    if ty == column.ty || ty == SType::Null || (ty == SType::Integer && column.ty == SType::Real) {
        Ok(())
    } else {
        Err(RErrorKind::Type)
    }
}

fn not_null(table: &Table, values: &[RValue]) -> Result<(), RErrorKind> {
    let missing = table
        .columns
        .iter()
        .zip(values)
        .any(|(column, value)| column.required() && value.is_null());
    if missing {
        Err(RErrorKind::Constraint)
    } else {
        Ok(())
    }
}

/// Replaces the rows with keys in `removed` by `added` after checking
/// PRIMARY KEY and every unique index on the resulting table.
fn commit_rows(
    state: &mut State,
    table_name: &str,
    removed: &[i64],
    added: Vec<(i64, Vec<RValue>)>,
) -> Result<(), RErrorKind> {
    let unique: Vec<usize> = state
        .indexes_of(table_name)
        .iter()
        .filter(|i| i.unique)
        .map(|i| i.column)
        .collect();
    let table = state
        .tables
        .get_mut(table_name)
        .ok_or(RErrorKind::NotFound)?;
    let mut rows: Vec<(i64, Vec<RValue>)> = table
        .rows
        .iter()
        .filter(|(key, _)| !removed.contains(key))
        .cloned()
        .collect();
    rows.extend(added);
    let mut keys: Vec<i64> = rows.iter().map(|(key, _)| *key).collect();
    keys.sort_unstable();
    if keys.windows(2).any(|pair| pair.first() == pair.get(1)) {
        return Err(RErrorKind::Constraint);
    }
    for column in unique {
        let values: Vec<&RValue> = rows
            .iter()
            .filter_map(|(_, row)| row.get(column))
            .filter(|v| !v.is_null())
            .collect();
        if has_duplicate(&values) {
            return Err(RErrorKind::Constraint);
        }
    }
    table.rows = rows;
    table.sort_rows();
    Ok(())
}

pub(crate) fn insert(state: &mut State, insert: &Insert) -> Result<u64, RErrorKind> {
    let table = find_table(state, &insert.table.node.0)?;
    let targets: Vec<usize> = match &insert.columns {
        None => (0..table.columns.len()).collect(),
        Some(names) => {
            let mut targets = Vec::with_capacity(names.len());
            for name in names {
                let column = table.column(&name.node.0).ok_or(RErrorKind::NotFound)?;
                if targets.contains(&column) {
                    return Err(RErrorKind::Type);
                }
                targets.push(column);
            }
            targets
        }
    };
    let mut next_rowid = table.next_rowid;
    let mut added = Vec::with_capacity(insert.rows.len());
    for row in &insert.rows {
        let exprs = &row.node.0;
        if exprs.len() != targets.len() {
            return Err(RErrorKind::Type);
        }
        let mut values = vec![RValue::Null; table.columns.len()];
        for (expr, &target) in exprs.iter().zip(&targets) {
            let typed = check(expr, &Scope::default(), &mut Context::Values)?;
            let column = table.columns.get(target).ok_or(RErrorKind::NotFound)?;
            assignable(column, typed.ty)?;
            let value = fit(eval(&typed, &[])?, column.ty);
            if let Some(slot) = values.get_mut(target) {
                *slot = value;
            }
        }
        not_null(table, &values)?;
        let key = match table.int_pk {
            Some(pk) => match values.get(pk) {
                Some(RValue::Integer(key)) => *key,
                _ => return Err(RErrorKind::Constraint),
            },
            None => {
                let key = next_rowid;
                next_rowid = key.checked_add(1).ok_or(RErrorKind::Constraint)?;
                key
            }
        };
        added.push((key, values));
    }
    let count = added.len() as u64;
    let name = table.name.clone();
    commit_rows(state, &name, &[], added)?;
    if let Some(table) = state.tables.get_mut(&name) {
        table.next_rowid = next_rowid;
    }
    Ok(count)
}

/// The rows a WHERE clause selects, read through the planner's access
/// path so that only the rows cairn reads are evaluated.
fn matching(
    state: &State,
    table: &Table,
    scope: &Scope,
    where_clause: Option<&Expr>,
) -> Result<Vec<(i64, Vec<RValue>)>, RErrorKind> {
    let cond: Option<Typed> = match where_clause {
        Some(w) => Some(check_condition(w, scope, &mut Context::Plain)?),
        None => None,
    };
    let conds = cond.as_ref().map(conjuncts).unwrap_or_default();
    let path = choose_path(state, table, &conds)?;
    let mut out = Vec::new();
    for (key, row) in fetch(table, &path) {
        let keep = match &cond {
            Some(cond) => crate::expr::passes(cond, &row)?,
            None => true,
        };
        if keep {
            out.push((key, row));
        }
    }
    Ok(out)
}

fn table_scope(table: &Table) -> Scope {
    let mut scope = Scope::default();
    scope.add(table.name.clone(), table.column_types());
    scope
}

pub(crate) fn update(state: &mut State, update: &Update) -> Result<u64, RErrorKind> {
    let table = find_table(state, &update.table.node.0)?;
    let scope = table_scope(table);
    let mut assignments: Vec<(usize, Typed)> = Vec::new();
    for assignment in &update.assignments {
        let column = table
            .column(&assignment.node.column.node.0)
            .ok_or(RErrorKind::NotFound)?;
        if assignments.iter().any(|(c, _)| *c == column) {
            return Err(RErrorKind::Type);
        }
        let typed = check(&assignment.node.value, &scope, &mut Context::Plain)?;
        let def = table.columns.get(column).ok_or(RErrorKind::NotFound)?;
        assignable(def, typed.ty)?;
        assignments.push((column, typed));
    }
    let matched = matching(state, table, &scope, update.where_clause.as_ref())?;
    let mut added = Vec::with_capacity(matched.len());
    for (key, old) in &matched {
        let mut values = old.clone();
        for (column, typed) in &assignments {
            let ty = table.columns.get(*column).map_or(SType::Null, |c| c.ty);
            let value = fit(eval(typed, old)?, ty);
            if let Some(slot) = values.get_mut(*column) {
                *slot = value;
            }
        }
        not_null(table, &values)?;
        let new_key = match table.int_pk.and_then(|pk| values.get(pk)) {
            Some(RValue::Integer(k)) => *k,
            _ => *key,
        };
        added.push((new_key, values));
    }
    let removed: Vec<i64> = matched.iter().map(|(key, _)| *key).collect();
    let name = table.name.clone();
    commit_rows(state, &name, &removed, added)?;
    Ok(removed.len() as u64)
}

pub(crate) fn delete(state: &mut State, delete: &Delete) -> Result<u64, RErrorKind> {
    let table = find_table(state, &delete.table.node.0)?;
    let scope = table_scope(table);
    let matched = matching(state, table, &scope, delete.where_clause.as_ref())?;
    let removed: Vec<i64> = matched.iter().map(|(key, _)| *key).collect();
    let name = table.name.clone();
    commit_rows(state, &name, &removed, Vec::new())?;
    Ok(removed.len() as u64)
}
