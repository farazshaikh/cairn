//! The public entry point: [`Database`] and [`QueryResult`].
//!
//! # Transactions
//!
//! A handle is either in autocommit mode or inside an explicit transaction
//! started by `BEGIN`:
//!
//! - In autocommit mode every write statement runs in its own storage
//!   transaction, committed when it succeeds and rolled back when it fails.
//!   Read statements (SELECT, EXPLAIN, `PRAGMA integrity_check`) take no
//!   lock; each sees one committed state for its whole run.
//! - After `BEGIN` the handle holds the file's single write lock until
//!   `COMMIT` or `ROLLBACK`. Each write statement runs under a savepoint, so
//!   one that fails is undone on its own and the transaction stays open.
//!   `ROLLBACK` undoes everything since `BEGIN`, catalog changes included.
//!
//! Other handles see only committed changes; a write from another handle
//! while the lock is held fails with [`ErrorKind::Busy`]. The in-memory
//! catalog is reloaded whenever the committed state it was read from
//! changes or a rollback discards catalog changes.

use std::path::Path;
use std::sync::Arc;

use cairn_sql::{Span, Statement, StatementKind};
use cairn_storage::{Options, OsVfs, Pager, Vfs};

use crate::catalog::{CATALOG_ROOT, Catalog};
use crate::error::{ErrorKind, ExecError};
use crate::explain;
use crate::integrity;
use crate::plan::run;
use crate::prepass::{self, Item, Pragma};
use crate::select::plan_select;
use crate::table::{WriteSet, apply};
use crate::value::Value;

/// Most problems listed by `PRAGMA integrity_check` before a summary row.
const MAX_INTEGRITY_ROWS: usize = 100;

/// The result of one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// Rows of a SELECT, EXPLAIN or PRAGMA, with the output column names.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
    /// Rows changed by INSERT, UPDATE or DELETE; 0 for DDL and for BEGIN,
    /// COMMIT and ROLLBACK.
    Affected(u64),
}

/// An open cairn database file. Several handles may share one file within
/// a process; see the module documentation for the transaction rules.
pub struct Database {
    pub(crate) pager: Pager,
    pub(crate) catalog: Catalog,
    /// Committed state the catalog was loaded from; 0 forces a reload.
    catalog_seq: u64,
    /// Whether an explicit transaction (BEGIN) is open.
    explicit: bool,
}

impl Database {
    /// Creates a new database file; fails if the file exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Database, ExecError> {
        Database::create_with(Arc::new(OsVfs), path.as_ref(), Options::default())
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Database, ExecError> {
        Database::open_with(Arc::new(OsVfs), path.as_ref(), Options::default())
    }

    /// Creates a new database file through `vfs` (for example the
    /// fault-injecting `cairn_storage::fault::FaultVfs`).
    pub fn create_with(
        vfs: Arc<dyn Vfs>,
        path: &Path,
        options: Options,
    ) -> Result<Database, ExecError> {
        let mut pager = Pager::create_with(vfs, path, options)?;
        let catalog = Catalog::create(&mut pager)?;
        pager.set_root(CATALOG_ROOT, catalog.root)?;
        pager.sync()?;
        let catalog_seq = pager.snapshot_seq();
        Ok(Database {
            pager,
            catalog,
            catalog_seq,
            explicit: false,
        })
    }

    /// Opens a database file through `vfs`, replaying its log if no other
    /// handle in the process has it open.
    pub fn open_with(
        vfs: Arc<dyn Vfs>,
        path: &Path,
        options: Options,
    ) -> Result<Database, ExecError> {
        let mut pager = Pager::open_with(vfs, path, options)?;
        pager.begin_read()?;
        let catalog = Catalog::load(&mut pager);
        let catalog_seq = pager.snapshot_seq();
        pager.end_read();
        Ok(Database {
            pager,
            catalog: catalog?,
            catalog_seq,
            explicit: false,
        })
    }

    /// Whether `BEGIN` has opened a transaction that is still active.
    pub fn in_transaction(&self) -> bool {
        self.explicit
    }

    /// Runs every statement in `sql` in order and stops at the first error.
    /// Statements before the error keep their effect: committed in autocommit
    /// mode, still pending inside a transaction (which stays open).
    pub fn execute(&mut self, sql: &str) -> Result<Vec<QueryResult>, ExecError> {
        self.run(sql, true)?.into_iter().collect()
    }

