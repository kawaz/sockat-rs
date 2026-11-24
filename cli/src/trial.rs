//! `bind` and `connect`: bind or connect by the routes named, and show how
//! the path was reached (docs/design/cli.md "手法を試すサブコマンド",
//! DR-0034). For trying the routes: nothing shown is promised.
//!
//! `bind` binds a stream socket and listens, shows the result, then removes
//! the socket file and the E it made (`remove()`), unless `--keep`. `connect`
//! connects a stream to the socket listening at the path, shows the result,
//! and closes it.

use std::io::{self, Write};
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sockat::__cli as entry;
use sockat::{ExError, Options, Skipped, SocketFile, Spelling, Until};

use crate::json;
use crate::resolve::Failure;

/// What `bind` and `connect` are asked for.
pub(crate) struct Request<'a> {
    pub(crate) path: &'a Path,
    /// `--route`: the names of the routes of the operation, or `default`.
    pub(crate) routes: Option<&'a [&'static str]>,
    pub(crate) base: Option<PathBuf>,
    pub(crate) json: bool,
    /// `--keep` (`bind` only).
    pub(crate) keep: bool,
}

/// What a bind or connect came to, as shown (cli.md "出力").
pub(crate) struct Outcome<'a> {
    /// The error, when it did not get through.
    pub(crate) error: Option<&'a io::Error>,
    /// The route that made the call (or failed for real).
    pub(crate) route: Option<&'static str>,
    /// The B the bind's spelling goes through.
    pub(crate) base: Option<&'static str>,
    /// What the bind's spelling is good for.
    pub(crate) spelling: Option<Spelling>,
    pub(crate) skipped: &'a [Skipped],
    /// What `getsockname` (bind) or `getpeername` (connect) gives.
    pub(crate) kernel_path: Option<PathBuf>,
    /// The E the bind made that is there when shown, and where it points.
    pub(crate) left: Vec<(PathBuf, PathBuf)>,
}

/// Run `bind` or `connect` (`op`) as `req` asks, with B in `places`.
pub(crate) fn run(op: entry::Op, req: &Request<'_>, places: &entry::Places) -> ExitCode {
    let options = Options {
        base: req.base.clone(),
        ..Options::default()
    };
    match op {
        entry::Op::Bind => match entry::bind(places, req.path, options, req.routes) {
            Ok(bound) => {
                let outcome = Outcome {
                    error: None,
                    route: Some(bound.route),
                    base: bound.base,
                    spelling: Some(bound.spelling),
                    skipped: &bound.skipped,
                    kernel_path: kernel_path(entry::local_path(bound.socket.as_fd())),
                    left: left(&bound.file),
                };
                let shown = show(op, &outcome, req.json);
                if req.keep {
                    return shown;
                }
                // Before the listener is closed (hub SPEC "remove").
                if let Err(e) = bound.file.remove() {
                    let _ = writeln!(
                        io::stderr(),
                        "sockat: cannot remove the socket file or the link made for it: {e}\n  remove them by hand: {}",
                        what_to_remove(req.path, &outcome.left)
                    );
                    return ExitCode::FAILURE;
                }
                drop(bound.socket);
                shown
            }
            Err(e) => failed(op, &e, req.json),
        },
        entry::Op::Connect => match entry::connect(places, req.path, options, req.routes) {
            Ok(connected) => {
                let kernel_path = kernel_path(entry::peer_path(connected.socket.as_fd()));
                show(
                    op,
                    &Outcome {
                        error: None,
                        route: Some(connected.route),
                        base: None,
                        spelling: None,
                        skipped: &connected.skipped,
                        kernel_path,
                        left: Vec::new(),
                    },
                    req.json,
                )
            }
            Err(e) => failed(op, &e, req.json),
        },
    }
}

/// The path the kernel gives (read by the library as the kernel wrote it,
/// not through std's `SocketAddr`), `None` when there is none.
fn kernel_path(read: io::Result<PathBuf>) -> Option<PathBuf> {
    read.ok().filter(|p| !p.as_os_str().is_empty())
}

/// The paths to remove by hand when `remove()` failed.
fn what_to_remove(path: &Path, left: &[(PathBuf, PathBuf)]) -> String {
    let mut paths = vec![path.display().to_string()];
    paths.extend(left.iter().map(|(e, _)| e.display().to_string()));
    paths.join(", ")
}

