//! The public entry point: [`Database`] and [`QueryResult`].

use std::collections::BTreeSet;
use std::path::Path;

use cairn_sql::{Span, Statement, StatementKind};
use cairn_storage::{BTree, Pager};

use crate::catalog::{CATALOG_ROOT, Catalog};
use crate::codec::key::decode_index_key;
use crate::error::{ErrorKind, ExecError};
use crate::explain;
use crate::plan::run;
use crate::select::plan_select;
use crate::table::{WriteSet, apply, scan};
use crate::value::{OrdRow, Value};

/// The result of one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// Rows of a SELECT or EXPLAIN, with the output column names.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// Rows changed by INSERT, UPDATE or DELETE; 0 for DDL.
    Affected(u64),
}

/// An open cairn database file.
///
/// Statements run one at a time and are not grouped into transactions:
/// each statement is atomic with respect to its own errors (it validates
/// everything before writing), and changes are synced to disk at the end
/// of every `execute` call.
pub struct Database {
    pub(crate) pager: Pager,
    pub(crate) catalog: Catalog,
    unusable: bool,
}

impl Database {
    /// Creates a new database file; fails if the file exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Database, ExecError> {
        let mut pager = Pager::create(path)?;
        let catalog = Catalog::create(&mut pager)?;
        pager.set_root(CATALOG_ROOT, catalog.root)?;
        pager.sync()?;
        Ok(Database {
            pager,
            catalog,
            unusable: false,
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Database, ExecError> {
        let mut pager = Pager::open(path)?;
        let catalog = Catalog::load(&mut pager)?;
        Ok(Database {
            pager,
            catalog,
            unusable: false,
        })
    }

    /// Runs every statement in `sql` in order and stops at the first error.
    /// Statements before the error stay applied.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>, ExecError> {
        self.run(sql, true)?.into_iter().collect()
    }

    /// Runs every statement in `sql` and returns each statement's outcome,
    /// continuing after errors. The outer error is a syntax error (nothing
    /// ran) or a failure to sync.
    pub fn execute_each(
        &mut self,
        sql: &str,
    ) -> Result<Vec<Result<QueryResult, ExecError>>, ExecError> {
        self.run(sql, false)
    }

    fn run(
        &mut self,
        sql: &str,
        stop_on_error: bool,
    ) -> Result<Vec<Result<QueryResult, ExecError>>, ExecError> {
        let pre = explain::preprocess(sql).map_err(|e| ExecError::from_sql(e, sql))?;
        let statements = cairn_sql::parse(&pre.text).map_err(|e| ExecError::from_sql(e, sql))?;
        let markers = pre.assign(&statements).map_err(|e| e.with_source(sql))?;
        let mut results = Vec::with_capacity(statements.len());
        for (statement, marker) in statements.iter().zip(markers) {
            let result = self
                .statement(statement, marker)
                .map_err(|e| e.with_source(sql));
            let failed = result.is_err();
            results.push(result);
            if failed && stop_on_error {
                break;
            }
        }
        if let Err(error) = self.pager.sync() {
            self.unusable = true;
            return Err(error.into());
        }
        Ok(results)
    }

    fn statement(
        &mut self,
        statement: &Statement,
        explain: Option<Span>,
    ) -> Result<QueryResult, ExecError> {
        if self.unusable {
            return Err(ExecError::unusable());
        }
        if let (Some(marker), false) = (explain, matches!(statement.node, StatementKind::Select(_)))
        {
            return Err(ExecError::at(
                ErrorKind::Unsupported,
                "EXPLAIN supports only SELECT",
                marker,
            ));
        }
        match &statement.node {
            StatementKind::Select(select) => {
                let planned = plan_select(select, &self.catalog)?;
                if explain.is_some() {
                    return Ok(explain::render(&planned.plan));
                }
                let rows = run(&planned.plan, &mut self.pager)?;
                Ok(QueryResult::Rows {
                    columns: planned.columns,
                    rows,
                })
            }
            StatementKind::CreateTable(create) => self.create_table(create),
            StatementKind::DropTable(drop) => self.drop_table(drop),
            StatementKind::CreateIndex(create) => self.create_index(create),
            StatementKind::Insert(insert) => self.insert(insert),
            StatementKind::Update(update) => self.update(update),
            StatementKind::Delete(delete) => self.delete(delete),
            StatementKind::Begin | StatementKind::Commit | StatementKind::Rollback => {
                let word = match statement.node {
                    StatementKind::Begin => "BEGIN",
                    StatementKind::Commit => "COMMIT",
                    _ => "ROLLBACK",
                };
                Err(ExecError::at(
                    ErrorKind::Unsupported,
                    format!("{word} is not supported yet"),
                    statement.span,
                ))
            }
        }
    }

    /// Runs the write phase. Any error leaves trees possibly half-written,
    /// so the database refuses further statements until reopened.
    pub(crate) fn guarded<T>(
        &mut self,
        write: impl FnOnce(&mut Pager, &mut Catalog) -> Result<T, ExecError>,
    ) -> Result<T, ExecError> {
        let result = write(&mut self.pager, &mut self.catalog);
        if result.is_err() {
            self.unusable = true;
        }
        result
    }

    pub(crate) fn apply(&mut self, writes: WriteSet) -> Result<(), ExecError> {
        self.guarded(|pager, catalog| apply(pager, catalog, writes))
    }

    /// Verifies every table and index: B-tree structure, record decoding,
    /// INTEGER PRIMARY KEY values equal to row keys, index entries exactly
    /// matching the rows, unique indexes free of duplicates, and row ids
    /// below the next row id.
    pub fn check(&mut self) -> Result<(), ExecError> {
        let corrupt = |message: String| ExecError::corrupt(message);
        BTree::open(self.catalog.root)
            .check(&mut self.pager)
            .map_err(|e| corrupt(format!("catalog: {e}")))?;
        for table in self.catalog.tables.values() {
            BTree::open(table.root)
                .check(&mut self.pager)
                .map_err(|e| corrupt(format!("table {}: {e}", table.name)))?;
            let rows = scan(&mut self.pager, table)?;
            for (key, row) in &rows {
                match table.pk {
                    Some(pk) if row.get(pk) != Some(&Value::Integer(*key)) => {
                        return Err(corrupt(format!(
                            "table {}: key {key} differs from its primary key",
                            table.name
                        )));
                    }
                    None if *key >= table.next_rowid => {
                        return Err(corrupt(format!(
                            "table {}: row id {key} not below next row id",
                            table.name
                        )));
                    }
                    _ => {}
                }
            }
            for index in self.catalog.indexes_of(&table.name) {
                let tree = BTree::open(index.root);
                tree.check(&mut self.pager)
                    .map_err(|e| corrupt(format!("index {}: {e}", index.name)))?;
                let ty = table
                    .columns
                    .get(index.column)
                    .map_or(crate::value::SqlType::Null, |c| c.ty);
                let mut stored = BTreeSet::new();
                for pair in tree.range(
                    &mut self.pager,
                    std::ops::Bound::Unbounded,
                    std::ops::Bound::Unbounded,
                )? {
                    let (key, _) = pair?;
                    let (value, row) = decode_index_key(&key, ty)
                        .map_err(|e| corrupt(format!("index {}: {e}", index.name)))?;
                    stored.insert((OrdRow(vec![value]), row));
                }
                let expected: BTreeSet<(OrdRow, i64)> = rows
                    .iter()
                    .map(|(key, row)| {
                        let value = row.get(index.column).cloned().unwrap_or(Value::Null);
                        (OrdRow(vec![value]), *key)
                    })
                    .collect();
                if stored != expected {
                    return Err(corrupt(format!(
                        "index {} does not match table {}",
                        index.name, table.name
                    )));
                }
                if index.unique {
                    let mut seen = BTreeSet::new();
                    for (value, _) in &stored {
                        if !value.0.iter().all(Value::is_null) && !seen.insert(value.clone()) {
                            return Err(corrupt(format!(
                                "unique index {} has duplicates",
                                index.name
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Syncs and closes the file, reporting errors that dropping would hide.
    pub fn close(self) -> Result<(), ExecError> {
        self.pager.close()?;
        Ok(())
    }
}