    /// Runs every statement in `sql` and returns each statement's outcome,
    /// continuing after errors. The outer error is a syntax error, and then
    /// nothing ran.
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
        let pre = prepass::preprocess(sql).map_err(|e| e.with_source(sql))?;
        let statements = if pre.has_statements {
            cairn_sql::parse(&pre.text).map_err(|e| ExecError::from_sql(e, sql))?
        } else {
            Vec::new()
        };
        let items = pre.items(&statements).map_err(|e| e.with_source(sql))?;
        let mut results = Vec::with_capacity(items.len());
        for (item, marker) in items {
            let result = self.item(item, marker).map_err(|e| e.with_source(sql));
            let failed = result.is_err();
            results.push(result);
            if failed && stop_on_error {
                break;
            }
        }
        Ok(results)
    }

    fn item(&mut self, item: Item<'_>, explain: Option<Span>) -> Result<QueryResult, ExecError> {
        let select =
            matches!(item, Item::Statement(s) if matches!(s.node, StatementKind::Select(_)));
        if let (Some(marker), false) = (explain, select) {
            return Err(ExecError::at(
                ErrorKind::Unsupported,
                "EXPLAIN supports only SELECT",
                marker,
            ));
        }
        match item {
            Item::Pragma(pragma) => self.pragma(pragma),
            Item::Statement(statement) => self.statement(statement, explain.is_some()),
        }
    }

    fn statement(
        &mut self,
        statement: &Statement,
        explain: bool,
    ) -> Result<QueryResult, ExecError> {
        match &statement.node {
            StatementKind::Begin => self.begin(statement.span),
            StatementKind::Commit => self.commit(statement.span),
            StatementKind::Rollback => self.rollback(statement.span),
            StatementKind::Select(select) => self.read(|db| {
                let planned = plan_select(select, &db.catalog)?;
                if explain {
                    return Ok(explain::render(&planned.plan));
                }
                let rows = run(&planned.plan, &mut db.pager)?;
                Ok(QueryResult::Rows {
                    columns: planned.columns,
                    rows,
                })
            }),
            StatementKind::CreateTable(create) => self.write(|db| db.create_table(create)),
            StatementKind::DropTable(drop) => self.write(|db| db.drop_table(drop)),
            StatementKind::CreateIndex(create) => self.write(|db| db.create_index(create)),
            StatementKind::Insert(insert) => self.write(|db| db.insert(insert)),
            StatementKind::Update(update) => self.write(|db| db.update(update)),
            StatementKind::Delete(delete) => self.write(|db| db.delete(delete)),
        }
    }

    fn begin(&mut self, span: Span) -> Result<QueryResult, ExecError> {
        if self.explicit {
            return Err(transaction_error(
                "cannot BEGIN: a transaction is already active",
                span,
            ));
        }
        self.pager.begin()?;
        self.explicit = true;
        if let Err(error) = self.refresh() {
            self.explicit = false;
            let _ = self.pager.rollback();
            return Err(error);
        }
        Ok(QueryResult::Affected(0))
    }

    fn commit(&mut self, span: Span) -> Result<QueryResult, ExecError> {
        if !self.explicit {
            return Err(transaction_error(
                "cannot COMMIT: no transaction is active",
                span,
            ));
        }
        self.explicit = false;
        self.finish_commit()?;
        Ok(QueryResult::Affected(0))
    }

    fn rollback(&mut self, span: Span) -> Result<QueryResult, ExecError> {
        if !self.explicit {
            return Err(transaction_error(
                "cannot ROLLBACK: no transaction is active",
                span,
            ));
        }
        self.explicit = false;
        self.catalog_seq = 0;
        self.pager.rollback()?;
        Ok(QueryResult::Affected(0))
    }

    /// Commits the pager's transaction. On failure it was rolled back, so
    /// the catalog may hold changes that never committed.
    fn finish_commit(&mut self) -> Result<(), ExecError> {
        match self.pager.commit() {
            Ok(()) => {
                self.catalog_seq = self.pager.snapshot_seq();
                Ok(())
            }
            Err(error) => {
                self.catalog_seq = 0;
                Err(error.into())
            }
        }
    }

    /// Runs a read-only statement against one committed state (or inside the
    /// open transaction).
    fn read<T>(
        &mut self,
        f: impl FnOnce(&mut Database) -> Result<T, ExecError>,
    ) -> Result<T, ExecError> {
        self.pager.begin_read()?;
        let result = self.refresh().and_then(|()| f(self));
        self.pager.end_read();
        result
    }

    /// Runs a write statement: in its own transaction in autocommit mode,
    /// under a savepoint inside an explicit transaction. Either way a
    /// failure leaves no trace of the statement.
    fn write<T>(
        &mut self,
        f: impl FnOnce(&mut Database) -> Result<T, ExecError>,
    ) -> Result<T, ExecError> {
        if self.explicit {
            let savepoint = self.pager.savepoint()?;
            let result = self.refresh().and_then(|()| f(self));
            if result.is_ok() {
                self.pager.release(savepoint)?;
            } else {
                self.catalog_seq = 0;
                self.pager.rollback_to(savepoint)?;
            }
            return result;
        }
        self.pager.begin()?;
        match self.refresh().and_then(|()| f(self)) {
            Ok(value) => {
                self.finish_commit()?;
                Ok(value)
            }
            Err(error) => {
                self.catalog_seq = 0;
                let _ = self.pager.rollback();
                Err(error)
            }
        }
    }

    /// Reloads the catalog if the state it describes changed.
    fn refresh(&mut self) -> Result<(), ExecError> {
        let seq = self.pager.snapshot_seq();
        if self.catalog_seq != seq {
            self.catalog = Catalog::load(&mut self.pager)?;
            self.catalog_seq = seq;
        }
        Ok(())
    }

    fn pragma(&mut self, pragma: &Pragma) -> Result<QueryResult, ExecError> {
        match pragma.name.as_str() {
            "checkpoint" => {
                if self.explicit {
                    return Err(transaction_error(
                        "cannot checkpoint inside a transaction",
                        pragma.span,
                    ));
                }
                self.pager.checkpoint()?;
                Ok(single_column("checkpoint", vec!["ok".to_string()]))
            }
            "integrity_check" => {
                let mut problems =
                    self.read(|db| Ok(integrity::collect(&mut db.pager, &db.catalog)))?;
                if problems.is_empty() {
                    problems.push("ok".to_string());
                }
                if problems.len() > MAX_INTEGRITY_ROWS {
                    let more = problems.len() - MAX_INTEGRITY_ROWS;
                    problems.truncate(MAX_INTEGRITY_ROWS);
                    problems.push(format!("and {more} more problems"));
                }
                Ok(single_column("integrity_check", problems))
            }
            other => Err(ExecError::at(
                ErrorKind::Unsupported,
                format!("unknown pragma: {other}"),
                pragma.name_span,
            )),
        }
    }

    /// Runs the write phase of a statement. A storage error part-way through
    /// is undone by the enclosing savepoint or transaction rollback.
    pub(crate) fn guarded<T>(
        &mut self,
        write: impl FnOnce(&mut Pager, &mut Catalog) -> Result<T, ExecError>,
    ) -> Result<T, ExecError> {
        write(&mut self.pager, &mut self.catalog)
    }

    pub(crate) fn apply(&mut self, writes: WriteSet) -> Result<(), ExecError> {
        self.guarded(|pager, catalog| apply(pager, catalog, writes))
    }

    /// Verifies every table and index and returns the first problem: B-tree
    /// structure, record decoding, INTEGER PRIMARY KEY values equal to row
    /// keys, index entries exactly matching the rows, unique indexes free of
    /// duplicates, row ids below the next row id, and page accounting (no
    /// page shared by two trees, free and in use, or leaked).
    /// `PRAGMA integrity_check` reports every problem instead.
    pub fn check(&mut self) -> Result<(), ExecError> {
        let problems = self.read(|db| Ok(integrity::collect(&mut db.pager, &db.catalog)))?;
        match problems.into_iter().next() {
            Some(problem) => Err(ExecError::corrupt(problem)),
            None => Ok(()),
        }
    }

    /// Discards an open transaction, then checkpoints and closes the handle,
    /// reporting errors that dropping would hide. Dropping a handle also
    /// discards an open transaction.
    pub fn close(mut self) -> Result<(), ExecError> {
        if self.explicit {
            self.pager.rollback()?;
        }
        self.pager.close()?;
        Ok(())
    }
}

fn transaction_error(message: &str, span: Span) -> ExecError {
    ExecError::at(ErrorKind::Transaction, message, span)
}

fn single_column(name: &str, values: Vec<String>) -> QueryResult {
    QueryResult::Rows {
        columns: vec![name.to_string()],
        rows: values.into_iter().map(|v| vec![Value::Text(v)]).collect(),
    }
}
