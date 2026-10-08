//! An independent, in-memory reference evaluator for the cairn SQL subset.
//!
//! It is written from the semantics in the repository README (sections
//! "SQL", "Querying" and "Transactions and durability"), not from
//! `cairn-exec`, and depends only on `cairn-sql` for parsing. The
//! differential fuzzer in `cairn-fuzz` runs generated SQL through both and
//! compares every result.
//!
//! # What it implements
//!
//! - strict static typing before any row is read, with INTEGER into REAL
//!   as the only conversion;
//! - three-valued logic, checked INTEGER arithmetic, finite REALs and
//!   `-0.0` stored as `0.0`;
//! - `LIKE`, `CASE`, `COALESCE`, `LOWER`, `UPPER`, `LENGTH`, `ABS` and the
//!   aggregates; integer `SUM` fails only when the total does not fit;
//! - inner and left joins, `WHERE`, `GROUP BY`, `HAVING`, `DISTINCT`,
//!   `ORDER BY` (by position, alias or expression), `LIMIT`/`OFFSET`;
//! - `CREATE TABLE`, `DROP TABLE`, `CREATE [UNIQUE] INDEX`, `INSERT`,
//!   `UPDATE`, `DELETE` with `NOT NULL`, `PRIMARY KEY` and `UNIQUE` judged
//!   on the state after the statement, and statement atomicity;
//! - `BEGIN`, `COMMIT`, `ROLLBACK` and their error cases;
//! - the README planner rules, which fix row order without `ORDER BY` and
//!   which rows an expression is evaluated on.
//!
//! # Gaps
//!
//! - `EXPLAIN` and `PRAGMA` are checked (static errors, misplaced
//!   `PRAGMA checkpoint`) but their rows are not produced:
//!   [`Outcome::Skipped`].
//! - Storage limits (encoded rows over 1024 bytes, index keys over 256
//!   bytes) and storage, corruption and concurrency errors do not exist.
//! - Errors carry only a kind, not a message or position.
#![warn(missing_docs)]

mod expr;
mod prepass;
mod select;
mod state;
mod statements;
mod value;

use cairn_sql::{Span, Statement, StatementKind, parse};

pub use value::RValue;

use state::State;

/// The class of a reference error, named like `cairn_exec::ErrorKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RErrorKind {
    /// The text does not tokenize or parse, or a directive is malformed.
    Syntax,
    /// A table, column, alias or function does not exist.
    NotFound,
    /// A table, index or column name is already in use.
    AlreadyExists,
    /// A type mismatch or an otherwise invalid statement.
    Type,
    /// NOT NULL, PRIMARY KEY or UNIQUE failed.
    Constraint,
    /// Integer or real overflow, or division by zero.
    Arithmetic,
    /// A statement the subset does not run (non-SELECT EXPLAIN, an unknown
    /// pragma).
    Unsupported,
    /// A table with more than 255 columns.
    TooLarge,
    /// BEGIN, COMMIT, ROLLBACK or a checkpoint in the wrong state.
    Transaction,
}

/// The result of one statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Output rows with their column names.
    Rows {
        /// Output column names.
        columns: Vec<String>,
        /// The rows, in output order.
        rows: Vec<Vec<RValue>>,
    },
    /// Rows changed by INSERT, UPDATE or DELETE; 0 for DDL and transaction
    /// statements.
    Affected(u64),
    /// The statement failed and changed nothing.
    Error(RErrorKind),
    /// An `EXPLAIN` or `PRAGMA` that passed its checks; its rows are not
    /// modelled.
    Skipped,
}

/// A reference database.
#[derive(Debug, Default)]
pub struct Engine {
    state: State,
    /// The state at `BEGIN`, while a transaction is open.
    saved: Option<State>,
}

impl Engine {
    /// An empty database.
    pub fn new() -> Engine {
        Engine::default()
    }

