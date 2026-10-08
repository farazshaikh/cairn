//! The `cairn` shell: opens a database file and runs SQL from standard
//! input, either as one script or interactively with prompts.
//!
//! [`run`] holds the whole program over generic input and output handles so
//! tests can drive it without a terminal; `main.rs` only wires in the real
//! standard streams. Statement results print in the golden-test format of
//! [`cairn_exec::render`] on standard output. Usage errors, open and close
//! failures and warnings go to standard error.
//!
//! Exit codes: 0 success, 1 a failed statement (script mode), open, read,
//! write or close, 2 a usage error.
#![warn(missing_docs)]

mod args;
mod meta;
mod script;
mod session;
mod splitter;

use std::ffi::OsString;
use std::io::{BufRead, Write};

use cairn_exec::Database;

use args::{Command, Invocation, Mode};

/// The usage line printed with every usage error and by `--help`.
pub const USAGE: &str = "usage: cairn [--create] [--interactive] PATH";

const HELP: &str = "
Opens the cairn database at PATH and runs SQL from standard input.
  --create       create PATH if it does not exist
  --interactive  prompt for input even when standard input is not a terminal
Statements end with ';'. In interactive mode, enter .help for commands.
";

/// Runs the shell with `args` (without the program name) and returns the
/// process exit code.
pub fn run(
    args: &[OsString],
    stdin: &mut dyn BufRead,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    stdin_is_terminal: bool,
) -> u8 {
    let invocation = match args::parse(args) {
        Ok(Command::Run(invocation)) => invocation,
        Ok(Command::Help) => return help(stdout),
        Err(message) => {
            report(stderr, &message);
            let _ = writeln!(stderr, "{USAGE}");
            return 2;
        }
    };
    let db = match open(&invocation) {
        Ok(db) => db,
        Err(message) => {
            report(stderr, &message);
            return 1;
        }
    };
    let session = Session {
        path: &invocation.path,
        stdin,
        stdout,
        stderr,
    };
    match args::mode(stdin_is_terminal, invocation.interactive) {
        Mode::Script => script::run(db, session),
        Mode::Interactive => session::run(db, session),
    }
}

fn help(stdout: &mut dyn Write) -> u8 {
    match write!(stdout, "{USAGE}\n{HELP}").and_then(|()| stdout.flush()) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}

/// Opens PATH, or creates it when it is missing and `--create` is given.
/// A missing PATH without `--create` is refused before the file system is
/// touched.
fn open(invocation: &Invocation) -> Result<Database, String> {
    let path = &invocation.path;
    let shown = path.display();
    let exists = path
        .try_exists()
        .map_err(|e| format!("cannot open {shown}: {e}"))?;
    if !exists && !invocation.create {
        return Err(format!(
            "cannot open {shown}: no such file (use --create to create it)"
        ));
    }
    if !exists {
        return Database::create(path).map_err(|e| format!("cannot create {shown}: {e}"));
    }
    Database::open(path).map_err(|e| format!("cannot open {shown}: {e}"))
}

/// The streams and database path one run of the shell works with.
pub(crate) struct Session<'a> {
    pub(crate) path: &'a std::path::Path,
    pub(crate) stdin: &'a mut dyn BufRead,
    pub(crate) stdout: &'a mut dyn Write,
    pub(crate) stderr: &'a mut dyn Write,
}

impl Session<'_> {
    /// Ends the session: warns about an open transaction, then closes the
    /// database, which discards that transaction and checkpoints. Returns
    /// false if the close failed.
    pub(crate) fn close(&mut self, db: Database) -> bool {
        if db.in_transaction() {
            warn(self.stderr, "open transaction rolled back");
        }
        match db.close() {
            Ok(()) => true,
            Err(e) => {
                report(
                    self.stderr,
                    &format!("cannot close {}: {e}", self.path.display()),
                );
                false
            }
        }
    }
}

/// Writes `error: message` to standard error. A failure to write it cannot
/// be reported anywhere, so it is ignored.
pub(crate) fn report(stderr: &mut dyn Write, message: &str) {
    let _ = writeln!(stderr, "error: {message}");
}

/// Writes `warning: message` to standard error.
pub(crate) fn warn(stderr: &mut dyn Write, message: &str) {
    let _ = writeln!(stderr, "warning: {message}");
}
