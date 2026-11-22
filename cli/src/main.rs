//! The command `sockat`: print a path for a Unix domain socket that fits the
//! socket address limit (`sun_path`), naming the same place as a path that
//! does not (hub DR-0018, docs/design/cli.md; this repository's DR-0033).

#![forbid(unsafe_code)]

mod json;
mod resolve;

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command, ValueHint, value_parser};
use clap_complete::Shell;
use sockat::__cli as entry;

use resolve::{Env, Failure, Use};

/// The options of `resolve`, which `sockat <PATH>` takes as well.
fn resolve_args() -> [Arg; 4] {
    [
        Arg::new("path")
            .value_name("PATH")
            .value_parser(value_parser!(PathBuf))
            .value_hint(ValueHint::AnyPath)
            .required(true)
            .help("The socket's path (from the current directory when relative)"),
        Arg::new("json")
            .long("json")
            .action(ArgAction::SetTrue)
            .help("Print the path with its length and how it was made, as JSON"),
        Arg::new("for")
            .long("for")
            .value_name("USE")
            .value_parser(["bind", "connect"])
            .help("What the path is for; needed only when the file name is too long for a path that works for both"),
        Arg::new("base")
            .long("base")
            .value_name("DIR")
            .value_parser(value_parser!(PathBuf))
            .value_hint(ValueHint::DirPath)
            .help("The base directory itself (made when missing), instead of /tmp/sockat-<uid> and the like"),
    ]
}

/// `--help` and `--help-all` (long options only). `--help-all` adds the
/// advanced options, hidden from `--help` by `hide_short_help`; there are
/// none yet, so both show the same.
fn help_args() -> [Arg; 2] {
    [
        Arg::new("help")
            .long("help")
            .action(ArgAction::HelpShort)
            .help("Print help"),
        Arg::new("help-all")
            .long("help-all")
            .action(ArgAction::HelpLong)
            .help("Print help with the advanced options"),
    ]
}

/// A subcommand taking `args`, then `--help` and `--help-all`, and no short
/// options.
fn subcommand(name: &'static str, args: impl IntoIterator<Item = Arg>) -> Command {
    Command::new(name)
        .disable_help_flag(true)
        .disable_version_flag(true)
        .args(args)
        .args(help_args())
}

fn command() -> Command {
    Command::new("sockat")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Print a path for a Unix domain socket that fits the socket address limit (sun_path), for a path that does not")
        .override_usage("sockat [OPTIONS] <PATH>    (= sockat resolve [OPTIONS] <PATH>)\n       sockat <COMMAND>")
        .disable_help_flag(true)
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        // `sockat <PATH>` is `sockat resolve <PATH>`: the first argument
        // names a subcommand when it is one, and is the path otherwise.
        .args_conflicts_with_subcommands(true)
        .subcommand_negates_reqs(true)
        .args(resolve_args())
        .args(help_args())
        .arg(
            Arg::new("version")
                .long("version")
                .action(ArgAction::Version)
                .help("Print the version"),
        )
        .subcommand(
            subcommand("resolve", resolve_args())
                .about("Print a path for the socket that fits the socket address limit"),
        )
        .subcommand(subcommand("sweep", []).about(
            "Remove what ended processes left in the bases in /tmp, $XDG_RUNTIME_DIR and $TMPDIR",
        ))
        .subcommand(
            subcommand(
                "completion",
                [Arg::new("shell")
                    .value_name("SHELL")
                    .value_parser(value_parser!(Shell))
                    .required(true)
                    .help("The shell")],
            )
            .about("Print the completion script for a shell"),
        )
        .after_help("Environment: TMPDIR and XDG_RUNTIME_DIR name the places for the base after /tmp.")
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    // With no arguments, the command and each subcommand that takes any
    // print their help (exit code 0), as `--help` does.
    match args.get(1..).unwrap_or_default() {
        [] => return print_help(command()),
        [sub] if sub == "resolve" || sub == "completion" => {
            let mut cmd = command();
            cmd.build();
            if let Some(sub) = cmd.find_subcommand(sub).cloned() {
                return print_help(sub);
            }
        }
        _ => {}
    }
    let matches = match command().try_get_matches_from(&args) {
        Ok(m) => m,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    match matches.subcommand() {
        Some(("resolve", m)) => run_resolve(m),
        Some(("sweep", _)) => run_sweep(),
        Some(("completion", m)) => run_completion(m),
        _ => run_resolve(&matches),
    }
}

fn print_help(mut cmd: Command) -> ExitCode {
    match cmd.print_help() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// The places for B: `/tmp`, `$XDG_RUNTIME_DIR`, `$TMPDIR`.
// Design rationale: a debug build (the tests' build) takes
// `SOCKAT_CLI_TEST_TMP` in place of `/tmp`, so the tests make B in a
// directory of their own and never touch the `/tmp/sockat-<uid>` the user's
// processes share. A release build (`cargo install`) has no such knob.
fn places() -> entry::Places {
    #[cfg(debug_assertions)]
    if let Some(tmp) = std::env::var_os("SOCKAT_CLI_TEST_TMP") {
        return entry::Places::with_tmp(PathBuf::from(tmp));
    }
    entry::Places::from_env()
}

fn run_resolve(m: &ArgMatches) -> ExitCode {
    let Some(path) = m.get_one::<PathBuf>("path") else {
        return print_help(command());
    };
    let for_ = match m.get_one::<String>("for").map(String::as_str) {
        Some("bind") => Some(Use::Bind),
        Some("connect") => Some(Use::Connect),
        _ => None,
    };
    let base = m.get_one::<PathBuf>("base");
    let env = Env { places: places() };
    let resolved = match resolve::resolve(path, for_, base.map(PathBuf::as_path), &env) {
        Ok(r) => r,
        Err(f) => return fail(&f),
    };
    let out = match m.get_flag("json") {
        true => match json::resolved(path, &resolved) {
            Ok(text) => text.into_bytes(),
            Err(f) => return fail(&f),
        },
        false => {
            let mut line = resolved.path.as_os_str().as_bytes().to_vec();
            line.push(b'\n');
            line
        }
    };
    write_out(&out)
}

fn run_sweep() -> ExitCode {
    match entry::sweep(&places()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&Failure {
            message: format!(
                "cannot sweep the bases: {e}\n  look at the bases in /tmp, $XDG_RUNTIME_DIR and $TMPDIR"
            ),
            usage: false,
        }),
    }
}

fn run_completion(m: &ArgMatches) -> ExitCode {
    let Some(shell) = m.get_one::<Shell>("shell").copied() else {
        return print_help(command());
    };
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut command(), "sockat", &mut out);
    write_out(&out)
}

/// Print the cause and what to do on the standard error, nothing on the
/// standard output; exit code 2 for an error in the arguments, else 1.
fn fail(f: &Failure) -> ExitCode {
    let _ = writeln!(io::stderr(), "sockat: {}", f.message);
    ExitCode::from(if f.usage { 2 } else { 1 })
}

fn write_out(out: &[u8]) -> ExitCode {
    let mut stdout = io::stdout().lock();
    match stdout.write_all(out).and_then(|()| stdout.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "sockat: cannot write the output: {e}");
            ExitCode::FAILURE
        }
    }
}