    /// Whether `BEGIN` has opened a transaction that is still active.
    pub fn in_transaction(&self) -> bool {
        self.saved.is_some()
    }

    /// Runs a script like `cairn_exec::Database::execute_each`: an input
    /// that does not parse runs nothing and is `Err(Syntax)`; otherwise
    /// every statement runs and has its own outcome.
    pub fn run_script(&mut self, sql: &str) -> Result<Vec<Outcome>, RErrorKind> {
        let directives = prepass::scan(sql)?;
        let statements = if directives.has_sql {
            parse(&directives.text).map_err(|_| RErrorKind::Syntax)?
        } else {
            Vec::new()
        };
        let items = items(&statements, &directives)?;
        Ok(items.into_iter().map(|item| self.item(item)).collect())
    }

    fn item(&mut self, item: Item) -> Outcome {
        let result = match item {
            Item::Pragma(name) => self.pragma(name),
            Item::ExplainedPragma => Err(RErrorKind::Unsupported),
            Item::Statement(statement, true) => match &statement.node {
                StatementKind::Select(select) => {
                    select::plan(&self.state, select).map(|_| Outcome::Skipped)
                }
                _ => Err(RErrorKind::Unsupported),
            },
            Item::Statement(statement, false) => self.statement(statement),
        };
        result.unwrap_or_else(Outcome::Error)
    }

    fn pragma(&self, name: &str) -> Result<Outcome, RErrorKind> {
        match name {
            "checkpoint" if self.in_transaction() => Err(RErrorKind::Transaction),
            "checkpoint" | "integrity_check" => Ok(Outcome::Skipped),
            _ => Err(RErrorKind::Unsupported),
        }
    }

    fn statement(&mut self, statement: &Statement) -> Result<Outcome, RErrorKind> {
        match &statement.node {
            StatementKind::Begin => {
                if self.saved.is_some() {
                    return Err(RErrorKind::Transaction);
                }
                self.saved = Some(self.state.clone());
                Ok(Outcome::Affected(0))
            }
            StatementKind::Commit => {
                self.saved.take().ok_or(RErrorKind::Transaction)?;
                Ok(Outcome::Affected(0))
            }
            StatementKind::Rollback => {
                self.state = self.saved.take().ok_or(RErrorKind::Transaction)?;
                Ok(Outcome::Affected(0))
            }
            StatementKind::Select(select) => {
                let planned = select::plan(&self.state, select)?;
                let rows = planned.run()?;
                Ok(Outcome::Rows {
                    columns: planned.columns,
                    rows,
                })
            }
            kind => {
                let mut next = self.state.clone();
                let count = match kind {
                    StatementKind::CreateTable(c) => statements::create_table(&mut next, c),
                    StatementKind::DropTable(d) => statements::drop_table(&mut next, d),
                    StatementKind::CreateIndex(c) => statements::create_index(&mut next, c),
                    StatementKind::Insert(i) => statements::insert(&mut next, i),
                    StatementKind::Update(u) => statements::update(&mut next, u),
                    StatementKind::Delete(d) => statements::delete(&mut next, d),
                    _ => Err(RErrorKind::Unsupported),
                }?;
                self.state = next;
                Ok(Outcome::Affected(count))
            }
        }
    }
}

