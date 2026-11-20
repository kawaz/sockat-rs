//! The conformance CLI that the hub's language-independent conformance test
//! runs (the hub's `docs/design/conformance.md`). Not part of sockat.
//!
//! Reads one JSON object from stdin,
//! `{ "op": "bind" | "connect", "path": "..", "usable_cross_uid": bool, "base": ".." | null }`
//! (`usable_cross_uid` and `base` may be left out: false and none), makes one
//! bind / connect by `sockat::bind_ex` / `sockat::connect_ex` with those
//! `Options`, and writes one JSON object to stdout:
//! `{ "ok", "error", "route", "base", "spelling", "skipped" }`. Neither
//! `remove()` nor `cleanup()` is called (the script looks at what is left). A
//! bound listener is not closed before the process ends; a connected stream
//! sends one byte `x` and is closed.
//!
//! Run it by `just --justfile <this repository>/justfile conformance` from any
//! directory: it builds this (`just conformance-build`), then runs it in the
//! caller's cwd and environment.
//!
//! Exit status: 0 once the result is written (whether the call succeeded or
//! not, and also when the byte could not be sent after a connect: whether it
//! arrived is for the conformance script to see), 2 when stdin is not such an
//! object (nothing is written to stdout).

fn main() -> std::process::ExitCode {
    cli::main()
}

mod cli {
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;

    use serde_json::{Value, json};
    use sockat::{Options, Spelling, Until};

    enum Op {
        Bind,
        Connect,
    }

    pub(crate) fn main() -> ExitCode {
        let (op, path, options) = match read_input() {
            Ok(input) => input,
            Err(message) => {
                eprintln!("conformance: {message}");
                return ExitCode::from(2);
            }
        };
        let path = Path::new(&path);
        let out = match op {
            Op::Bind => match sockat::bind_ex(path, options) {
                Ok(b) => {
                    let out = json!({
                        "ok": true,
                        "error": null,
                        "route": b.route,
                        "base": b.base,
                        "spelling": spelling(b.spelling),
                        "skipped": names(&b.skipped),
                    });
                    // The listener stays open until the process ends (the
                    // hub's conformance.md), and nothing is removed.
                    std::mem::forget(b);
                    out
                }
                Err(e) => failed(e),
            },
            Op::Connect => match sockat::connect_ex(path, options) {
                Ok(mut c) => {
                    if let Err(e) = c.socket.write_all(b"x") {
                        eprintln!("conformance: connected, but sending `x` failed: {e}");
                    }
                    json!({
                        "ok": true,
                        "error": null,
                        "route": c.route,
                        "base": null,
                        "spelling": null,
                        "skipped": names(&c.skipped),
                    })
                }
                Err(e) => failed(e),
            },
        };
        println!("{out}");
        ExitCode::SUCCESS
    }

    fn failed(e: sockat::ExError) -> Value {
        json!({
            "ok": false,
            "error": error_name(&e.error),
            "route": e.route,
            "base": null,
            "spelling": null,
            "skipped": names(&e.skipped),
        })
    }

