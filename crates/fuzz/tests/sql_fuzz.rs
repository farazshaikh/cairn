//! SQL front-end robustness (milestone M6 AC2).
//!
//! For random bytes, random token streams, generated statements and
//! expressions at the nesting limit:
//!
//! - `tokenize`, `parse` and `Database::execute_each` never panic (the
//!   runner reports a panic with its seed);
//! - every error position lies inside the input, on character boundaries,
//!   and its line and column match a recount of the text before it
//!   (columns count characters, only `\n` starts a line);
//! - generated statements parse, print canonically, parse back to an equal
//!   tree, and print the same text again.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cairn_exec::Database;
use cairn_fuzz::generate::case;
use cairn_fuzz::generate::sql_text::{bytes, chain, nested, tokens};
use cairn_fuzz::minimize::Step;
use cairn_fuzz::rng::Rng;
use cairn_fuzz::runner::{Budget, CaseResult, run};
use cairn_sql::{MAX_DEPTH, Span, Statement, StatementKind, parse, tokenize};
use cairn_storage::Options;
use cairn_storage::fault::FaultVfs;

const BUDGET: Budget = Budget {
    seed: 0x5A1F_0E2E_0000_0002,
    cases: 1500,
    timeout: Duration::from_secs(30),
};

/// Checks a span against the text it points into.
fn check_span(sql: &str, span: Span, what: &str) -> Result<(), String> {
    let bad = |problem: &str| Err(format!("{what}: {problem} {span:?} in {sql:?}"));
    if span.start > span.end || span.end > sql.len() {
        return bad("span outside the input");
    }
    if !sql.is_char_boundary(span.start) || !sql.is_char_boundary(span.end) {
        return bad("span not on a character boundary");
    }
    let before = &sql[..span.start];
    let line = 1 + before.matches('\n').count();
    let column = 1 + before.rsplit('\n').next().map_or(0, |l| l.chars().count());
    if (line, column) != (span.line, span.column) {
        return bad(&format!("line/column should be {line}/{column}"));
    }
    Ok(())
}

fn kind_name(statement: &Statement) -> &'static str {
    match statement.node {
        StatementKind::CreateTable(_) => "CREATE TABLE",
        StatementKind::DropTable(_) => "DROP TABLE",
        StatementKind::CreateIndex(_) => "CREATE INDEX",
        StatementKind::Insert(_) => "INSERT",
        StatementKind::Update(_) => "UPDATE",
        StatementKind::Delete(_) => "DELETE",
        StatementKind::Select(_) => "SELECT",
        StatementKind::Begin => "BEGIN",
        StatementKind::Commit => "COMMIT",
        StatementKind::Rollback => "ROLLBACK",
    }
}

/// Parses, prints, re-parses and re-prints; returns the statement kinds.
fn round_trip(sql: &str) -> Result<Vec<&'static str>, String> {
    let first = parse(sql).map_err(|e| format!("does not parse: {e}"))?;
    let printed: Vec<String> = first.iter().map(ToString::to_string).collect();
    let printed = printed.join("; ");
    let second =
        parse(&printed).map_err(|e| format!("printed text {printed:?} does not parse: {e}"))?;
    if first != second {
        return Err(format!("printed {printed:?} parses to a different tree"));
    }
    let again: Vec<String> = second.iter().map(ToString::to_string).collect();
    if again.join("; ") != printed {
        return Err(format!("printing {printed:?} again changes it"));
    }
    Ok(first.iter().map(kind_name).collect())
}

/// No-panic and span checks for arbitrary text, through the lexer, the
/// parser and the executor.
fn robust(sql: &str) -> Result<(), String> {
    if let Err(e) = tokenize(sql) {
        check_span(sql, e.span, "tokenize")?;
    }
    if let Err(e) = parse(sql) {
        check_span(sql, e.span, "parse")?;
    }
    let vfs = Arc::new(FaultVfs::new());
    let mut db = Database::create_with(vfs, Path::new("/robust.db"), Options::default())
        .map_err(|e| format!("create: {e}"))?;
    let _ =
        db.execute("CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT); INSERT INTO t VALUES (1, 'x')");
    let errors: Vec<_> = match db.execute_each(sql) {
        Err(e) => vec![e],
        Ok(results) => results.into_iter().filter_map(Result::err).collect(),
    };
    for error in errors {
        if let Some(span) = error.span() {
            check_span(sql, span, "execute_each")?;
        }
    }
    Ok(())
}

fn check(
    index: u32,
    rng: &mut Rng,
    kinds: &Mutex<BTreeSet<&'static str>>,
) -> Result<(), (String, String)> {
    let fail = |sql: &str, detail: String| (sql.to_string(), detail);
    match index % 4 {
        0 => {
            let sql = bytes(rng);
            robust(&sql).map_err(|d| fail(&sql, d))
        }
        1 => {
            let sql = tokens(rng);
            robust(&sql).map_err(|d| fail(&sql, d))
        }
        2 => {
            let (steps, _) = case(rng);
            for step in steps {
                let Step::Sql(sql) = step.step else {
                    continue;
                };
                let found = round_trip(&sql).map_err(|d| fail(&sql, d))?;
                if let Ok(mut k) = kinds.lock() {
                    k.extend(found);
                }
            }
            Ok(())
        }
        _ => {
            let depth = match rng.below(4) {
                0 => MAX_DEPTH - 1,
                1 => MAX_DEPTH,
                2 => MAX_DEPTH + 1,
                _ => rng.index(MAX_DEPTH),
            };
            let sql = if rng.chance(1, 4) {
                chain(depth)
            } else {
                nested(rng, depth)
            };
            robust(&sql).map_err(|d| fail(&sql, d))?;
            if parse(&sql).is_ok() {
                round_trip(&sql).map_err(|d| fail(&sql, d))?;
            }
            Ok(())
        }
    }
}

#[test]
fn the_front_end_never_panics_and_round_trips() {
    let kinds = Arc::new(Mutex::new(BTreeSet::new()));
    let shared = Arc::clone(&kinds);
    let summary = run("sql_fuzz", BUDGET, move |index, mut rng| {
        match check(index, &mut rng, &shared) {
            Ok(()) => CaseResult::Pass,
            Err((reproducer, detail)) => CaseResult::Fail { reproducer, detail },
        }
    });
    if summary.cases < 100 {
        return;
    }
    let kinds = kinds.lock().expect("kinds");
    let all = [
        "CREATE TABLE",
        "DROP TABLE",
        "CREATE INDEX",
        "INSERT",
        "UPDATE",
        "DELETE",
        "SELECT",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
    ];
    for kind in all {
        assert!(
            kinds.contains(kind),
            "no generated {kind} statement round-tripped: {kinds:?}"
        );
    }
}

#[test]
fn span_checks_reject_wrong_positions() {
    let sql = "SELECT\n  é + x";
    let at = |start, line, column| Span {
        start,
        end: start + 1,
        line,
        column,
    };
    assert!(check_span(sql, at(14, 2, 7), "t").is_ok());
    assert!(check_span(sql, at(14, 2, 8), "t").is_err());
    assert!(
        check_span(sql, at(10, 2, 4), "t").is_err() && check_span(sql, at(9, 2, 3), "t").is_err()
    );
    assert!(check_span(sql, at(40, 2, 4), "t").is_err());
}
