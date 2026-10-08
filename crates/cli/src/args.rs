//! Command-line arguments: `cairn [--create] [--interactive] PATH` and
//! `cairn --help`.

use std::ffi::OsString;
use std::path::PathBuf;

/// What the command line asks for.
#[derive(Debug, PartialEq)]
pub(crate) enum Command {
    Help,
    Run(Invocation),
}

/// A database to open and how to run it.
#[derive(Debug, PartialEq)]
pub(crate) struct Invocation {
    pub(crate) path: PathBuf,
    pub(crate) create: bool,
    pub(crate) interactive: bool,
}

/// How input is read: with prompts, or as one script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Interactive,
    Script,
}

/// Interactive when standard input is a terminal or `--interactive` is
/// given.
pub(crate) fn mode(stdin_is_terminal: bool, interactive_flag: bool) -> Mode {
    if stdin_is_terminal || interactive_flag {
        return Mode::Interactive;
    }
    Mode::Script
}

/// Parses the arguments after the program name. `--help` anywhere wins;
/// options may come before or after PATH. The error is the message for a
/// usage error.
pub(crate) fn parse(args: &[OsString]) -> Result<Command, String> {
    if args.iter().any(|arg| arg == "--help") {
        return Ok(Command::Help);
    }
    let mut create = false;
    let mut interactive = false;
    let mut paths = Vec::new();
    for arg in args {
        if arg == "--create" {
            set_once(&mut create, "--create")?;
        } else if arg == "--interactive" {
            set_once(&mut interactive, "--interactive")?;
        } else if arg.as_encoded_bytes().first() == Some(&b'-') {
            return Err(format!("unknown option {}", arg.to_string_lossy()));
        } else {
            paths.push(PathBuf::from(arg));
        }
    }
    let path = match paths.len() {
        0 => return Err("missing database path".to_string()),
        1 => paths.remove(0),
        _ => return Err("more than one database path".to_string()),
    };
    Ok(Command::Run(Invocation {
        path,
        create,
        interactive,
    }))
}

fn set_once(flag: &mut bool, name: &str) -> Result<(), String> {
    if *flag {
        return Err(format!("option {name} given twice"));
    }
    *flag = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Command, String> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        parse(&args)
    }

    fn run(path: &str, create: bool, interactive: bool) -> Result<Command, String> {
        Ok(Command::Run(Invocation {
            path: PathBuf::from(path),
            create,
            interactive,
        }))
    }

    #[test]
    fn one_path_with_optional_flags_in_any_order() {
        assert_eq!(parse_strs(&["db"]), run("db", false, false));
        assert_eq!(parse_strs(&["--create", "db"]), run("db", true, false));
        assert_eq!(parse_strs(&["db", "--interactive"]), run("db", false, true));
        assert_eq!(
            parse_strs(&["--interactive", "db", "--create"]),
            run("db", true, true)
        );
    }

    #[test]
    fn help_wins_over_everything_else() {
        assert_eq!(parse_strs(&["--help"]), Ok(Command::Help));
        assert_eq!(
            parse_strs(&["a", "b", "--bogus", "--help"]),
            Ok(Command::Help)
        );
    }

    #[test]
    fn usage_errors() {
        assert_eq!(parse_strs(&[]), Err("missing database path".to_string()));
        assert_eq!(
            parse_strs(&["a", "b"]),
            Err("more than one database path".to_string())
        );
        assert_eq!(
            parse_strs(&["--flag", "db"]),
            Err("unknown option --flag".to_string())
        );
        assert_eq!(parse_strs(&["-"]), Err("unknown option -".to_string()));
        assert_eq!(
            parse_strs(&["--create", "db", "--create"]),
            Err("option --create given twice".to_string())
        );
    }

    #[test]
    fn mode_is_interactive_for_a_terminal_or_the_flag() {
        assert_eq!(mode(false, false), Mode::Script);
        assert_eq!(mode(true, false), Mode::Interactive);
        assert_eq!(mode(false, true), Mode::Interactive);
        assert_eq!(mode(true, true), Mode::Interactive);
    }
}