    fn names(skipped: &[sockat::Skipped]) -> Vec<&'static str> {
        skipped.iter().map(|s| s.name).collect()
    }

    /// The SPEC's `Spelling` as JSON: `kind` and, for `Linked`, its
    /// attributes under their Rust names.
    fn spelling(s: Spelling) -> Value {
        match s {
            Spelling::Original => json!({ "kind": "Original" }),
            Spelling::CwdRelative => json!({ "kind": "CwdRelative" }),
            Spelling::Linked {
                removable,
                until,
                usable_cross_uid,
                usable_with_private_tmp,
            } => json!({
                "kind": "Linked",
                "removable": removable,
                "until": match until {
                    Until::Process => "Process",
                    Until::Cleanup => "Cleanup",
                },
                "usable_cross_uid": usable_cross_uid,
                "usable_with_private_tmp": usable_with_private_tmp,
            }),
            Spelling::Dangling => json!({ "kind": "Dangling" }),
            _ => json!({ "kind": "Unknown" }),
        }
    }

    /// `op`, `path` and the `Options` (`usable_cross_uid`, `base`) of the
    /// JSON object on stdin.
    fn read_input() -> Result<(Op, String, Options), String> {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("reading stdin: {e}"))?;
        let v: Value =
            serde_json::from_str(&text).map_err(|e| format!("stdin is not JSON: {e}"))?;
        let op = match v.get("op").and_then(Value::as_str) {
            Some("bind") => Op::Bind,
            Some("connect") => Op::Connect,
            _ => return Err("`op` must be \"bind\" or \"connect\"".into()),
        };
        let path = v
            .get("path")
            .and_then(Value::as_str)
            .ok_or("`path` must be a string")?;
        let usable_cross_uid = match v.get("usable_cross_uid") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("`usable_cross_uid` must be true or false".into()),
        };
        let base = match v.get("base") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(PathBuf::from(s)),
            Some(_) => return Err("`base` must be a string or null".into()),
        };
        Ok((
            op,
            path.to_owned(),
            Options {
                usable_cross_uid,
                base,
            },
        ))
    }

    /// The errno's name for an OS error, `invalid-argument` for sockat's
    /// refusal of the argument, `general` for anything else.
    fn error_name(e: &io::Error) -> String {
        match e.raw_os_error() {
            Some(n) => ERRNO_NAMES
                .iter()
                .find(|(code, _)| *code == n)
                .map_or_else(|| format!("errno-{n}"), |(_, name)| (*name).to_owned()),
            None if e.kind() == io::ErrorKind::InvalidInput => "invalid-argument".to_owned(),
            None => "general".to_owned(),
        }
    }

    /// The POSIX errnos, the first of two names for one value winning
    /// (`EAGAIN` over `EWOULDBLOCK`).
    const ERRNO_NAMES: &[(i32, &str)] = &[
        (libc::EPERM, "EPERM"),
        (libc::ENOENT, "ENOENT"),
        (libc::ESRCH, "ESRCH"),
        (libc::EINTR, "EINTR"),
        (libc::EIO, "EIO"),
        (libc::ENXIO, "ENXIO"),
        (libc::E2BIG, "E2BIG"),
        (libc::ENOEXEC, "ENOEXEC"),
        (libc::EBADF, "EBADF"),
        (libc::ECHILD, "ECHILD"),
        (libc::EAGAIN, "EAGAIN"),
        (libc::EWOULDBLOCK, "EWOULDBLOCK"),
        (libc::ENOMEM, "ENOMEM"),
        (libc::EACCES, "EACCES"),
        (libc::EFAULT, "EFAULT"),
        (libc::EBUSY, "EBUSY"),
        (libc::EEXIST, "EEXIST"),
        (libc::EXDEV, "EXDEV"),
        (libc::ENODEV, "ENODEV"),
        (libc::ENOTDIR, "ENOTDIR"),
        (libc::EISDIR, "EISDIR"),
        (libc::EINVAL, "EINVAL"),
        (libc::ENFILE, "ENFILE"),
        (libc::EMFILE, "EMFILE"),
        (libc::ENOTTY, "ENOTTY"),
        (libc::ETXTBSY, "ETXTBSY"),
        (libc::EFBIG, "EFBIG"),
        (libc::ENOSPC, "ENOSPC"),
        (libc::ESPIPE, "ESPIPE"),
        (libc::EROFS, "EROFS"),
        (libc::EMLINK, "EMLINK"),
        (libc::EPIPE, "EPIPE"),
        (libc::EDOM, "EDOM"),
        (libc::ERANGE, "ERANGE"),
        (libc::EDEADLK, "EDEADLK"),
        (libc::ENAMETOOLONG, "ENAMETOOLONG"),
        (libc::ENOLCK, "ENOLCK"),
        (libc::ENOSYS, "ENOSYS"),
        (libc::ENOTEMPTY, "ENOTEMPTY"),
        (libc::ELOOP, "ELOOP"),
        (libc::ENOTSOCK, "ENOTSOCK"),
        (libc::EDESTADDRREQ, "EDESTADDRREQ"),
        (libc::EMSGSIZE, "EMSGSIZE"),
        (libc::EPROTOTYPE, "EPROTOTYPE"),
        (libc::ENOPROTOOPT, "ENOPROTOOPT"),
        (libc::EPROTONOSUPPORT, "EPROTONOSUPPORT"),
        (libc::EOPNOTSUPP, "EOPNOTSUPP"),
        (libc::ENOTSUP, "ENOTSUP"),
        (libc::EAFNOSUPPORT, "EAFNOSUPPORT"),
        (libc::EADDRINUSE, "EADDRINUSE"),
        (libc::EADDRNOTAVAIL, "EADDRNOTAVAIL"),
        (libc::ENETDOWN, "ENETDOWN"),
        (libc::ENETUNREACH, "ENETUNREACH"),
        (libc::ECONNABORTED, "ECONNABORTED"),
        (libc::ECONNRESET, "ECONNRESET"),
        (libc::ENOBUFS, "ENOBUFS"),
        (libc::EISCONN, "EISCONN"),
        (libc::ENOTCONN, "ENOTCONN"),
        (libc::ETIMEDOUT, "ETIMEDOUT"),
        (libc::ECONNREFUSED, "ECONNREFUSED"),
        (libc::EHOSTUNREACH, "EHOSTUNREACH"),
        (libc::EALREADY, "EALREADY"),
        (libc::EINPROGRESS, "EINPROGRESS"),
        (libc::EOVERFLOW, "EOVERFLOW"),
        (libc::ECANCELED, "ECANCELED"),
        (libc::EDQUOT, "EDQUOT"),
        (libc::ESTALE, "ESTALE"),
    ];
}