/// The E the bind's spelling goes through (link-dir, link-file), by its real
/// path (B's spelling may go through `/proc`, which names it only while this
/// process lives), with where it points, when it is there. None for the
/// other routes, whose spelling goes through no E kept.
fn left(file: &SocketFile) -> Vec<(PathBuf, PathBuf)> {
    let found =
        entry::link_path(file).and_then(|e| std::fs::read_link(&e).ok().map(|target| (e, target)));
    found.into_iter().collect()
}

/// A call that did not get through: an error in the arguments (exit code 2,
/// on the standard error), else the result shown (exit code 1).
fn failed(op: entry::Op, e: &ExError, json: bool) -> ExitCode {
    if e.error.kind() == io::ErrorKind::InvalidInput && e.error.raw_os_error().is_none() {
        let _ = writeln!(
            io::stderr(),
            "sockat: {}\n  give a path ending with the socket's file name, and a --base that is absolute and ends with a name",
            e.error
        );
        return ExitCode::from(2);
    }
    let shown = show(
        op,
        &Outcome {
            error: Some(&e.error),
            route: e.route,
            base: None,
            spelling: None,
            skipped: &e.skipped,
            kernel_path: None,
            left: Vec::new(),
        },
        json,
    );
    match shown == ExitCode::SUCCESS {
        true => ExitCode::FAILURE,
        false => shown,
    }
}

/// Show `outcome` on the standard output, one `name: value` a line, or as
/// JSON.
fn show(op: entry::Op, outcome: &Outcome<'_>, as_json: bool) -> ExitCode {
    let out = match as_json {
        true => match json::outcome(outcome) {
            Ok(text) => text.into_bytes(),
            Err(f) => return fail(&f),
        },
        false => lines(op, outcome),
    };
    let mut stdout = io::stdout().lock();
    match stdout.write_all(&out).and_then(|()| stdout.flush()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(io::stderr(), "sockat: cannot write the output: {e}");
            ExitCode::FAILURE
        }
    }
}

fn fail(f: &Failure) -> ExitCode {
    let _ = writeln!(io::stderr(), "sockat: {}", f.message);
    ExitCode::FAILURE
}

/// The lines of `outcome`: `error` (when it did not get through), `route`,
/// then for a bind that got through `base`, `kernel-path`, `spelling`,
/// `skipped` and `left`, for a connect `kernel-path` and `skipped`. `-` for
/// nothing.
fn lines(op: entry::Op, o: &Outcome<'_>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut line = |name: &str, value: &[u8]| {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(match value.is_empty() {
            true => b"-",
            false => value,
        });
        out.push(b'\n');
    };
    if let Some(e) = o.error {
        line("error", format!("{}: {e}", entry::error_name(e)).as_bytes());
    }
    line("route", o.route.unwrap_or_default().as_bytes());
    let ok = o.error.is_none();
    if ok && op == entry::Op::Bind {
        line("base", o.base.unwrap_or_default().as_bytes());
    }
    if ok {
        line(
            "kernel-path",
            o.kernel_path
                .as_deref()
                .map(|p| p.as_os_str().as_bytes())
                .unwrap_or_default(),
        );
    }
    if ok && op == entry::Op::Bind {
        line(
            "spelling",
            o.spelling.map(spelling).unwrap_or_default().as_bytes(),
        );
    }
    let skipped: Vec<String> = o
        .skipped
        .iter()
        .map(|s| format!("{} ({})", s.name, s.reason))
        .collect();
    line("skipped", skipped.join(", ").as_bytes());
    if ok && op == entry::Op::Bind {
        if o.left.is_empty() {
            line("left", b"");
        }
        for (e, target) in &o.left {
            let mut value = e.as_os_str().as_bytes().to_vec();
            value.extend_from_slice(b" -> ");
            value.extend_from_slice(target.as_os_str().as_bytes());
            line("left", &value);
        }
    }
    out
}

/// `s` in words: its kind, and for `Linked` what it is good for.
fn spelling(s: Spelling) -> String {
    match s {
        Spelling::Original => "Original".to_owned(),
        Spelling::CwdRelative => "CwdRelative".to_owned(),
        Spelling::Linked {
            removable,
            until,
            usable_cross_uid,
            usable_with_private_tmp,
            ..
        } => {
            let not = |b: bool| if b { "" } else { "not " };
            let until = match until {
                Until::Process => "until process",
                Until::Cleanup => "until cleanup",
            };
            format!(
                "Linked ({}removable, {until}, {}usable cross uid, {}usable with private tmp)",
                not(removable),
                not(usable_cross_uid),
                not(usable_with_private_tmp)
            )
        }
        Spelling::Dangling => "Dangling".to_owned(),
        _ => "Unknown".to_owned(),
    }
}
