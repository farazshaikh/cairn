//! Meta-commands: lines starting with `.` at the primary prompt in
//! interactive mode. Their output and errors go to standard output, like
//! statement results.

use std::io::{self, Write};

use cairn_exec::render::render_error;
use cairn_exec::{Database, TableSchema};
use cairn_sql::{Name, TokenKind, tokenize};

/// Whether the session goes on after a command.
pub(crate) enum Flow {
    Continue,
    Quit,
}

const HELP: &str = "\
.help             show this list of commands
.quit             close the database and exit
.exit             close the database and exit
.tables           list the tables
.schema [TABLE]   show the CREATE statements for every table, or for TABLE
";

/// Runs one meta-command line. Only a failure to write output is an error;
/// a bad command prints `error: ...` and the session continues.
pub(crate) fn run(line: &str, db: &mut Database, out: &mut dyn Write) -> io::Result<Flow> {
    let line = line.trim();
    let (command, argument) = match line.split_once(char::is_whitespace) {
        Some((command, rest)) => (command, rest.trim()),
        None => (line, ""),
    };
    match (command, argument) {
        (".help", "") => out.write_all(HELP.as_bytes())?,
        (".quit" | ".exit", "") => return Ok(Flow::Quit),
        (".tables", "") => tables(db, out)?,
        (".schema", argument) => schema(db, argument, out)?,
        (".help" | ".quit" | ".exit" | ".tables", _) => {
            writeln!(out, "error: usage: {command}")?;
        }
        _ => writeln!(
            out,
            "error: unknown command {command} (enter .help for a list of commands)"
        )?,
    }
    Ok(Flow::Continue)
}

fn tables(db: &mut Database, out: &mut dyn Write) -> io::Result<()> {
    let tables = match db.schema() {
        Ok(tables) => tables,
        Err(e) => return writeln!(out, "{}", render_error(&e)),
    };
    for table in tables {
        writeln!(out, "{}", Name(table.name))?;
    }
    Ok(())
}

fn schema(db: &mut Database, argument: &str, out: &mut dyn Write) -> io::Result<()> {
    let wanted = match argument {
        "" => None,
        text => match table_name(text) {
            Some(name) => Some(name),
            None => return writeln!(out, "error: usage: .schema [TABLE]"),
        },
    };
    let tables = match db.schema() {
        Ok(tables) => tables,
        Err(e) => return writeln!(out, "{}", render_error(&e)),
    };
    let selected: Vec<&TableSchema> = tables
        .iter()
        .filter(|table| wanted.as_deref().is_none_or(|name| table.name == name))
        .collect();
    if let (Some(name), true) = (&wanted, selected.is_empty()) {
        return writeln!(out, "error: no such table: {}", Name(name.clone()));
    }
    for table in selected {
        writeln!(out, "{};", table.create_table_sql())?;
        for index in table.create_index_sql() {
            writeln!(out, "{index};")?;
        }
    }
    Ok(())
}

/// The normalised table name when `text` is exactly one identifier, so
/// `Users` finds `users` and `"Users"` finds `Users`.
fn table_name(text: &str) -> Option<String> {
    let tokens = tokenize(text).ok()?;
    match tokens.as_slice() {
        [name, end] if end.kind == TokenKind::Eof => match &name.kind {
            TokenKind::Ident(name) => Some(name.clone()),
            _ => None,
        },
        _ => None,
    }
}