enum Item<'a> {
    /// A statement and whether EXPLAIN marks it.
    Statement(&'a Statement, bool),
    Pragma(&'a str),
    /// `EXPLAIN PRAGMA ...`, which is never valid.
    ExplainedPragma,
}

/// Statements and pragmas in source order, each EXPLAIN marker attached to
/// the item after it. A marker with nothing after it is a syntax error.
fn items<'a>(
    statements: &'a [Statement],
    directives: &'a prepass::Directives,
) -> Result<Vec<Item<'a>>, RErrorKind> {
    let mut spans: Vec<(Span, Option<&'a Statement>, &'a str)> = statements
        .iter()
        .map(|s| (s.span, Some(s), ""))
        .chain(
            directives
                .pragmas
                .iter()
                .map(|(span, name)| (*span, None, name.as_str())),
        )
        .collect();
    spans.sort_by_key(|(span, _, _)| span.start);
    let mut out = Vec::with_capacity(spans.len());
    let mut previous_end = 0;
    for (span, statement, name) in spans {
        let explained = directives
            .explains
            .iter()
            .any(|m| m.start >= previous_end && m.start < span.start);
        out.push(match statement {
            Some(statement) => Item::Statement(statement, explained),
            None if explained => Item::ExplainedPragma,
            None => Item::Pragma(name),
        });
        previous_end = span.end;
    }
    if directives.explains.iter().any(|m| m.start >= previous_end) {
        return Err(RErrorKind::Syntax);
    }
    Ok(out)
}

/// One result block in the golden-test format. Errors print as
/// `error: <Kind>`, since the reference has no messages or positions.
pub fn render_outcome(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Rows { columns, rows } => {
            let mut lines = vec![
                columns
                    .iter()
                    .map(|c| escape(c))
                    .collect::<Vec<_>>()
                    .join("\t"),
            ];
            for row in rows {
                lines.push(
                    row.iter()
                        .map(|v| match v {
                            RValue::Text(t) => escape(t),
                            other => other.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("\t"),
                );
            }
            lines.join("\n")
        }
        Outcome::Affected(n) => format!("affected {n}"),
        Outcome::Error(kind) => format!("error: {kind:?}"),
        Outcome::Skipped => "skipped".to_string(),
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(engine: &mut Engine, sql: &str) -> Vec<String> {
        engine
            .run_script(sql)
            .expect("parses")
            .iter()
            .map(render_outcome)
            .collect()
    }

    #[test]
    fn a_failed_statement_changes_nothing_and_rollback_restores() {
        let mut db = Engine::new();
        let out = run(
            &mut db,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE);
             INSERT INTO t VALUES (1, 'a'), (2, 'b');
             INSERT INTO t VALUES (3, 'c'), (4, 'a');
             BEGIN; DELETE FROM t; ROLLBACK;
             UPDATE t SET id = id + 1;
             SELECT id, v FROM t",
        );
        assert_eq!(out[2], "error: Constraint");
        assert_eq!(out[4], "affected 2");
        assert_eq!(out[6], "affected 2");
        assert_eq!(out[7], "id\tv\n2\ta\n3\tb");
    }

    #[test]
    fn transaction_misuse_and_directives() {
        let mut db = Engine::new();
        let out = run(
            &mut db,
            "COMMIT; BEGIN; BEGIN; PRAGMA checkpoint; ROLLBACK; PRAGMA checkpoint;
             PRAGMA nope; EXPLAIN SELECT 1; EXPLAIN BEGIN",
        );
        assert_eq!(
            out,
            [
                "error: Transaction",
                "affected 0",
                "error: Transaction",
                "error: Transaction",
                "affected 0",
                "skipped",
                "error: Unsupported",
                "skipped",
                "error: Unsupported",
            ]
        );
        assert_eq!(db.run_script("SELECT 1; EXPLAIN"), Err(RErrorKind::Syntax));
        assert_eq!(db.run_script("SELEC 1"), Err(RErrorKind::Syntax));
    }

    #[test]
    fn grouping_orders_groups_and_sum_uses_the_total() {
        let mut db = Engine::new();
        let out = run(
            &mut db,
            "CREATE TABLE t (g TEXT, v INTEGER);
             INSERT INTO t VALUES ('b', 9223372036854775807), ('a', 1), ('b', 1), ('b', -1), (NULL, 2);
             SELECT g, SUM(v), COUNT(*) FROM t GROUP BY g",
        );
        assert_eq!(
            out[2],
            "g\tsum(v)\tcount(*)\nNULL\t2\t1\na\t1\t1\nb\t9223372036854775807\t3"
        );
    }
}
