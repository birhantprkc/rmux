#![deny(missing_docs)]
#![forbid(unsafe_code)]

//! Repository maintenance command entry point.

use std::env;
use std::process::ExitCode;

const HELP: &str = "\
RMUX repository tasks

Usage:
    cargo run -p xtask -- --help

Commands:
    --help, -h, help    Print this help text
";

fn main() -> ExitCode {
    match parse_args(env::args().skip(1)) {
        Ok(Command::Help) => {
            print!("{HELP}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            eprintln!();
            eprint!("{HELP}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
}

fn parse_args<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    let Some(command) = args.next() else {
        return Ok(Command::Help);
    };

    if let Some(extra) = args.next() {
        return Err(format!(
            "unexpected xtask argument after {command}: {extra}"
        ));
    }

    match command.as_str() {
        "--help" | "-h" | "help" => Ok(Command::Help),
        _ => Err(format!("unknown xtask command: {command}")),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_args, Command};

    #[test]
    fn no_args_prints_help() {
        assert_eq!(parse_args([] as [&str; 0]), Ok(Command::Help));
    }

    #[test]
    fn help_aliases_print_help() {
        for alias in ["--help", "-h", "help"] {
            assert_eq!(parse_args([alias]), Ok(Command::Help));
        }
    }

    #[test]
    fn unknown_command_is_an_error() {
        assert_eq!(
            parse_args(["build"]).expect_err("unknown command errors"),
            "unknown xtask command: build"
        );
    }

    #[test]
    fn extra_argument_is_an_error() {
        assert_eq!(
            parse_args(["--help", "build"]).expect_err("extra argument errors"),
            "unexpected xtask argument after --help: build"
        );
    }
}
