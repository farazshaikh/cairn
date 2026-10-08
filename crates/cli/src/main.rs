//! The `cairn` binary: wires the real standard streams into
//! [`cairn_cli::run`].

use std::io::{self, IsTerminal};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let stdin = io::stdin();
    let stdin_is_terminal = stdin.is_terminal();
    let code = cairn_cli::run(
        &args,
        &mut stdin.lock(),
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
        stdin_is_terminal,
    );
    ExitCode::from(code)
}
