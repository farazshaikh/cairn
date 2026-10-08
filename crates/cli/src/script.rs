//! Script mode: all of standard input runs as one batch.

use cairn_exec::render::render_outcome;
use cairn_exec::{Database, ExecError, QueryResult};

use crate::{Session, report};

/// Reads all of standard input, runs it with `Database::execute_each` and
/// prints the result blocks exactly as the golden tests expect. A syntax
/// error anywhere runs nothing. Returns 1 if anything failed.
pub(crate) fn run(mut db: Database, mut session: Session<'_>) -> u8 {
    let mut ok = execute(&mut db, &mut session);
    ok &= session.close(db);
    u8::from(!ok)
}

fn execute(db: &mut Database, session: &mut Session<'_>) -> bool {
    let mut text = String::new();
    if let Err(e) = session.stdin.read_to_string(&mut text) {
        report(session.stderr, &format!("cannot read stdin: {e}"));
        return false;
    }
    let outcome = db.execute_each(&text);
    let written = session
        .stdout
        .write_all(render_outcome(&outcome).as_bytes())
        .and_then(|()| session.stdout.flush());
    if let Err(e) = written {
        report(session.stderr, &format!("cannot write output: {e}"));
        return false;
    }
    succeeded(&outcome)
}

fn succeeded(outcome: &Result<Vec<Result<QueryResult, ExecError>>, ExecError>) -> bool {
    match outcome {
        Ok(results) => results.iter().all(Result::is_ok),
        Err(_) => false,
    }
}
