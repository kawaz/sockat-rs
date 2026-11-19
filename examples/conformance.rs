//! The conformance CLI that the hub's language-independent conformance test
//! runs (the hub's `docs/design/conformance.md`, DR-0030). Not part of sockat.
//!
//! Reads one JSON object from stdin,
//! `{ "op": "bind" | "connect", "path": "..", "last_resort": true | false }`
//! (`last_resort` may be left out, meaning false), makes one bind / connect by
//! `sockat::bind_ex` / `sockat::connect_ex` with `LastResort::Allow` when
//! `last_resort` is true and `LastResort::Avoid` (the route order of
//! `sockat::bind` / `sockat::connect`) otherwise, and writes one JSON object to stdout:
//! `{ "ok", "error", "route", "losses", "skipped" }`. A listener is closed
//! (the socket file stays); a connected stream sends one byte `x` and is
//! closed.
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
    use std::path::Path;
    use std::process::ExitCode;

    use sockat::LastResort;

    /// What one call came to, in the order of the output's fields.
    struct Report {
        ok: bool,
        error: Option<String>,
        route: Option<&'static str>,
        losses: Vec<&'static str>,
        skipped: Vec<&'static str>,
    }

    pub(crate) fn main() -> ExitCode {
        let (op, path, last_resort) = match read_input() {
            Ok(input) => input,
            Err(message) => {
                eprintln!("conformance: {message}");
                return ExitCode::from(2);
            }
        };
        let path = Path::new(&path);
        let report = match op {
            Op::Bind => match sockat::bind_ex(path, last_resort) {
                Ok(l) => {
                    drop(l.listener);
                    Report {
                        ok: true,
                        error: None,
                        route: Some(l.route),
                        losses: l.losses.iter().map(|l| l.as_str()).collect(),
                        skipped: l.skipped.iter().map(|s| s.name).collect(),
                    }
                }
                Err(e) => failed(e),
            },
            Op::Connect => match sockat::connect_ex(path, last_resort) {
                Ok(mut c) => {
                    if let Err(e) = c.stream.write_all(b"x") {
                        eprintln!("conformance: connected, but sending `x` failed: {e}");
                    }
                    drop(c.stream);
                    Report {
                        ok: true,
                        error: None,
                        route: Some(c.route),
                        losses: c.losses.iter().map(|l| l.as_str()).collect(),
                        skipped: c.skipped.iter().map(|s| s.name).collect(),
                    }
                }
                Err(e) => failed(e),
            },
        };
        println!("{}", to_json(&report));
        ExitCode::SUCCESS
    }

    fn failed(e: sockat::ExError) -> Report {
        Report {
            ok: false,
            error: Some(error_name(&e.error)),
            route: e.route,
            losses: Vec::new(),
            skipped: e.skipped.iter().map(|s| s.name).collect(),
        }
    }

    enum Op {
        Bind,
        Connect,
    }

    /// `op`, `path` and `last_resort` (false when left out) of the JSON
    /// object on stdin.
    fn read_input() -> Result<(Op, String, LastResort), String> {
        let mut text = String::new();
        io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| format!("reading stdin: {e}"))?;
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| format!("stdin is not JSON: {e}"))?;
        let op = match v.get("op").and_then(|o| o.as_str()) {
            Some("bind") => Op::Bind,
            Some("connect") => Op::Connect,
            _ => return Err("`op` must be \"bind\" or \"connect\"".into()),
        };
        let path = v
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or("`path` must be a string")?;
        let last_resort = match v.get("last_resort") {
            None => LastResort::Avoid,
            Some(serde_json::Value::Bool(false)) => LastResort::Avoid,
            Some(serde_json::Value::Bool(true)) => LastResort::Allow,
            Some(_) => return Err("`last_resort` must be true or false".into()),
        };
        Ok((op, path.to_owned(), last_resort))
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

    /// The output object, its fields in the order of the hub's
    /// `conformance.md`.
    fn to_json(r: &Report) -> String {
        let s = |v: &str| serde_json::Value::from(v).to_string();
        let opt = |v: Option<&str>| v.map_or_else(|| "null".to_owned(), s);
        let list = |v: &[&str]| {
            format!(
                "[{}]",
                v.iter().map(|x| s(x)).collect::<Vec<_>>().join(", ")
            )
        };
        format!(
            "{{ \"ok\": {}, \"error\": {}, \"route\": {}, \"losses\": {}, \"skipped\": {} }}",
            r.ok,
            opt(r.error.as_deref()),
            opt(r.route),
            list(&r.losses),
            list(&r.skipped),
        )
    }
}
