//! The command `sockat`: print a path for a Unix domain socket that fits the
//! socket address limit (`sun_path`), naming the same place as a path that
//! does not (hub DR-0018, docs/design/cli.md; this repository's DR-0033 and
//! DR-0034).

#![forbid(unsafe_code)]

mod json;
mod resolve;
mod trial;

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::builder::PossibleValuesParser;
use clap::{Arg, ArgAction, ArgMatches, Command, ValueHint, value_parser};
use clap_complete::Shell;
use sockat::__cli as entry;

use resolve::{BaseArg, Env, Failure, Route, Use};

/// The names `--route` takes for an operation whose routes are `routes`:
/// them, and `default` for the order of the operation.
fn route_names(routes: impl IntoIterator<Item = &'static str>) -> Vec<&'static str> {
    routes.into_iter().chain([entry::DEFAULT]).collect()
}

/// `--route` for an operation whose routes are `routes` (hub DR-0018 "手法の
/// 名指し", cli.md "`--route` に渡す名前").
fn route_arg(routes: impl IntoIterator<Item = &'static str>) -> Arg {
    Arg::new("route")
        .long("route")
        .value_name("NAME[,NAME...]")
        .value_delimiter(',')
        .action(ArgAction::Append)
        .value_parser(PossibleValuesParser::new(route_names(routes)))
        .help("Try only these routes, in this order (default: the usual order). For trying the routes: the names are not promised")
}

/// `--base`, as the library's `Options.base`.
fn base_arg() -> Arg {
    Arg::new("base")
        .long("base")
        .value_name("DIR")
        .value_parser(value_parser!(PathBuf))
        .value_hint(ValueHint::DirPath)
        .help("The base directory itself (made when missing), instead of /tmp/sockat-<uid> and the like")
}

fn json_arg(help: &'static str) -> Arg {
    Arg::new("json")
        .long("json")
        .action(ArgAction::SetTrue)
        .help(help)
}

fn path_arg() -> Arg {
    Arg::new("path")
        .value_name("PATH")
        .value_parser(value_parser!(PathBuf))
        .value_hint(ValueHint::AnyPath)
        .required(true)
        .help("The socket's path (from the current directory when relative)")
}

/// The options of `resolve`, which `sockat <PATH>` takes as well. The
/// advanced ones are shown only with `all` (cli.md "呼び方").
fn resolve_args(all: bool) -> [Arg; 7] {
    let advanced = |a: Arg| a.hide(!all).help_heading("Advanced options");
    [
        path_arg(),
        json_arg("Print the path with its length and how it was made, as JSON"),
        Arg::new("for")
            .long("for")
            .value_name("USE")
            .value_parser(["bind", "connect"])
            .help("What the path is for; matters only when the file name is too long for a path through the directory, where the path to the file is given for connect unless --for bind"),
        base_arg(),
        advanced(route_arg(Route::ORDER.map(Route::name))),
        advanced(
            Arg::new("base-fd")
                .long("base-fd")
                .value_name("FD")
                .value_parser(value_parser!(u32).range(..=i64::from(i32::MAX)))
                .help("The fd of a directory the process --base-pid holds open, as the base: the path goes through /proc/<PID>/fd/<FD> and can be followed while that process holds the fd (Linux)"),
        ),
        advanced(
            Arg::new("base-pid")
                .long("base-pid")
                .value_name("PID")
                .value_parser(value_parser!(u32).range(1..))
                .help("The process that holds --base-fd open"),
        ),
    ]
}

/// The options of `bind` (`op` `Bind`) and `connect`.
fn trial_args(op: entry::Op) -> Vec<Arg> {
    let mut args = vec![
        path_arg(),
        route_arg(entry::routes(op).iter().copied()),
        base_arg(),
        json_arg("Print the result as JSON"),
    ];
    if op == entry::Op::Bind {
        args.push(
            Arg::new("keep")
                .long("keep")
                .action(ArgAction::SetTrue)
                .help("Leave the socket file and what was made in the base (removed after the result is shown by default)"),
        );
    }
    args
}

/// `--help` and `--help-all` (long options only). Both print the help of
/// the command they are in; `--help-all` is parsed by [`command`] made with
/// the advanced options and commands shown ([`main`]).
fn help_args() -> [Arg; 2] {
    [
        Arg::new("help")
            .long("help")
            .action(ArgAction::HelpShort)
            .help("Print help"),
        Arg::new("help-all")
            .long("help-all")
            .action(ArgAction::HelpShort)
            .help("Print help with the advanced options and commands"),
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

/// The command; with `all`, the advanced options and the commands that try
/// the routes are shown in the help (`--help-all`), else hidden.
// Design rationale: `--help-all` prints the help of the command made with
// them shown, rather than clap's long help (`ArgAction::HelpLong` with
// `hide_short_help`): clap hides a subcommand from both helps alike, and
// lays out the long help otherwise (the help of an option on its own line),
// so `--help-all` would not hold the lines of `--help` as they are.
fn command(all: bool) -> Command {
    Command::new("sockat")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Print a path to the same Unix domain socket that fits the socket address limit (sun_path)")
        .override_usage("sockat [OPTIONS] <PATH>    (= sockat resolve [OPTIONS] <PATH>)\n       sockat <COMMAND>")
        .disable_help_flag(true)
        .disable_version_flag(true)
        .disable_help_subcommand(true)
        // `sockat <PATH>` is `sockat resolve <PATH>`: the first argument
        // names a subcommand when it is one, and is the path otherwise.
        .args_conflicts_with_subcommands(true)
        .subcommand_negates_reqs(true)
        .args(resolve_args(all))
        .args(help_args())
        .arg(
            Arg::new("version")
                .long("version")
                .action(ArgAction::Version)
                .help("Print the version"),
        )
        .subcommand(
            subcommand("resolve", resolve_args(all))
                .about("Print a path to the same Unix domain socket that fits the socket address limit (sun_path)"),
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
        // The commands that try the routes (cli.md "手法を試すサブコマンド").
        .subcommand(
            subcommand("bind", trial_args(entry::Op::Bind))
                .about("Bind a socket at the path and show how it was reached, then remove what was made (for trying the routes)")
                .hide(!all),
        )
        .subcommand(
            subcommand("connect", trial_args(entry::Op::Connect))
                .about("Connect to the socket at the path and show how it was reached (for trying the routes)")
                .hide(!all),
        )
        .after_help("Environment: https://github.com/kawaz/sockat/blob/main/pages/cli.md#environment")
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().collect();
    // `--help-all` anywhere before `--` asks for the help with everything
    // shown, of the command or subcommand it is given to.
    let all = args
        .iter()
        .skip(1)
        .take_while(|a| *a != "--")
        .any(|a| a == "--help-all");
    // With no arguments, the command and each subcommand that takes any
    // print their help (exit code 0), as `--help` does.
    match args.get(1..).unwrap_or_default() {
        [] => return print_help(command(false)),
        [sub]
            if ["resolve", "completion", "bind", "connect"]
                .map(OsString::from)
                .contains(sub) =>
        {
            let mut cmd = command(false);
            cmd.build();
            if let Some(sub) = cmd.find_subcommand(sub).cloned() {
                return print_help(sub);
            }
        }
        _ => {}
    }
    let matches = match command(all).try_get_matches_from(&args) {
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
        Some(("bind", m)) => run_trial(entry::Op::Bind, m),
        Some(("connect", m)) => run_trial(entry::Op::Connect, m),
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

/// The names given to `--route` (checked by the parser against the names of
/// the operation), `None` when none is given.
fn route_values(m: &ArgMatches) -> Option<Vec<&str>> {
    m.get_many::<String>("route")
        .map(|v| v.map(String::as_str).collect())
}

/// Where B is: `--base`, or `--base-fd` with `--base-pid` (a pair: one of
/// them alone is an error in the arguments, and so is either with `--base`),
/// else the places.
fn base_arg_of(m: &ArgMatches) -> Result<BaseArg<'_>, Failure> {
    let base = m.get_one::<PathBuf>("base");
    let fd = m.get_one::<u32>("base-fd").copied();
    let pid = m.get_one::<u32>("base-pid").copied();
    let usage = |message: &str| Err(Failure::usage(message.to_owned()));
    match (base, fd, pid) {
        (_, Some(_), None) => usage(
            "--base-fd needs --base-pid, the process that holds the fd open\n  give both, for example --base-fd 3 --base-pid $$",
        ),
        (_, None, Some(_)) => usage(
            "--base-pid needs --base-fd, the fd of a directory that process holds open\n  give both, for example --base-fd 3 --base-pid $$",
        ),
        (Some(_), Some(_), Some(_)) => {
            usage("--base and --base-fd both give the base\n  give one of them")
        }
        (None, Some(_), Some(_)) if !cfg!(any(target_os = "linux", target_os = "android")) => {
            Err(resolve::held_base_unsupported())
        }
        (None, Some(fd), Some(pid)) => Ok(BaseArg::Held { fd, pid }),
        (Some(b), None, None) => Ok(BaseArg::Dir(b)),
        (None, None, None) => Ok(BaseArg::Places),
    }
}

fn run_resolve(m: &ArgMatches) -> ExitCode {
    let Some(path) = m.get_one::<PathBuf>("path") else {
        return print_help(command(false));
    };
    let for_ = match m.get_one::<String>("for").map(String::as_str) {
        Some("bind") => Some(Use::Bind),
        Some("connect") => Some(Use::Connect),
        _ => None,
    };
    let base = match base_arg_of(m) {
        Ok(b) => b,
        Err(f) => return fail(&f),
    };
    // `default` is the order of resolve at its place; alone, it is no
    // `--route` at all (the same errors).
    let routes: Option<Vec<Route>> = route_values(m)
        .filter(|names| *names != [entry::DEFAULT])
        .map(|names| {
            names
                .iter()
                .flat_map(|n| match Route::named(n) {
                    Some(r) => vec![r],
                    None => Route::ORDER.to_vec(),
                })
                .collect()
        });
    let env = Env {
        places: places(),
        pwd: std::env::var_os("PWD"),
    };
    let resolved = match resolve::resolve(path, for_, base, routes.as_deref(), &env) {
        Ok(r) => r,
        Err(f) => return fail(&f),
    };
    let out = match m.get_flag("json") {
        true => match json::resolved(path, &resolved) {
            Ok(text) => text.into_bytes(),
            Err(f) => return fail(&f),
        },
        false => {
            for w in &resolved.warnings {
                let _ = writeln!(io::stderr(), "sockat: warning: {w}");
            }
            let mut line = resolved.path.as_os_str().as_bytes().to_vec();
            line.push(b'\n');
            line
        }
    };
    write_out(&out)
}

fn run_trial(op: entry::Op, m: &ArgMatches) -> ExitCode {
    let Some(path) = m.get_one::<PathBuf>("path") else {
        return print_help(command(false));
    };
    // The names as the library's (the parser took only these, so each is
    // found).
    let known = route_names(entry::routes(op).iter().copied());
    let routes: Option<Vec<&'static str>> = route_values(m).map(|names| {
        names
            .iter()
            .filter_map(|n| known.iter().copied().find(|r| r == n))
            .collect()
    });
    trial::run(
        op,
        &trial::Request {
            path,
            routes: routes.as_deref(),
            base: m.get_one::<PathBuf>("base").cloned(),
            json: m.get_flag("json"),
            keep: op == entry::Op::Bind && m.get_flag("keep"),
        },
        &places(),
    )
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
        return print_help(command(false));
    };
    let mut out = Vec::new();
    clap_complete::generate(shell, &mut command(true), "sockat", &mut out);
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
