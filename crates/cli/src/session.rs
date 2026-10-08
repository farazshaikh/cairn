//! Interactive mode: prompts, one statement at a time, meta-commands.

use std::io;

use cairn_exec::Database;
use cairn_exec::render::render_outcome;

use crate::meta::{self, Flow};
use crate::splitter::Splitter;
use crate::{Session, report, warn};

pub(crate) const PROMPT: &str = "cairn> ";
pub(crate) const CONTINUATION: &str = "   ...> ";

/// Why the read-eval-print loop stopped early.
enum Failure {
    Read(io::Error),
    Write(io::Error),
}

/// Runs the session until end of input, `.quit` or `.exit`, then closes the
/// database. Statement errors never end the session and do not change the
/// exit code; a failed read, write or close makes it 1.
pub(crate) fn run(mut db: Database, mut session: Session<'_>) -> u8 {
    let mut splitter = Splitter::new();
    let mut ok = match read_eval(&mut db, &mut splitter, &mut session) {
        Ok(()) => true,
        Err(Failure::Read(e)) => {
            report(session.stderr, &format!("cannot read stdin: {e}"));
            false
        }
        Err(Failure::Write(e)) => {
            report(session.stderr, &format!("cannot write output: {e}"));
            false
        }
    };
    if splitter.take_pending().is_some() {
        warn(session.stderr, "incomplete statement discarded");
    }
    ok &= session.close(db);
    u8::from(!ok)
}

fn read_eval(
    db: &mut Database,
    splitter: &mut Splitter,
    session: &mut Session<'_>,
) -> Result<(), Failure> {
    let mut line = String::new();
    loop {
        let prompt = if splitter.is_pending() {
            CONTINUATION
        } else {
            PROMPT
        };
        session
            .stdout
            .write_all(prompt.as_bytes())
            .and_then(|()| session.stdout.flush())
            .map_err(Failure::Write)?;
        line.clear();
        if session.stdin.read_line(&mut line).map_err(Failure::Read)? == 0 {
            return Ok(());
        }
        if !splitter.is_pending() && line.trim_start().starts_with('.') {
            match meta::run(&line, db, session.stdout).map_err(Failure::Write)? {
                Flow::Continue => continue,
                Flow::Quit => return Ok(()),
            }
        }
        for statement in splitter.push_line(&line) {
            let outcome = db.execute_each(&statement);
            session
                .stdout
                .write_all(render_outcome(&outcome).as_bytes())
                .map_err(Failure::Write)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    use cairn_storage::Options;
    use cairn_storage::fault::{Fault, FaultVfs};

    /// A close that fails cannot be provoked through the binary, so this
    /// drives the session directly with a fault-injecting file layer.
    #[test]
    fn a_failed_close_is_reported_with_exit_code_1() {
        let vfs = FaultVfs::new();
        let path = Path::new("/close.db");
        let mut db =
            Database::create_with(Arc::new(vfs.clone()), path, Options::default()).expect("db");
        db.execute("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1);")
            .expect("setup");
        vfs.arm(Fault::StopAfterSync(vfs.syncs() + 1));
        let mut stdin: &[u8] = b"SELECT a FROM t;\n";
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let session = Session {
            path,
            stdin: &mut stdin,
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        assert_eq!(run(db, session), 1);
        assert_eq!(
            String::from_utf8(stdout).expect("utf-8"),
            "cairn> a\n1\ncairn> "
        );
        let stderr = String::from_utf8(stderr).expect("utf-8");
        assert!(
            stderr.starts_with("error: cannot close /close.db: "),
            "stderr: {stderr}"
        );
    }
}
