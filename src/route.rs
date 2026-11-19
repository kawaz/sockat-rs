//! The order in which the ways of reaching a long socket path are tried
//! (DR-0027). [`run`] is the only entry point used by the public API; the
//! pieces are `pub(crate)` so the tests can force each one.
//!
//! A path longer than [`DIRECT_MAX`] goes, in this order:
//!
//! 1. **The central route** ([`central`]): a directory of sockat's own, D, is
//!    made in a place ([`Config::own_dir_places`]) and held by an fd, a
//!    symlink R with a random name to the socket's directory is made in it,
//!    and the call is made with `R/<file name>` relative to D's fd, by the
//!    first means that can be used ([`Means`]): a thread whose own working
//!    directory is D (Linux, Apple), `/proc/self/fd/<D's fd>/R/<file name>`
//!    (Linux), `bindat` / `connectat` (FreeBSD), and for a bind a forked
//!    child in D. D and R are removed when the call returns, so the spelling
//!    the kernel keeps for a bound socket names nothing afterwards. A bind
//!    for which D can be made nowhere makes R beside the socket instead and
//!    tries the same means with `.sockat.XXXXXXXXXX/<file name>` relative to
//!    the socket's directory's fd.
//! 2. **The fallback routes** for a connect ([`fallback`]), which leave no
//!    spelling behind (a connected socket is not bound): per target, the ones
//!    of the table in [`fallback`].
//! 3. **A forked child** in the socket's directory, for the connects that may
//!    fork ([`fork_beside`]); `ENAMETOOLONG` for the others and for a bind.

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::path;
use crate::sys::{self, Op, SockAddr};

/// How a connect is made. The four public connects are its four values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConnectOptions {
    /// Make the socket `O_NONBLOCK` before connecting, on every route, and
    /// leave it so (`connect_nonblocking`, `connect_any_nonblocking`).
    ///
    /// A full backlog then fails the connect at once instead of waiting for
    /// room: Linux answers `EAGAIN` (where a blocking connect waits), macOS
    /// `ECONNREFUSED` (as a blocking connect does).
    ///
    /// A Unix-domain connect completes or fails within the call even on a
    /// nonblocking socket: neither Linux nor XNU ever answers `EINPROGRESS`
    /// for it (checked on Linux 7.2 and macOS 26.5 with 200000 nonblocking
    /// connects against a listener with a small backlog: only success and
    /// `EAGAIN` / `ECONNREFUSED`; neither `net/unix/af_unix.c` nor XNU's
    /// `uipc_usrreq.c` produces it, the latter marking the socket connected
    /// before `connect(2)` looks for a connect still in progress). Should a
    /// kernel ever answer `EINPROGRESS`, it is returned as the error it is and
    /// the socket is closed, rather than handing back a stream that is not
    /// connected yet.
    pub(crate) nonblocking: bool,
    /// Whether the fork route may be taken (`connect_any`,
    /// `connect_any_nonblocking`). Without it, a path that only the fork route
    /// can reach is an error ([`fork_ruled_out`]) and nothing is created or
    /// connected; every other route, and its real failures, stay as they are.
    pub(crate) fork: bool,
}

/// How a bind is made: `sockat::bind` / `bind_ex`, or `bind_any`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BindOptions {
    /// Whether the symlink route may be taken once every other bind route is
    /// unusable (`bind_any`, [`links::bind_through`]). Without it, that is
    /// `ENAMETOOLONG`.
    pub(crate) symlink: bool,
}

/// What to do at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    /// Bind and listen, as the options say (forking when nothing else in D
    /// or beside the socket reaches the path).
    Bind(BindOptions),
    /// Connect, as the options say.
    Connect(ConnectOptions),
}

impl Request {
    /// The system call this comes down to.
    fn op(self) -> Op {
        match self {
            Request::Bind(_) => Op::Bind,
            Request::Connect(_) => Op::Connect,
        }
    }

    /// Whether this is a bind.
    pub(crate) fn is_bind(self) -> bool {
        matches!(self, Request::Bind(_))
    }

    /// Whether the socket is made `O_NONBLOCK` before the call.
    fn nonblocking(self) -> bool {
        matches!(self, Request::Connect(o) if o.nonblocking)
    }

    /// Whether the fork route may be taken.
    fn may_fork(self) -> bool {
        match self {
            Request::Bind(_) => true,
            Request::Connect(o) => o.fork,
        }
    }
}

/// The error for a path that only the fork route reaches, when the options
/// rule the fork route out: `ENAMETOOLONG`, the error of a plain connect to a
/// path longer than `sun_path`. That is what the path is once every other way
/// around the limit is too long or unavailable.
fn fork_ruled_out() -> io::Error {
    too_long()
}

/// `ENAMETOOLONG`: no route reaches the path.
fn too_long() -> io::Error {
    io::Error::from_raw_os_error(libc::ENAMETOOLONG)
}

/// The longest path std accepts, `sun_path` less the NUL: 107 on Linux,
/// Android and illumos, 103 on Apple and the BSDs. A path of at most this many
/// bytes is handed to std as is, so std alone decides what happens to every
/// path it can take (DR-0002). It is also the limit of every other
/// spelling that goes in the plain sockaddr.
pub(crate) const DIRECT_MAX: usize = sys::SUN_PATH_CAP - 1;

/// Which way a bind / connect went, one variant per route of the SPEC
/// ("経路の名前"; its `Display` is the name). The public API does not
/// show it; the `any` functions show what it cost (`sockat::Loss`).
///
/// "In the plain sockaddr" means through std, except for a nonblocking
/// connect, which std cannot make: that one is sockat's own socket, set up as
/// std's, with the same address bytes ([`plain_op`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// standard: the path itself, in the plain sockaddr.
    Standard,
    /// thread-cwd: `R/<name>` on a thread whose own working directory is D
    /// (Linux, Apple; [`sys::op_in_dir_on_thread`]).
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    ThreadCwd,
    /// at: `bindat(2)` / `connectat(2)` with `R/<name>` on D's fd (FreeBSD).
    #[cfg(target_os = "freebsd")]
    At,
    /// proc: `/proc/self/fd/<D's fd>/R/<name>`, in the plain sockaddr
    /// (Linux).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Proc,
    /// fork: a forked child that `fchdir`s into D and binds `R/<name>`
    /// (not FreeBSD, where bindat cannot turn out unusable).
    #[cfg(not(target_os = "freebsd"))]
    Fork {
        /// As [`Route::ForkDirect`]'s.
        child: libc::pid_t,
    },
    /// thread-cwd-sockdir: thread-cwd without D, `.sockat.XXXXXXXXXX/<name>`
    /// on a thread whose own working directory is the socket's directory
    /// (a bind only).
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    ThreadCwdSockdir,
    /// at-sockdir: at without D (FreeBSD, a bind only).
    #[cfg(target_os = "freebsd")]
    AtSockdir,
    /// proc-sockdir: `/proc/self/fd/<the socket's directory's fd>/
    /// .sockat.XXXXXXXXXX/<name>` (Linux, a bind only).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    ProcSockdir,
    /// fork-sockdir: fork without D (a bind only; not FreeBSD).
    #[cfg(not(target_os = "freebsd"))]
    ForkSockdir {
        /// As [`Route::ForkDirect`]'s.
        child: libc::pid_t,
    },
    /// proc-direct (connect fallback): `/proc/self/fd/<the socket's
    /// directory's fd>/<name>`, in the plain sockaddr (Linux).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    ProcDirect,
    /// proc-held (connect fallback): `/proc/self/fd/<D's fd>/s`, `s` a
    /// symlink in D to the socket file, in the plain sockaddr (Linux).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    ProcHeld,
    /// relative (connect fallback): the path spelled relative to the cwd, in
    /// the plain sockaddr.
    #[cfg(not(target_os = "freebsd"))]
    Relative,
    /// widened (connect fallback): the path itself in a widened
    /// `sockaddr_un` (Apple).
    #[cfg(target_vendor = "apple")]
    Widened,
    /// widened (connect fallback), with the relative spelling (Apple).
    #[cfg(target_vendor = "apple")]
    WidenedRelative,
    /// symlink (connect fallback): a short temporary symlink to the socket
    /// file in a safe place, in the plain sockaddr.
    #[cfg(not(target_os = "freebsd"))]
    Symlink,
    /// symlink (`bind_any` only): a temporary symlink in a place to the
    /// socket's directory, and the bind by `<link>/<name>`
    /// ([`links::bind_through`]).
    SymlinkBind {
        /// Whether the place is a shared one (sticky, as `/tmp` is) rather
        /// than one only this user can write to: others can then make the
        /// spelling again ([`links::PlaceKind`]).
        shared: bool,
    },
    /// at-direct (connect fallback): `connectat(2)` with the file name on
    /// the socket's directory fd (FreeBSD).
    #[cfg(target_os = "freebsd")]
    AtDirect,
    /// fork-direct (`connect_any` only): a forked child that `fchdir`s into
    /// the socket's directory and connects by the file name.
    ForkDirect {
        /// The pid of the child that made the call ([`sys::op_in_dir_forked`]),
        /// as in every fork route. The public API does not return it: it is
        /// the pid of an exited process, of no use to the caller or the server
        /// (DR-0010). The tests read it to check that the peer the server sees
        /// is this child.
        child: libc::pid_t,
    },
}

impl Route {
    /// Whether the route forked: the call was made by a forked child.
    ///
    /// For a connect, the peer pid the server sees is then that of the child,
    /// a process that has already exited (DR-0007).
    #[cfg(any(test, feature = "any"))]
    pub(crate) fn forked(self) -> bool {
        #[cfg(not(target_os = "freebsd"))]
        if matches!(self, Route::Fork { .. } | Route::ForkSockdir { .. }) {
            return true;
        }
        matches!(self, Route::ForkDirect { .. })
    }
}

impl Route {
    /// The route's name in the SPEC ("経路の名前", [`names`]), for
    /// diagnostics: it promises nothing about what the route does.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Route::Standard => names::STANDARD,
            #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
            Route::ThreadCwd => names::THREAD_CWD,
            #[cfg(target_os = "freebsd")]
            Route::At => names::AT,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Route::Proc => names::PROC,
            #[cfg(not(target_os = "freebsd"))]
            Route::Fork { .. } => names::FORK,
            #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
            Route::ThreadCwdSockdir => names::THREAD_CWD_SOCKDIR,
            #[cfg(target_os = "freebsd")]
            Route::AtSockdir => names::AT_SOCKDIR,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Route::ProcSockdir => names::PROC_SOCKDIR,
            #[cfg(not(target_os = "freebsd"))]
            Route::ForkSockdir { .. } => names::FORK_SOCKDIR,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Route::ProcDirect => names::PROC_DIRECT,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Route::ProcHeld => names::PROC_HELD,
            #[cfg(not(target_os = "freebsd"))]
            Route::Relative => names::RELATIVE,
            #[cfg(target_vendor = "apple")]
            Route::Widened | Route::WidenedRelative => names::WIDENED,
            #[cfg(not(target_os = "freebsd"))]
            Route::Symlink => names::SYMLINK,
            Route::SymlinkBind { .. } => names::SYMLINK,
            #[cfg(target_os = "freebsd")]
            Route::AtDirect => names::AT_DIRECT,
            Route::ForkDirect { .. } => names::FORK_DIRECT,
        }
    }
}

/// The names of the routes in the SPEC ("経路の名前"), and the reasons a
/// route is passed over ([`Skipped`]). Diagnostics: they promise nothing.
// Design rationale: the whole table of the SPEC, every target's names and
// reasons in one place, rather than each behind the cfg of the routes that
// use it; a target uses only some of them.
#[allow(dead_code)]
pub(crate) mod names {
    pub(crate) const STANDARD: &str = "standard";
    pub(crate) const THREAD_CWD: &str = "thread-cwd";
    pub(crate) const AT: &str = "at";
    pub(crate) const PROC: &str = "proc";
    pub(crate) const FORK: &str = "fork";
    pub(crate) const THREAD_CWD_SOCKDIR: &str = "thread-cwd-sockdir";
    pub(crate) const AT_SOCKDIR: &str = "at-sockdir";
    pub(crate) const PROC_SOCKDIR: &str = "proc-sockdir";
    pub(crate) const FORK_SOCKDIR: &str = "fork-sockdir";
    pub(crate) const PROC_DIRECT: &str = "proc-direct";
    pub(crate) const PROC_HELD: &str = "proc-held";
    pub(crate) const RELATIVE: &str = "relative";
    pub(crate) const WIDENED: &str = "widened";
    pub(crate) const SYMLINK: &str = "symlink";
    pub(crate) const AT_DIRECT: &str = "at-direct";
    pub(crate) const FORK_DIRECT: &str = "fork-direct";

    /// The spelling would not fit the sockaddr.
    pub(crate) const TOO_LONG: &str = "spelling too long";
    /// The thread-cwd means is turned off (the tests' stand-in).
    pub(crate) const THREAD_CWD_OFF: &str = "thread cwd off";
    /// Linux: a seccomp filter is in effect (or its mode cannot be read), so
    /// `unshare` is not called.
    pub(crate) const SECCOMP: &str = "seccomp filter";
    /// Apple: `pthread_fchdir_np` is missing.
    pub(crate) const NO_PTHREAD_FCHDIR: &str = "no pthread_fchdir_np";
    /// The thread could not move its working directory alone (`unshare`
    /// refused, `pthread_fchdir_np` failed, no thread).
    pub(crate) const THREAD_CWD_REFUSED: &str = "thread cwd refused";
    /// `/proc/self/fd/<fd>` cannot be looked up.
    pub(crate) const PROC_UNUSABLE: &str = "/proc unusable";
    /// D could be made in none of the places.
    pub(crate) const NO_PLACE: &str = "no place for D";
    /// The socket's directory cannot be opened.
    pub(crate) const SOCKDIR_UNOPENABLE: &str = "socket directory unopenable";
    /// R could not be made in the socket's directory.
    pub(crate) const NO_R: &str = "R not made";
    /// The cwd cannot be read (the path is relative).
    pub(crate) const CWD_UNREADABLE: &str = "cwd unreadable";
    /// No relative spelling fits, or the cwd turned out to move.
    pub(crate) const NO_RELATIVE: &str = "no relative spelling";
    /// The kernel does not take the widened sockaddr.
    pub(crate) const WIDENED_REFUSED: &str = "kernel refused widened";
    /// No place for the symlink is one where only this user (or root) can
    /// replace it.
    pub(crate) const NO_SAFE_BASE: &str = "no safe base";
    /// The symlink could not be made in any usable place.
    pub(crate) const NO_SYMLINK: &str = "symlink not made";
}

/// Available on crate feature `any` only.
///
/// A route that was tried and could not be used, with why: one of the routes
/// passed over before the one that reached the path (`skipped` of
/// `AnyListener` and `AnyConnection`), in the order tried. Only for
/// diagnostics: routes may be added, renamed or changed, and so may the
/// reasons, so do not branch on them.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Skipped {
    /// The route's name in the SPEC (for example `"thread-cwd"`).
    pub name: &'static str,
    /// Why it could not be used (for example `"seccomp filter"`).
    pub reason: &'static str,
}

/// What a call reached the path by: the socket, the route, the
/// [`Bound`] of a bind when asked for, and the routes passed over before it,
/// in the order tried.
pub(crate) struct Reached {
    pub(crate) fd: OwnedFd,
    pub(crate) route: Route,
    pub(crate) bound: Option<Bound>,
    // Design rationale: collected on every long path (a few entries, no
    // allocation for a short one) rather than only when asked for, so every
    // route records the same way; only the `any` functions and the tests
    // read it.
    #[cfg_attr(not(any(test, feature = "any")), allow(dead_code))]
    pub(crate) skipped: Vec<Skipped>,
}

/// Why a call did not reach the path: the error, the route that made the
/// call and failed for real (`None` when no route could be used, or the path
/// was refused before any), and the routes passed over before it.
#[derive(Debug)]
pub(crate) struct Failed {
    pub(crate) error: io::Error,
    #[cfg_attr(not(any(test, feature = "any")), allow(dead_code))]
    pub(crate) route: Option<&'static str>,
    #[cfg_attr(not(any(test, feature = "any")), allow(dead_code))]
    pub(crate) skipped: Vec<Skipped>,
}

impl Failed {
    /// `error` before any route was tried: no route, nothing passed over.
    fn before_routes(error: io::Error) -> Self {
        Self {
            error,
            route: None,
            skipped: Vec::new(),
        }
    }
}

/// A call made by a route: the route's name, and the socket with the route
/// (which for a fork names the child) or the error.
pub(crate) struct Call {
    pub(crate) name: &'static str,
    pub(crate) result: io::Result<(OwnedFd, Route)>,
}

impl Call {
    /// The call `route` made, with `result`.
    fn made(route: Route, result: io::Result<OwnedFd>) -> Self {
        Self {
            name: route.name(),
            result: result.map(|fd| (fd, route)),
        }
    }
}

/// Bind or connect a stream socket to `path`, trying the routes in order.
///
/// A path of at most [`DIRECT_MAX`] bytes goes as is, before anything else is
/// looked at (the environment included).
///
/// A NUL byte is looked at before the length: a path with one is an
/// `InvalidInput` error at any length, and nothing is created
/// ([`path::refuse_nul`]). The exception is a path starting with a NUL on Linux
/// and Android, a name in the abstract namespace rather than a file, which is
/// not sockat's business at any length: it goes as is ([`path::is_abstract`]).
pub(crate) fn run(req: Request, path: &Path) -> io::Result<(OwnedFd, Route)> {
    run_reporting(req, path)
        .map(|r| (r.fd, r.route))
        .map_err(|f| f.error)
}

/// [`run`], with the route and the routes passed over, on success
/// ([`Reached`]) and on failure ([`Failed`]).
pub(crate) fn run_reporting(req: Request, path: &Path) -> Result<Reached, Failed> {
    run_with(req, path, false)
}

/// The socket file a bind made: its real path and `(st_dev, st_ino)`, taken
/// right after the bind, inside the route (what `sockat::bind_ex` keeps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bound {
    pub(crate) path: PathBuf,
    pub(crate) id: (u64, u64),
}

/// A bind as [`run`] makes it, also taking the [`Bound`] of the socket file
/// right after the bind: for a path handed to std, as soon as std returns; on
/// the central route, while D and R are still there, by the same spelling the
/// bind took (`R/<name>` relative to D's fd, the symlink R followed, the
/// socket file not) and the real path R points to. When that cannot be taken,
/// the listener is closed and the error returned; the socket file stays.
pub(crate) fn bind_remembering(path: &Path) -> io::Result<(OwnedFd, Route, Bound)> {
    let r =
        run_with(Request::Bind(BindOptions { symlink: false }), path, true).map_err(|f| f.error)?;
    let bound = r
        .bound
        .ok_or_else(|| io::Error::other("the socket file was not looked up"))?;
    Ok((r.fd, r.route, bound))
}

/// [`run`], and with `remember` the [`Bound`] of a bind.
fn run_with(req: Request, path: &Path, remember: bool) -> Result<Reached, Failed> {
    let direct = |req, path: &Path| {
        let failed = |error| Failed {
            error,
            route: Some(names::STANDARD),
            skipped: Vec::new(),
        };
        let fd = plain_op(req, path).map_err(failed)?;
        let bound = match remember {
            true => Some(bound_at(path).map_err(failed)?),
            false => None,
        };
        Ok(Reached {
            fd,
            route: Route::Standard,
            bound,
            skipped: Vec::new(),
        })
    };
    if path::is_abstract(path) {
        return direct(req, path);
    }
    path::refuse_nul(path).map_err(Failed::before_routes)?;
    if path::len(path) <= DIRECT_MAX {
        return direct(req, path);
    }
    run_long_with(req, path, &Config::from_env(), remember)
}

/// The [`Bound`] of the socket file at `path`, bound just now by std:
/// `realpath(3)` of `path`, and `lstat(2)` of that.
fn bound_at(path: &Path) -> io::Result<Bound> {
    use std::os::unix::fs::MetadataExt;
    let path = std::fs::canonicalize(path)?;
    let m = std::fs::symlink_metadata(&path)?;
    Ok(Bound {
        path,
        id: (m.dev(), m.ino()),
    })
}

/// What the route order reads from the environment, made explicit so the
/// tests can vary it without touching the process environment.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// Where D may be made, in the order tried ([`Config::from_vars`]).
    pub(crate) own_dir_places: Vec<Place>,
    /// Where the temporary symlink of the fallback route may go, in the
    /// order tried: `TMPDIR` when it is set to an absolute path, then `/tmp`.
    #[cfg(not(target_os = "freebsd"))]
    pub(crate) link_places: Vec<PathBuf>,
    /// Where the temporary symlink of `bind_any`'s symlink route may go, the
    /// candidates in order: `XDG_RUNTIME_DIR`, then `TMPDIR`, each when set to
    /// an absolute path, then `/tmp`, a spelling once ([`links::bind_through`]
    /// tries the places only this user can write to among them first).
    pub(crate) bind_link_places: Vec<PathBuf>,
    /// Whether to try the widened `sockaddr_un` at all for a connect
    /// (Apple; a bind never does). Always `true` outside the tests, which
    /// turn it off to stand in for a kernel that refuses the widened form.
    #[cfg(target_vendor = "apple")]
    pub(crate) wide: bool,
    /// Where the kernel lists the fds of the process (Linux):
    /// [`linux::PROC_SELF_FD`] outside the tests, which point it
    /// elsewhere to stand in for a system without /proc.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) proc_self_fd: PathBuf,
    /// Whether to try the thread-cwd means at all (Linux, Apple). Always
    /// `true` outside the tests, which turn it off to stand in for a system
    /// where it cannot be used (a seccomp filter, `unshare` refused, no
    /// `pthread_fchdir_np`). Where it is tried and turns out unusable, the
    /// next means is tried just the same.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    pub(crate) thread_cwd: bool,
    /// How the socket's directory is opened once a route needs its fd
    /// ([`Target::dir_fd`]): [`open_dir`] outside the tests, which put a
    /// failing one in its place to stand in for a directory that cannot be
    /// opened (no read permission where it is opened `O_RDONLY`).
    pub(crate) open_socket_dir: fn(&Path) -> io::Result<OwnedFd>,
    /// How D is opened right after it is made ([`own::OwnDir::hold`]):
    /// [`own::open_own_dir`] outside the tests, which put a failing one in its
    /// place to stand in for a D that a umask made impossible to open where
    /// the kernel would open it anyway (`O_PATH` on Linux).
    pub(crate) open_own_dir: own::OpenAt,
}

/// A place where D may be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Place {
    /// A directory by its path. D is named `sockat.XXXXXXXXXX` there.
    Path(PathBuf),
    /// The socket's directory, through the fd the route order opened. D is
    /// named `.sockat.XXXXXXXXXX` there: the directory is the user's, and a
    /// D left behind by a process that died stays out of plain listings and
    /// is recognisable as sockat's.
    SocketDir,
}

impl Config {
    pub(crate) fn from_env() -> Self {
        Self::from_vars(
            std::env::var_os("TMPDIR"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        )
    }

    /// The configuration for a `TMPDIR` of `tmpdir` and an `XDG_RUNTIME_DIR`
    /// of `runtime_dir`.
    ///
    /// D goes in `${TMPDIR:-/tmp}` (a relative `TMPDIR` too: it is opened
    /// once and held by its fd, and no spelling names it), then `/tmp` unless
    /// that is the same spelling, then the socket's directory. A place needs
    /// neither to be safe nor short: D is held by its fd and checked to be
    /// the one made.
    ///
    /// The temporary symlink of the fallback route goes in `TMPDIR` when it is
    /// absolute (a relative one would put the link somewhere depending on the
    /// cwd), then `/tmp`, tried once when `TMPDIR` is `/tmp`. That of
    /// `bind_any` goes in `XDG_RUNTIME_DIR`, `TMPDIR` (each when absolute)
    /// and `/tmp`, a spelling once.
    pub(crate) fn from_vars(tmpdir: Option<OsString>, runtime_dir: Option<OsString>) -> Self {
        let tmp = Path::new("/tmp");
        let set = |v: Option<OsString>| v.map(PathBuf::from).filter(|t| !t.as_os_str().is_empty());
        let tmpdir = set(tmpdir);
        let mut bind_link_places: Vec<PathBuf> = Vec::new();
        for place in set(runtime_dir)
            .into_iter()
            .chain(tmpdir.clone())
            .filter(|p| p.is_absolute())
            .chain([tmp.to_path_buf()])
        {
            if !bind_link_places.contains(&place) {
                bind_link_places.push(place);
            }
        }
        let first = tmpdir.clone().unwrap_or_else(|| tmp.to_path_buf());
        let mut own_dir_places = vec![Place::Path(first.clone())];
        if first != tmp {
            own_dir_places.push(Place::Path(tmp.to_path_buf()));
        }
        own_dir_places.push(Place::SocketDir);
        #[cfg(not(target_os = "freebsd"))]
        let link_places = tmpdir
            .filter(|t| t.is_absolute() && t != tmp)
            .into_iter()
            .chain([tmp.to_path_buf()])
            .collect();
        Self {
            own_dir_places,
            #[cfg(not(target_os = "freebsd"))]
            link_places,
            bind_link_places,
            #[cfg(target_vendor = "apple")]
            wide: true,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            proc_self_fd: PathBuf::from(linux::PROC_SELF_FD),
            #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
            thread_cwd: true,
            open_socket_dir: open_dir,
            open_own_dir: own::open_own_dir,
        }
    }
}

/// One long path being bound / connected, with what the route order has
/// found out about it so far.
struct Target<'a> {
    req: Request,
    /// The path as given (the fallback routes spell it; FreeBSD's does not).
    #[cfg_attr(target_os = "freebsd", allow(dead_code))]
    path: &'a Path,
    dir: &'a Path,
    name: &'a OsStr,
    cfg: &'a Config,
    /// The socket's directory opened by `cfg.open_socket_dir`, once a route
    /// needs its fd ([`Target::dir_fd`]); `None` when it cannot be opened.
    dir_fd: OnceCell<Option<OwnedFd>>,
    /// Where `/proc/self/fd` is when /proc can be used, and the number of the
    /// fd it was checked with ([`Target::probed_proc`]).
    proc: OnceCell<Option<(&'a Path, RawFd)>>,
    /// The real path of the socket's directory, what R points to.
    real_dir: PathBuf,
    /// Set once D could be made in none of the places, so the fallback's
    /// symlink in D is not tried in them again.
    no_place: Cell<bool>,
    /// Whether to take the [`Bound`] of a bind ([`bind_remembering`]), and
    /// where it is kept once taken.
    remember: bool,
    bound: OnceCell<Bound>,
    /// The routes passed over so far, in the order tried ([`Target::skip`]).
    skipped: RefCell<Vec<Skipped>>,
}

impl<'a> Target<'a> {
    fn real_dir(&self) -> &Path {
        &self.real_dir
    }

    /// Note that the route `name` was tried and cannot be used, for `reason`
    /// ([`names`]).
    fn skip(&self, name: &'static str, reason: &'static str) {
        self.skipped.borrow_mut().push(Skipped { name, reason });
    }

    /// The socket's directory's fd, opened the first time a route asks for
    /// it; `None` when it cannot be opened (then the routes that need it
    /// cannot be used). Only the /proc and connectat fallbacks, the fork
    /// routes in it and D or R made in it need it: the central route reaches
    /// the directory through R in D, with search permission alone, where
    /// opening it `O_RDONLY` would need read permission as well.
    fn dir_fd(&self) -> Option<BorrowedFd<'_>> {
        self.dir_fd
            .get_or_init(|| (self.cfg.open_socket_dir)(self.dir).ok())
            .as_ref()
            .map(|fd| fd.as_fd())
    }

    /// Where `/proc/self/fd` is (`cfg.proc_self_fd`) when /proc can be used
    /// ([`Target::probed_proc`]).
    fn proc(&self) -> Option<&'a Path> {
        self.probed_proc().map(|(proc, _)| proc)
    }

    /// Where `/proc/self/fd` is when /proc can be used, checked once with `/`
    /// ([`linux::probe`]), and the number the fd of `/` got (Linux). The
    /// socket's directory's fd is not used for this: the /proc means of the
    /// central route works on D's fd, and the check is needed before D is
    /// made. Without the check a call through `<it>/<fd>/...` would fail with
    /// `ENOENT`, which reads as "the directory does not exist" although it
    /// does. Always `None` elsewhere.
    fn probed_proc(&self) -> Option<(&'a Path, RawFd)> {
        *self.proc.get_or_init(|| {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            return linux::probe(&self.cfg.proc_self_fd)
                .map(|fd| (self.cfg.proc_self_fd.as_path(), fd));
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            None
        })
    }
}

/// Where a route stands after trying.
pub(crate) enum Step {
    /// The route made the call (or failed for real); this is the result.
    Done(Call),
    /// The route could not be used; nothing it made is left. Try the next.
    Next,
}

/// [`run_long_with`] without the [`Bound`]: the tests' way into the routes for
/// a long path with inputs of their own.
#[cfg(test)]
pub(crate) fn run_long(req: Request, path: &Path, cfg: &Config) -> io::Result<(OwnedFd, Route)> {
    run_long_with(req, path, cfg, false)
        .map(|r| (r.fd, r.route))
        .map_err(|f| f.error)
}

/// The routes for a path longer than [`DIRECT_MAX`], with what they read from
/// the environment given as `cfg` (the tests pass their own), and with
/// `remember` the [`Bound`] of a bind. See the module doc for the order.
///
/// The socket's directory is looked up (`stat`) first, so a missing directory
/// is `ENOENT` whatever the file name (DR-0012), and a file in its place
/// `ENOTDIR`, as a bind / connect through the path would answer. Its real
/// path (what R points to) is taken next, before any route is tried; when it
/// cannot be resolved that error is returned as is, rather than turning into
/// `ENAMETOOLONG` once no R can be made.
///
/// A failure up to there is one before any route, with nothing passed over:
/// the length is judged only once the directory is looked up, so `standard`
/// is passed over (its spelling, the path itself, too long) only from then on.
pub(crate) fn run_long_with(
    req: Request,
    path: &Path,
    cfg: &Config,
    remember: bool,
) -> Result<Reached, Failed> {
    let before = Failed::before_routes;
    let (dir, name) = path::split(path).map_err(before)?;
    if !std::fs::metadata(dir).map_err(before)?.is_dir() {
        return Err(before(io::Error::from_raw_os_error(libc::ENOTDIR)));
    }
    let real_dir = std::fs::canonicalize(dir).map_err(before)?;
    let t = Target {
        req,
        path,
        dir,
        name,
        cfg,
        dir_fd: OnceCell::new(),
        proc: OnceCell::new(),
        real_dir,
        no_place: Cell::new(false),
        remember,
        bound: OnceCell::new(),
        skipped: RefCell::new(vec![Skipped {
            name: names::STANDARD,
            reason: names::TOO_LONG,
        }]),
    };
    // The call a route made, or `None` when no route could be used.
    let call = (|| {
        if let Step::Done(call) = central(&t) {
            return Some(call);
        }
        let options = match req {
            Request::Bind(o) => {
                if o.symlink {
                    if let Step::Done(call) = links::bind_through(&t) {
                        return Some(call);
                    }
                }
                return None;
            }
            Request::Connect(options) => options,
        };
        let mut wide = wide_allowed(cfg);
        if let Step::Done(call) = fallback(&t, &mut wide) {
            return Some(call);
        }
        fork_beside(options, &t, wide)
    })();
    let skipped = t.skipped.into_inner();
    match call {
        Some(Call {
            result: Ok((fd, route)),
            ..
        }) => Ok(Reached {
            fd,
            route,
            bound: t.bound.into_inner(),
            skipped,
        }),
        Some(Call {
            name,
            result: Err(error),
        }) => Err(Failed {
            error,
            route: Some(name),
            skipped,
        }),
        None => Err(Failed {
            error: too_long(),
            route: None,
            skipped,
        }),
    }
}

/// Whether a connect may still use the widened `sockaddr_un` (Apple).
#[cfg(target_vendor = "apple")]
fn wide_allowed(cfg: &Config) -> bool {
    cfg.wide
}

/// No widened `sockaddr_un` outside Apple.
#[cfg(not(target_vendor = "apple"))]
fn wide_allowed(_cfg: &Config) -> bool {
    false
}

// ---------------------------------------------------------------------------
// the central route
// ---------------------------------------------------------------------------

/// A way of making the call with a spelling relative to a directory fd (D's,
/// or the socket's directory's without D).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Means {
    /// A thread of its own whose working directory alone is the directory
    /// (Linux, Apple; [`sys::op_in_dir_on_thread`]).
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    ThreadCwd,
    /// `/proc/self/fd/<fd>/<spelling>` (Linux).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    Proc,
    /// `bindat(2)` / `connectat(2)` (FreeBSD).
    #[cfg(target_os = "freebsd")]
    At,
    /// A forked child that `fchdir`s into the directory (a bind only here).
    /// FreeBSD has none: bindat cannot turn out unusable, so nothing comes
    /// after it.
    #[cfg(not(target_os = "freebsd"))]
    Fork,
}

impl Means {
    /// The name of the route this means makes, in D or (`sockdir`) beside
    /// the socket.
    fn name(self, sockdir: bool) -> &'static str {
        match (self, sockdir) {
            #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
            (Means::ThreadCwd, false) => names::THREAD_CWD,
            #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
            (Means::ThreadCwd, true) => names::THREAD_CWD_SOCKDIR,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            (Means::Proc, false) => names::PROC,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            (Means::Proc, true) => names::PROC_SOCKDIR,
            #[cfg(target_os = "freebsd")]
            (Means::At, false) => names::AT,
            #[cfg(target_os = "freebsd")]
            (Means::At, true) => names::AT_SOCKDIR,
            #[cfg(not(target_os = "freebsd"))]
            (Means::Fork, false) => names::FORK,
            #[cfg(not(target_os = "freebsd"))]
            (Means::Fork, true) => names::FORK_SOCKDIR,
        }
    }
}

/// The means of the central route for a call, each with why it cannot be
/// used (`None` when it may be: whether it really can is known only once it
/// is tried).
type MeansList = Vec<(Means, Option<&'static str>)>;

/// Note every means of `all` as passed over (`sockdir`: under its name beside
/// the socket), each for its own reason, the usable ones for `reason`.
fn skip_all(
    t: &Target<'_>,
    all: &[(Means, Option<&'static str>)],
    sockdir: bool,
    reason: &'static str,
) {
    for &(m, why) in all {
        t.skip(m.name(sockdir), why.unwrap_or(reason));
    }
}

/// The central route: D, R, and the means in order (module doc).
///
/// The length and whether each means can be used (as far as that can be told
/// beforehand) are checked before D is made; when no means is left, nothing is
/// made. Every means but the /proc one takes `R/<name>`, 11 bytes more than
/// the file name, in the plain sockaddr. A bind never widens a sockaddr: the
/// address of a socket bound so would not fit `sun_path` and would break its
/// readers (DR-0027).
///
/// D is tried in [`Config::own_dir_places`] in order. Made in none of them, a
/// bind tries the same means with R made beside the socket
/// ([`without_own_dir`]) and a connect goes on to the fallback routes.
fn central(t: &Target<'_>) -> Step {
    let all = central_means(t);
    if all.iter().all(|(_, why)| why.is_some()) {
        skip_all(t, &all, false, names::TOO_LONG);
        return Step::Next;
    }
    let real_dir = t.real_dir();
    match make_own_dir(t, real_dir, own::LinkName::Random) {
        Some(d) => {
            let step = through(t, d.anchor(), &all);
            drop(d);
            step
        }
        None => {
            skip_all(t, &all, false, names::NO_PLACE);
            match t.req {
                Request::Bind(_) => without_own_dir(t, real_dir, &all),
                Request::Connect(_) => Step::Next,
            }
        }
    }
}

/// The means of the central route for `t` in order, those of this target and
/// its state (the /proc means only where /proc can be used, as the SPEC's
/// proc is for a Linux with /proc), each with why it cannot be used as far as
/// that can be told before D is made: the spelling `R/<name>` does not fit,
/// the thread-cwd means is off or unavailable, the /proc spelling would not
/// fit.
// Design rationale: each push is behind the cfg of the targets that have the
// means, so on some targets the list is one push after `new`.
#[allow(clippy::vec_init_then_push)]
fn central_means(t: &Target<'_>) -> MeansList {
    let fits = path::len(&own::spelling_placeholder(OsStr::new(""), t.name)) <= DIRECT_MAX;
    let unless_too_long = |why: Option<&'static str>| match fits {
        true => why,
        false => Some(names::TOO_LONG),
    };
    let mut all = MeansList::new();
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    all.push((
        Means::ThreadCwd,
        unless_too_long(if !t.cfg.thread_cwd {
            Some(names::THREAD_CWD_OFF)
        } else if !sys::thread_cwd_available() {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            let why = names::SECCOMP;
            #[cfg(target_vendor = "apple")]
            let why = names::NO_PTHREAD_FCHDIR;
            Some(why)
        } else {
            None
        }),
    ));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Some((proc, probe)) = t.probed_proc() {
        all.push((
            Means::Proc,
            unless_too_long(None).or_else(|| {
                // D's fd is not known until D is opened. The fd of `/` opened
                // by the check got the lowest free number; the place is opened
                // before D, so one more stands in for the number of digits of
                // D's fd, and the spelling is measured again with D's own fd
                // before it is used ([`by_means`]; when it does not fit, D is
                // removed and the next means taken).
                let estimate = proc
                    .join((probe + 1).to_string())
                    .join(own::spelling_placeholder(OsStr::new(""), t.name));
                (path::len(&estimate) > DIRECT_MAX).then_some(names::TOO_LONG)
            }),
        ));
    }
    // bindat / connectat cannot turn out unusable, so nothing comes after it.
    #[cfg(target_os = "freebsd")]
    all.push((Means::At, unless_too_long(None)));
    #[cfg(not(target_os = "freebsd"))]
    if t.req.is_bind() {
        all.push((Means::Fork, unless_too_long(None)));
    }
    all
}

/// A directory fd and the name of a symlink in it to the socket's directory:
/// the call is made with `<link>/<name>` relative to `fd`.
#[derive(Clone, Copy)]
pub(crate) struct Anchor<'a> {
    pub(crate) fd: BorrowedFd<'a>,
    pub(crate) link: &'a OsStr,
    /// Whether `fd` is the socket's directory (R made beside the socket, the
    /// `-sockdir` routes) rather than D.
    pub(crate) sockdir: bool,
}

/// Try the means of `all` in order with `<anchor.link>/<name>` relative to
/// `anchor.fd`, noting each one passed over (those `all` already rules out,
/// and those that turn out unusable). The first means that can be used makes
/// the call, and its outcome is the result; [`Step::Next`] when none could
/// be used.
fn through(t: &Target<'_>, anchor: Anchor<'_>, all: &[(Means, Option<&'static str>)]) -> Step {
    let spelling = Path::new(anchor.link).join(t.name);
    let Ok(addr) = SockAddr::standard(spelling.as_os_str()) else {
        skip_all(t, all, anchor.sockdir, names::TOO_LONG);
        return Step::Next;
    };
    let tried = all.iter().find_map(|&(m, why)| match why {
        Some(why) => {
            t.skip(m.name(anchor.sockdir), why);
            None
        }
        None => by_means(t, anchor, &addr, m),
    });
    match tried {
        Some(Call {
            name,
            result: Ok(done),
        }) if t.remember && t.req.is_bind() => Step::Done(Call {
            name,
            result: remember_through(t, &spelling, anchor.fd).map(|()| done),
        }),
        Some(call) => Step::Done(call),
        None => Step::Next,
    }
}

/// Take the [`Bound`] of the socket file just bound by `spelling` relative to
/// `dir` (D's fd with `R/<name>`, or the socket's directory's with
/// `.sockat.XXXXXXXXXX/<name>`), while R is still there: `(st_dev, st_ino)`
/// by that same spelling (R followed, the socket file itself not), and the
/// real path R points to joined with the file name.
fn remember_through(t: &Target<'_>, spelling: &Path, dir: BorrowedFd<'_>) -> io::Result<()> {
    let id = sys::id_at(dir, spelling.as_os_str())?;
    let real_dir = t.real_dir();
    let _ = t.bound.set(Bound {
        path: real_dir.join(t.name),
        id,
    });
    Ok(())
}

/// Make the call by `means` with `<anchor.link>/<name>` (in `addr`)
/// relative to `anchor.fd`; `None` when that means cannot be used, nothing
/// having been called (noted in `t` with why).
fn by_means(t: &Target<'_>, anchor: Anchor<'_>, addr: &SockAddr, means: Means) -> Option<Call> {
    match means {
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        Means::ThreadCwd => match thread_cwd_in(t.req, anchor.fd, addr) {
            sys::OnThread::Done(done) => {
                let route = match anchor.sockdir {
                    false => Route::ThreadCwd,
                    true => Route::ThreadCwdSockdir,
                };
                Some(Call::made(route, done))
            }
            sys::OnThread::Unusable(_) => {
                t.skip(means.name(anchor.sockdir), names::THREAD_CWD_REFUSED);
                None
            }
        },
        #[cfg(any(target_os = "linux", target_os = "android"))]
        Means::Proc => {
            let name = means.name(anchor.sockdir);
            let Some(fd_dir) = t.proc().and_then(|proc| linux::fd_dir(proc, anchor.fd)) else {
                t.skip(name, names::PROC_UNUSABLE);
                return None;
            };
            let via = fd_dir.join(anchor.link).join(t.name);
            if path::len(&via) > DIRECT_MAX {
                t.skip(name, names::TOO_LONG);
                return None;
            }
            let route = match anchor.sockdir {
                false => Route::Proc,
                true => Route::ProcSockdir,
            };
            Some(Call::made(route, plain_op(t.req, &via)))
        }
        #[cfg(target_os = "freebsd")]
        Means::At => {
            let route = match anchor.sockdir {
                false => Route::At,
                true => Route::AtSockdir,
            };
            Some(Call::made(route, freebsd::at(t.req, anchor.fd, addr)))
        }
        #[cfg(not(target_os = "freebsd"))]
        Means::Fork => Some(Call {
            name: means.name(anchor.sockdir),
            result: fork_in(t.req, anchor.fd, addr).map(|(fd, child)| {
                let route = match anchor.sockdir {
                    false => Route::Fork { child },
                    true => Route::ForkSockdir { child },
                };
                (fd, route)
            }),
        }),
    }
}

/// Make D in the first of [`Config::own_dir_places`] where it can be made,
/// with the symlink `link` to `target` in it. `None` when it can be made in
/// none of them (remembered in `t.no_place`, so the next attempt for `t`
/// does not try them again).
///
/// A place is passed over when it cannot be opened, D cannot be created there
/// (no free name in [`own::ATTEMPTS`] tries, no random bytes, no write
/// permission), what was created fails the checks of [`own::OwnDir::hold`],
/// or the symlink cannot be made in it. What fails the checks is left alone;
/// everything else made for a place passed over is removed again.
fn make_own_dir(t: &Target<'_>, target: &Path, link: own::LinkName) -> Option<own::OwnDir> {
    if t.no_place.get() {
        return None;
    }
    for place in &t.cfg.own_dir_places {
        // Only D in the socket's directory needs its fd.
        let socket_dir = match place {
            Place::SocketDir => t.dir_fd(),
            Place::Path(_) => None,
        };
        if let Ok(d) = own::OwnDir::create(
            place,
            socket_dir,
            t.proc(),
            t.cfg.open_own_dir,
            target,
            link,
        ) {
            return Some(d);
        }
    }
    t.no_place.set(true);
    None
}

/// A bind for which D could be made in none of the places: R
/// (`.sockat.XXXXXXXXXX`, to the socket's directory) is made in the socket's
/// directory itself, and the call is made with `<R>/<name>` relative to the
/// socket's directory fd by the same means as with D (thread cwd, /proc,
/// fork; bindat on FreeBSD), each measured again against this spelling
/// (hub DR-0012, D12-Q1). R is removed when this returns. [`Step::Next`]
/// (and so `ENAMETOOLONG`) when the socket's directory cannot be opened, the
/// spelling does not fit or R cannot be made.
fn without_own_dir(t: &Target<'_>, real_dir: &Path, all: &[(Means, Option<&'static str>)]) -> Step {
    let placeholder = own::spelling_placeholder(OsStr::new(own::HIDDEN_PREFIX), t.name);
    if path::len(&placeholder) > DIRECT_MAX {
        skip_all(t, all, true, names::TOO_LONG);
        return Step::Next;
    }
    let Some(dir_fd) = t.dir_fd() else {
        skip_all(t, all, true, names::SOCKDIR_UNOPENABLE);
        return Step::Next;
    };
    let Ok(link) = own::LinkBeside::create(dir_fd, real_dir) else {
        skip_all(t, all, true, names::NO_R);
        return Step::Next;
    };
    let step = through(t, link.anchor(), all);
    drop(link);
    step
}

/// sockat's own directory D and the symlinks made in it and beside a socket
/// (SPEC "中心の経路").
pub(crate) mod own {
    use super::*;

    /// What D's name starts with in a place other than the socket's
    /// directory, so a D left behind by a process that died is recognisable
    /// as sockat's.
    pub(crate) const PREFIX: &str = "sockat.";

    /// What D's name starts with in the socket's directory, and the name of
    /// R made there when D can be made nowhere ([`LinkBeside`]).
    pub(crate) const HIDDEN_PREFIX: &str = ".sockat.";

    /// Length of the random part of every name sockat creates.
    pub(crate) const RANDOM_LEN: usize = 10;

    /// How many names to try before giving up. Each try is a fresh random
    /// name from 62^10 possibilities, so exhausting this means something
    /// other than bad luck (for example a place that refuses every name).
    pub(crate) const ATTEMPTS: usize = 16;

    /// The name of the symlink in D to the socket file (the fallback route
    /// [`Route::ProcHeld`]).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) const FILE_LINK: &str = "s";

    /// `<prefix>XXXXXXXXXX/<name>`: the length of the spelling through a link
    /// whose name is `prefix` and the random part, before any is made.
    pub(crate) fn spelling_placeholder(prefix: &OsStr, name: &OsStr) -> PathBuf {
        let mut link = prefix.to_os_string();
        link.push("X".repeat(RANDOM_LEN));
        Path::new(&link).join(name)
    }

    /// [`RANDOM_LEN`] random letters and digits from the system's secure
    /// generator ([`sys::random_bytes`]; when it fails, as `getrandom(2)`
    /// does on a Linux older than 3.17, nothing that needs a name can be
    /// made).
    pub(crate) fn random_part() -> io::Result<String> {
        const ALPHABET: &[u8; 62] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut random = [0u8; RANDOM_LEN];
        sys::random_bytes(&mut random)?;
        // 256 is not a multiple of 62, so this is slightly biased; the name
        // only has to be hard to collide with, not uniform.
        Ok(random
            .iter()
            .map(|b| ALPHABET[*b as usize % 62] as char)
            .collect())
    }

    /// `prefix` followed by a fresh [`random_part`].
    pub(crate) fn named(prefix: &str) -> io::Result<String> {
        Ok(format!("{prefix}{}", random_part()?))
    }

    /// Create an entry with `create`, named by `next_name` (called once per
    /// try; its error ends the attempt), and return its name.
    ///
    /// `create` must fail with `EEXIST` rather than replace an existing entry,
    /// as `symlink(2)` and `mkdir(2)` do (their `O_EXCL`): the entry created is
    /// always one nobody else had, and a name already taken (by anyone) is
    /// skipped for the next one, [`ATTEMPTS`] times at most. Any other error
    /// ends the attempt.
    pub(crate) fn create_unique(
        mut create: impl FnMut(&OsStr) -> io::Result<()>,
        mut next_name: impl FnMut() -> io::Result<String>,
    ) -> io::Result<OsString> {
        for _ in 0..ATTEMPTS {
            let name = OsString::from(next_name()?);
            match create(&name) {
                Ok(()) => return Ok(name),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not find a free name for a temporary entry",
        ))
    }

    /// Which symlink D holds.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum LinkName {
        /// R: a random name ([`RANDOM_LEN`] letters and digits, no prefix),
        /// to the socket's directory (the central route).
        Random,
        /// [`FILE_LINK`], to the socket file (the fallback route
        /// [`Route::ProcHeld`]).
        #[cfg(any(target_os = "linux", target_os = "android"))]
        File,
    }

    /// sockat's own directory D, made in a place, held by an fd, and the
    /// symlink made in it.
    ///
    /// D is reached through the fd, not its name, so someone renaming it
    /// away and putting their own directory at its name changes nothing: the
    /// fd keeps naming the directory sockat made. A replacement between
    /// `mkdirat` and `openat` is what the checks of [`OwnDir::hold`] are for.
    /// So a place needs to be neither safe nor short.
    ///
    /// A value exists only once D passed the checks of [`OwnDir::hold`] (what
    /// fails them is someone else's and is left alone, its contents included),
    /// and `link` is set only once this value's own `symlinkat` made the link.
    /// On drop: the link is removed through the fd if this value made it,
    /// then D through the place's fd, but only while D's name there is still
    /// the held directory (same `(st_dev, st_ino)` as the fd, looked up without
    /// following a symlink), so a directory put in its place is left alone.
    /// Failures are ignored: the call is made by then, and failing it would
    /// not remove anything either.
    pub(crate) struct OwnDir {
        /// The place D was made in, opened by [`open_dir`].
        base: OwnedFd,
        /// D's name in the place.
        pub(crate) name: OsString,
        /// D, opened by [`OwnDir::hold`].
        dir: std::fs::File,
        /// The symlink this value made in D.
        link: Option<OsString>,
    }

    impl OwnDir {
        /// Make D in `place` ([`OwnDir::make`], [`OwnDir::hold`]) with the
        /// symlink `link` to `target` in it ([`OwnDir::link`]). `socket_dir`
        /// is the socket's directory's fd, the base for [`Place::SocketDir`]
        /// (`None` when it cannot be opened: that place is passed over);
        /// `proc` is where `/proc/self/fd` is when /proc can be used; `open`
        /// opens D ([`open_own_dir`] outside the tests).
        pub(crate) fn create(
            place: &Place,
            socket_dir: Option<BorrowedFd<'_>>,
            proc: Option<&Path>,
            open: OpenAt,
            target: &Path,
            link: LinkName,
        ) -> io::Result<Self> {
            let (base, prefix) = match place {
                Place::Path(p) => (open_dir(p)?, PREFIX),
                Place::SocketDir => {
                    let Some(socket_dir) = socket_dir else {
                        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                    };
                    (socket_dir.try_clone_to_owned()?, HIDDEN_PREFIX)
                }
            };
            let name = Self::make(base.as_fd(), || named(prefix))?;
            let mut d = Self::hold(base, name, proc, open)?;
            d.link(target, link)?;
            Ok(d)
        }

        /// Create a directory of mode 0700 in the place `base` (`mkdirat`),
        /// named by `next_name` ([`create_unique`]), and return its name.
        pub(crate) fn make(
            base: BorrowedFd<'_>,
            next_name: impl FnMut() -> io::Result<String>,
        ) -> io::Result<OsString> {
            create_unique(|name| sys::mkdir_at(base, name, 0o700), next_name)
        }

        /// Open `name` in the place `base` and, if it is the directory sockat
        /// made, hold it.
        ///
        /// It is opened by `open` ([`open_own_dir`] outside the tests: with
        /// `O_DIRECTORY | O_NOFOLLOW`, so a symlink or anything but a
        /// directory put at `name` is refused, and with `O_PATH` on Linux,
        /// `O_SEARCH` on Apple, `O_RDONLY` elsewhere). What was opened must be a directory
        /// owned by the effective uid with no permission for group and others
        /// (`mode & 0o077 == 0`), as [`OwnDir::make`] makes it: anything else
        /// was put there by someone else between the two steps, and is neither
        /// used nor removed (`InvalidData`).
        ///
        /// Only the group and other bits are judged because the umask may
        /// have taken some of the owner's from the 0700 asked for (umask 0277
        /// makes it 0500, umask 0477 makes it 0300, without read); a umask
        /// cannot add group or other bits. `O_PATH` opens D whatever the umask
        /// left, `O_SEARCH` without read permission. When D cannot be opened
        /// at all (`O_SEARCH` without the owner's search bit, `O_RDONLY`
        /// without the read bit), it is removed by its name if it passes the
        /// same checks looked up there ([`remove_unopened`]), and the error
        /// returned: the place is passed over with nothing left.
        ///
        /// Once checked, the owner's bits are set to 0700 ([`OwnDir::set_mode`]).
        /// When that cannot be done, D is removed again (by drop) and the
        /// error returned: the place is passed over.
        pub(crate) fn hold(
            base: OwnedFd,
            name: OsString,
            proc: Option<&Path>,
            open: OpenAt,
        ) -> io::Result<Self> {
            use std::os::unix::fs::MetadataExt;
            let fd = match open(base.as_fd(), &name) {
                Ok(fd) => fd,
                Err(e) => {
                    remove_unopened(base.as_fd(), &name);
                    return Err(e);
                }
            };
            let dir = std::fs::File::from(fd);
            let m = dir.metadata()?;
            if !(m.is_dir() && m.uid() == sys::effective_uid() && m.mode() & 0o077 == 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the directory sockat made was replaced",
                ));
            }
            let held = Self {
                base,
                name,
                dir,
                link: None,
            };
            held.set_mode(proc)?;
            Ok(held)
        }

        /// Set D's mode to 0700 through what reaches the very directory the
        /// fd holds, so nothing put at its name meanwhile is changed instead:
        ///
        /// * an `O_PATH` fd (Linux) takes no `fchmod` (`EBADF`): with /proc
        ///   (`proc`), `chmod("<proc>/<fd>")`; without, D is opened again by
        ///   its name `O_RDONLY` (which needs the owner's read bit), checked
        ///   to be the same directory (`(st_dev, st_ino)` of both fds), and
        ///   that fd is `fchmod`ed. A umask without the owner's read bit makes
        ///   this fail there, and the place is passed over with nothing left;
        /// * any other fd: `fchmod`.
        fn set_mode(&self, proc: Option<&Path>) -> io::Result<()> {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::Permissions::from_mode(0o700);
            if let Some(proc) = proc {
                return std::fs::set_permissions(self.fd_dir(proc), mode);
            }
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                use std::os::unix::fs::MetadataExt;
                let id = |f: &std::fs::File| f.metadata().map(|m| (m.dev(), m.ino()));
                let again = std::fs::File::from(sys::open_dir_at(
                    self.base.as_fd(),
                    &self.name,
                    libc::O_RDONLY,
                )?);
                if id(&again)? != id(&self.dir)? {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the directory sockat made was replaced",
                    ));
                }
                again.set_permissions(mode)
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            self.dir.set_permissions(mode)
        }

        /// Make the symlink `link` to `target` in D (`symlinkat` on the fd).
        /// R's name is drawn as [`create_unique`] does; [`FILE_LINK`] is
        /// tried once (D is empty).
        pub(crate) fn link(&mut self, target: &Path, link: LinkName) -> io::Result<()> {
            let dir = self.dir.as_fd();
            let create = |name: &OsStr| sys::symlink_at(target, dir, name);
            let name = match link {
                LinkName::Random => create_unique(create, random_part)?,
                #[cfg(any(target_os = "linux", target_os = "android"))]
                LinkName::File => {
                    let name = OsString::from(FILE_LINK);
                    create(&name)?;
                    name
                }
            };
            self.link = Some(name);
            Ok(())
        }

        /// D's fd and the symlink made in it.
        pub(crate) fn anchor(&self) -> Anchor<'_> {
            Anchor {
                fd: self.dir.as_fd(),
                link: self.link.as_deref().unwrap_or_default(),
                sockdir: false,
            }
        }

        /// `<proc>/<fd>`: D itself, by its fd, `proc` being where
        /// `/proc/self/fd` is.
        pub(crate) fn fd_dir(&self, proc: &Path) -> PathBuf {
            proc.join(self.dir.as_raw_fd().to_string())
        }
    }

    impl Drop for OwnDir {
        fn drop(&mut self) {
            use std::os::unix::fs::MetadataExt;
            if let Some(link) = &self.link {
                let _ = sys::unlink_at(self.dir.as_fd(), link, false);
            }
            let held = self.dir.metadata().map(|m| (m.dev(), m.ino()));
            let named = sys::id_at(self.base.as_fd(), &self.name);
            if let (Ok(held), Ok(named)) = (held, named) {
                if held == named {
                    let _ = sys::unlink_at(self.base.as_fd(), &self.name, true);
                }
            }
        }
    }

    /// How D is opened: [`open_own_dir`], or a stand-in in the tests.
    pub(crate) type OpenAt = fn(BorrowedFd<'_>, &OsStr) -> io::Result<OwnedFd>;

    /// Remove D, `name` in the place `base`, which could not be opened right
    /// after it was made (a umask took the owner's bits), when what is at
    /// `name`, looked up without following a symlink, passes the checks of
    /// [`OwnDir::hold`]: a directory owned by the effective uid with no
    /// permission for group and others. Anything else is left alone.
    /// Someone who replaces it between the look-up and the removal gets at
    /// most their own empty directory removed: `AT_REMOVEDIR` removes only an
    /// empty one. Failures are ignored: the place is passed over either way.
    pub(crate) fn remove_unopened(base: BorrowedFd<'_>, name: &OsStr) {
        let ours = sys::stat_at(base, name)
            .is_ok_and(|st| st.is_dir && st.uid == sys::effective_uid() && st.mode & 0o077 == 0);
        if ours {
            let _ = sys::unlink_at(base, name, true);
        }
    }

    /// Open D, `name` in the place `base`, with `O_DIRECTORY | O_NOFOLLOW`
    /// and: `O_PATH` on Linux, `O_SEARCH` on Apple (a kernel refusing it on a
    /// directory, `EISDIR` / `EINVAL`, gets `O_RDONLY` instead, as
    /// [`search_or_read_only`] does for the places), `O_RDONLY` elsewhere.
    pub(crate) fn open_own_dir(base: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return sys::open_dir_at(base, name, libc::O_PATH);
        #[cfg(target_vendor = "apple")]
        match sys::open_dir_at(base, name, libc::O_SEARCH) {
            Err(e) if matches!(e.raw_os_error(), Some(libc::EISDIR | libc::EINVAL)) => {}
            searched => return searched,
        }
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        sys::open_dir_at(base, name, libc::O_RDONLY)
    }

    /// R made beside the socket, in its directory, when D can be made in none
    /// of the places: `.sockat.XXXXXXXXXX` to the socket's directory, removed
    /// on drop (it is always this value's own: the value exists only once
    /// `symlinkat` made it).
    pub(crate) struct LinkBeside<'a> {
        dir: BorrowedFd<'a>,
        pub(crate) name: OsString,
    }

    impl<'a> LinkBeside<'a> {
        /// Make R in the socket's directory `dir` pointing to `target`.
        pub(crate) fn create(dir: BorrowedFd<'a>, target: &Path) -> io::Result<Self> {
            let name = create_unique(
                |name| sys::symlink_at(target, dir, name),
                || named(HIDDEN_PREFIX),
            )?;
            Ok(Self { dir, name })
        }

        pub(crate) fn anchor(&self) -> Anchor<'_> {
            Anchor {
                fd: self.dir,
                link: &self.name,
                sockdir: true,
            }
        }
    }

    impl Drop for LinkBeside<'_> {
        fn drop(&mut self) {
            let _ = sys::unlink_at(self.dir, &self.name, false);
        }
    }
}

// ---------------------------------------------------------------------------
// the fallback routes (connect) and the last fork
// ---------------------------------------------------------------------------

/// The fallback routes of a connect, tried when the central route cannot be
/// used, in this order:
///
/// | target | order |
/// |---|---|
/// | Linux, Android (/proc usable) | /proc, symlink in D, symlink |
/// | Linux, Android (no /proc) | relative path, symlink |
/// | Apple | relative path, widened sockaddr, symlink |
/// | FreeBSD | connectat |
/// | NetBSD, OpenBSD, illumos, other Unix | relative path, symlink |
///
/// None of them puts a random part in the spelling: a connected socket is not
/// bound, so the spelling is not kept anywhere. `wide` says whether the
/// widened form may still be used (Apple); the kernel refusing it turns it off
/// for the fork after this too.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn fallback(t: &Target<'_>, _wide: &mut bool) -> Step {
    // Without /proc the row is that of a Linux without it, with no /proc route
    // in it to pass over.
    let Some(proc) = t.proc() else {
        return match posix::relative(t) {
            Step::Next => posix::symlinks(t),
            done => done,
        };
    };
    match t.dir_fd() {
        Some(dir_fd) => {
            let via = proc.join(dir_fd.as_raw_fd().to_string()).join(t.name);
            if path::len(&via) <= DIRECT_MAX {
                return Step::Done(Call::made(Route::ProcDirect, plain_op(t.req, &via)));
            }
            t.skip(names::PROC_DIRECT, names::TOO_LONG);
        }
        None => t.skip(names::PROC_DIRECT, names::SOCKDIR_UNOPENABLE),
    }
    match linux::file_link_in_own_dir(t, proc) {
        Step::Next => posix::symlinks(t),
        done => done,
    }
}

/// See the Linux version.
#[cfg(target_vendor = "apple")]
fn fallback(t: &Target<'_>, wide: &mut bool) -> Step {
    let rel = posix::Relative::new(t.path, t.dir);
    if let Some(rel) = rel.as_ref().filter(|r| path::len(&r.path) <= DIRECT_MAX) {
        return Step::Done(Call::made(Route::Relative, rel.run(|p| plain_op(t.req, p))));
    }
    t.skip(names::RELATIVE, names::NO_RELATIVE);
    if !*wide {
        t.skip(names::WIDENED, names::WIDENED_REFUSED);
    }
    if let (true, Request::Connect(o)) = (*wide, t.req) {
        let attempt = if path::len(t.path) <= sys::WIDE_PATH_MAX {
            Some(Call::made(Route::Widened, apple::wide_connect(o, t.path)))
        } else {
            rel.as_ref()
                .filter(|r| path::len(&r.path) <= sys::WIDE_PATH_MAX)
                .map(|rel| {
                    Call::made(
                        Route::WidenedRelative,
                        rel.run(|p| apple::wide_connect(o, p)),
                    )
                })
        };
        match attempt {
            None => t.skip(names::WIDENED, names::TOO_LONG),
            Some(Call { result: Err(e), .. }) if apple::refuses_wide(&e) => {
                t.skip(names::WIDENED, names::WIDENED_REFUSED);
                *wide = false;
            }
            Some(call) => return Step::Done(call),
        }
    }
    posix::symlinks(t)
}

/// See the Linux version.
#[cfg(target_os = "freebsd")]
fn fallback(t: &Target<'_>, _wide: &mut bool) -> Step {
    let Ok(addr) = SockAddr::standard(t.name) else {
        t.skip(names::AT_DIRECT, names::TOO_LONG);
        return Step::Next;
    };
    match t.dir_fd() {
        Some(dir_fd) => Step::Done(Call::made(
            Route::AtDirect,
            freebsd::at(t.req, dir_fd, &addr),
        )),
        None => {
            t.skip(names::AT_DIRECT, names::SOCKDIR_UNOPENABLE);
            Step::Next
        }
    }
}

/// See the Linux version.
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
)))]
fn fallback(t: &Target<'_>, _wide: &mut bool) -> Step {
    match posix::relative(t) {
        Step::Next => posix::symlinks(t),
        done => done,
    }
}

/// The last route of a connect, fork-direct: a forked child that `fchdir`s
/// into the socket's directory and connects by the file name, for the
/// connects that may fork. `None` (and so `ENAMETOOLONG`) for the others, with
/// nothing noted (fork-direct is not in their order), and when the file name
/// fits no sockaddr or the socket's directory cannot be opened. The file name
/// goes in the plain sockaddr, or on Apple in the widened one when it is too
/// long for that and the kernel takes the widened form (`wide`).
fn fork_beside(options: ConnectOptions, t: &Target<'_>, wide: bool) -> Option<Call> {
    let req = Request::Connect(options);
    if !req.may_fork() {
        return None;
    }
    #[cfg(target_vendor = "apple")]
    let addr = match SockAddr::standard(t.name) {
        Err(e) if wide && e.raw_os_error() == Some(libc::ENAMETOOLONG) => SockAddr::wide(t.name),
        addr => addr,
    };
    #[cfg(not(target_vendor = "apple"))]
    let addr = {
        let _ = wide;
        SockAddr::standard(t.name)
    };
    let Ok(addr) = addr else {
        t.skip(names::FORK_DIRECT, names::TOO_LONG);
        return None;
    };
    let Some(dir_fd) = t.dir_fd() else {
        t.skip(names::FORK_DIRECT, names::SOCKDIR_UNOPENABLE);
        return None;
    };
    Some(Call {
        name: names::FORK_DIRECT,
        result: fork_in(req, dir_fd, &addr).map(|(fd, child)| (fd, Route::ForkDirect { child })),
    })
}

// ---------------------------------------------------------------------------
// the calls themselves
// ---------------------------------------------------------------------------

/// Bind (and listen) or connect with `path` as is, in the plain sockaddr.
///
/// A bind and a blocking connect go to std unchanged. std cannot make a
/// nonblocking connect (`UnixStream::connect` creates and connects its socket
/// in one call), so that one is made on sockat's own socket, set up as std's
/// ([`sys::socket`]) plus `O_NONBLOCK`, with the same address bytes std would
/// build ([`SockAddr::standard`]).
pub(crate) fn plain_op(req: Request, path: &Path) -> io::Result<OwnedFd> {
    match req {
        Request::Bind(_) => UnixListener::bind(path).map(OwnedFd::from),
        Request::Connect(o) if !o.nonblocking => UnixStream::connect(path).map(OwnedFd::from),
        Request::Connect(_) => raw_op(req, &SockAddr::standard(path.as_os_str())?),
    }
}

/// Bind (and listen) or connect a fresh socket (`O_NONBLOCK` for a
/// nonblocking connect) to `addr`.
pub(crate) fn raw_op(req: Request, addr: &SockAddr) -> io::Result<OwnedFd> {
    let sock = sys::socket(req.nonblocking())?;
    sys::op(req.op(), sock.as_fd(), addr)?;
    if req.is_bind() {
        sys::listen(sock.as_fd())?;
    }
    Ok(sock)
}

/// Open `dir`, the socket's directory or a place for D, to use as the base of
/// `*at` calls, `fchdir` and `/proc/self/fd/<fd>`.
///
/// Apple: `O_SEARCH`, which needs only search permission on the directory,
/// the same as a plain bind / connect through the path (bind also needs write
/// permission, checked by the bind itself). An `O_SEARCH` directory fd can be
/// `fchdir`ed into and serves as the base of `mkdirat` / `symlinkat` (checked
/// on macOS 26.5: a mode 0300 directory, which `O_RDONLY` cannot open, binds
/// fine). A kernel that does not take `O_SEARCH` on a directory answers
/// `EISDIR` (as it does for `O_EXEC` alone) or `EINVAL`; then the directory
/// is opened `O_RDONLY`, which needs read permission.
///
/// Linux: `O_PATH | O_DIRECTORY`, which needs only search permission. The fd
/// serves `/proc/self/fd/<fd>`, `fchdir(2)` (which takes an `O_PATH`
/// directory fd since Linux 3.5) and the `*at` calls.
///
/// Other targets: `O_RDONLY | O_DIRECTORY`, so the fork route and `bindat`
/// need read permission on the directory.
pub(crate) fn open_dir(dir: &Path) -> io::Result<OwnedFd> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(dir)
            .map(OwnedFd::from)
    }
    #[cfg(target_vendor = "apple")]
    {
        // `read(true)` is O_RDONLY (0), so the access mode is O_SEARCH's own
        // O_EXEC bit; std keeps custom flags outside O_ACCMODE.
        let searched = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_SEARCH)
            .open(dir)
            .map(OwnedFd::from);
        search_or_read_only(dir, searched)
    }
    #[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
    open_read_only(dir)
}

/// Open `dir` `O_RDONLY | O_DIRECTORY` (needs read permission).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn open_read_only(dir: &Path) -> io::Result<OwnedFd> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(dir)
        .map(OwnedFd::from)
}

/// Keep the outcome of opening `dir` `O_SEARCH`, unless it is the answer of a
/// kernel that does not take `O_SEARCH` on a directory (`EISDIR` or
/// `EINVAL`); then open `dir` `O_RDONLY` instead. Any other error (`ENOENT`,
/// `EACCES`, ...) is the real outcome and is returned as is.
///
/// The outcome is taken as a value so the tests can stand in for such a
/// kernel, which the running one is not.
#[cfg(target_vendor = "apple")]
pub(crate) fn search_or_read_only(
    dir: &Path,
    searched: io::Result<OwnedFd>,
) -> io::Result<OwnedFd> {
    match searched {
        Err(e) if matches!(e.raw_os_error(), Some(libc::EISDIR | libc::EINVAL)) => {
            open_read_only(dir)
        }
        searched => searched,
    }
}

/// Bind (and listen) or connect a fresh socket (`O_NONBLOCK` for a
/// nonblocking connect) to `addr`, resolved relative to `dir`, from a forked
/// child that `fchdir`s into `dir` ([`sys::op_in_dir_forked`]). The forced
/// form of the fork route for the tests: `dir` is opened by [`open_dir`].
#[cfg(test)]
pub(crate) fn fork_op(
    req: Request,
    dir: &Path,
    addr: &SockAddr,
) -> io::Result<(OwnedFd, libc::pid_t)> {
    fork_in(req, open_dir(dir)?.as_fd(), addr)
}

/// The fork route with the directory fd `dir`: returns the socket and the
/// child's pid.
///
/// When `req` rules the fork route out, this is where it ends, with
/// [`fork_ruled_out`], before any socket is created.
///
/// The socket is made `O_NONBLOCK` here, before the fork, so the child's
/// connect runs on a nonblocking socket: the flag belongs to the open file
/// description that parent and child share.
///
/// After a bind, the listen is made here, by this process, never by the
/// child: the process that listens is the one a client's peer-credential
/// query (`LOCAL_PEERPID`, `SO_PEERCRED`) names as the server, so a child
/// that listened would leave clients seeing an exited process (DR-0025).
fn fork_in(
    req: Request,
    dir: BorrowedFd<'_>,
    addr: &SockAddr,
) -> io::Result<(OwnedFd, libc::pid_t)> {
    if !req.may_fork() {
        return Err(fork_ruled_out());
    }
    let sock = sys::socket(req.nonblocking())?;
    let child = sys::op_in_dir_forked(req.op(), sock.as_fd(), dir, addr)?;
    if req.is_bind() {
        sys::listen(sock.as_fd())?;
    }
    Ok((sock, child))
}

/// Bind (and listen) or connect a fresh socket (`O_NONBLOCK` for a
/// nonblocking connect) to `addr`, resolved relative to `dir`, on a thread
/// whose own working directory is moved there ([`sys::op_in_dir_on_thread`]).
/// `Unusable` when the thread's working directory cannot be moved on its own;
/// the request is then still to be made.
///
/// Every request may take it (no side effect a server or another thread can
/// see). The listen after a bind is made here, on the calling thread.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn thread_cwd_in(req: Request, dir: BorrowedFd<'_>, addr: &SockAddr) -> sys::OnThread<OwnedFd> {
    let sock = match sys::socket(req.nonblocking()) {
        Ok(sock) => sock,
        Err(e) => return sys::OnThread::Done(Err(e)),
    };
    match sys::op_in_dir_on_thread(req.op(), sock.as_fd(), dir, addr) {
        sys::OnThread::Done(Ok(())) => {
            let listened = match req {
                Request::Bind(_) => sys::listen(sock.as_fd()),
                Request::Connect(_) => Ok(()),
            };
            sys::OnThread::Done(listened.map(|()| sock))
        }
        sys::OnThread::Done(Err(e)) => sys::OnThread::Done(Err(e)),
        sys::OnThread::Unusable(e) => sys::OnThread::Unusable(e),
    }
}

/// [`thread_cwd_in`] with the directory `dir` opened by [`open_dir`]: the
/// forced form of the thread-cwd means for the tests.
#[cfg(all(
    test,
    any(target_os = "linux", target_os = "android", target_vendor = "apple")
))]
pub(crate) fn thread_cwd_op(req: Request, dir: &Path, addr: &SockAddr) -> sys::OnThread<OwnedFd> {
    match open_dir(dir) {
        Ok(opened) => thread_cwd_in(req, opened.as_fd(), addr),
        Err(e) => sys::OnThread::Done(Err(e)),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) mod linux {
    use super::*;

    /// Where the kernel lists the fds of the calling process.
    pub(crate) const PROC_SELF_FD: &str = "/proc/self/fd";

    /// Whether /proc can be used: `<proc_self_fd>/<fd>` of `/`, opened by
    /// [`open_dir`], can be looked up. The number that fd got (the lowest free
    /// one at that moment), or `None` when /proc cannot be used.
    pub(crate) fn probe(proc_self_fd: &Path) -> Option<RawFd> {
        let root = open_dir(Path::new("/")).ok()?;
        fd_dir(proc_self_fd, root.as_fd())?;
        Some(root.as_raw_fd())
    }

    /// `<proc_self_fd>/<fd>`, or `None` when it cannot be looked up (no
    /// /proc).
    pub(crate) fn fd_dir(proc_self_fd: &Path, fd: BorrowedFd<'_>) -> Option<PathBuf> {
        let fd_dir = proc_self_fd.join(fd.as_raw_fd().to_string());
        std::fs::metadata(&fd_dir).ok()?;
        Some(fd_dir)
    }

    /// The fallback route [`Route::ProcHeld`]: D made as the central route
    /// makes it, with the symlink [`own::FILE_LINK`] in it to the socket file
    /// (the path as given, made absolute against the cwd), and the connect by
    /// `<proc>/<D's fd>/s`. That spelling is about 20 bytes whatever the
    /// directory depth and the file name. Unusable when D can be made in none
    /// of the places, or the spelling does not fit. D is removed when this
    /// returns.
    pub(super) fn file_link_in_own_dir(t: &Target<'_>, proc: &Path) -> Step {
        let Some(target) = posix::absolute(t.path) else {
            t.skip(names::PROC_HELD, names::CWD_UNREADABLE);
            return Step::Next;
        };
        let Some(d) = make_own_dir(t, &target, own::LinkName::File) else {
            t.skip(names::PROC_HELD, names::NO_PLACE);
            return Step::Next;
        };
        let via = d.fd_dir(proc).join(own::FILE_LINK);
        if path::len(&via) > DIRECT_MAX {
            t.skip(names::PROC_HELD, names::TOO_LONG);
            return Step::Next;
        }
        Step::Done(Call::made(Route::ProcHeld, plain_op(t.req, &via)))
    }
}

#[cfg(target_os = "freebsd")]
pub(crate) mod freebsd {
    use super::*;

    /// `bindat(2)` / `connectat(2)` of a fresh socket (`O_NONBLOCK` for a
    /// nonblocking connect) to `addr` relative to the directory fd `dir`,
    /// then the listen after a bind.
    pub(crate) fn at(req: Request, dir: BorrowedFd<'_>, addr: &SockAddr) -> io::Result<OwnedFd> {
        let sock = sys::socket(req.nonblocking())?;
        sys::op_at(req.op(), sock.as_fd(), dir, addr)?;
        if req.is_bind() {
            sys::listen(sock.as_fd())?;
        }
        Ok(sock)
    }
}

/// The widened `sockaddr_un` that XNU takes (Apple), for a connect only.
#[cfg(target_vendor = "apple")]
pub(crate) mod apple {
    use super::*;

    /// Connect a fresh socket (`O_NONBLOCK` for a nonblocking connect) to
    /// `path` in a widened `sockaddr_un`. There is no bind counterpart: a
    /// socket bound so has an address that does not fit `sun_path`, which
    /// readers of it break on (DR-0027).
    pub(crate) fn wide_connect(options: ConnectOptions, path: &Path) -> io::Result<OwnedFd> {
        raw_op(
            Request::Connect(options),
            &SockAddr::wide(path.as_os_str())?,
        )
    }

    /// Whether an error from a widened-address call means the kernel does not
    /// take the widened form (as opposed to a real outcome such as `ENOENT`,
    /// `EADDRINUSE` or `ECONNREFUSED`). XNU reports an address it will not take
    /// with `EINVAL`, or `ENAMETOOLONG` for one longer than `SOCK_MAXADDRLEN`.
    pub(crate) fn refuses_wide(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENAMETOOLONG))
    }
}

/// The fallback routes that need nothing but POSIX: the relative spelling and
/// the temporary symlink in a safe place (every target but FreeBSD).
#[cfg(not(target_os = "freebsd"))]
pub(crate) mod posix {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[cfg(target_vendor = "apple")]
    use crate::sys::WIDE_PATH_MAX;

    /// The longest spelling a sockaddr carries for a connect on this target:
    /// the widened form's 253 bytes on Apple, [`DIRECT_MAX`] elsewhere.
    #[cfg(target_vendor = "apple")]
    const SPELLING_MAX: usize = WIDE_PATH_MAX;
    #[cfg(not(target_vendor = "apple"))]
    const SPELLING_MAX: usize = DIRECT_MAX;

    /// `path` made absolute against the cwd, or `None` when the cwd cannot be
    /// read.
    pub(crate) fn absolute(path: &Path) -> Option<PathBuf> {
        if path.is_absolute() {
            return Some(path.to_path_buf());
        }
        std::env::current_dir().ok().map(|cwd| cwd.join(path))
    }

    /// The relative route: the path spelled relative to the cwd when that
    /// fits the plain sockaddr ([`Relative`]).
    #[cfg_attr(target_vendor = "apple", allow(dead_code))]
    pub(super) fn relative(t: &Target<'_>) -> Step {
        match Relative::new(t.path, t.dir).filter(|r| path::len(&r.path) <= DIRECT_MAX) {
            Some(rel) => Step::Done(Call::made(Route::Relative, rel.run(|p| plain_op(t.req, p)))),
            None => {
                t.skip(names::RELATIVE, names::NO_RELATIVE);
                Step::Next
            }
        }
    }

    /// The symlink route: a temporary symlink to the socket file under the
    /// first of [`Config::link_places`] where it can be used
    /// ([`symlink_route`]).
    pub(super) fn symlinks(t: &Target<'_>) -> Step {
        let Some(target) = absolute(t.path) else {
            t.skip(names::SYMLINK, names::CWD_UNREADABLE);
            return Step::Next;
        };
        let mut why = names::NO_SAFE_BASE;
        for base in &t.cfg.link_places {
            match symlink_route(t.req, base, &target) {
                Ok(done) => return Step::Done(done),
                Err(reason) => why = reason,
            }
        }
        t.skip(names::SYMLINK, why);
        Step::Next
    }

    /// A relative spelling of an absolute socket path, checked to name the
    /// same directory as the absolute one.
    pub(crate) struct Relative {
        pub(crate) path: PathBuf,
        /// `(st_dev, st_ino)` of the cwd when the spelling was made.
        cwd: (u64, u64),
    }

    fn dir_id(dir: &Path) -> Option<(u64, u64)> {
        let m = std::fs::metadata(dir).ok()?;
        Some((m.dev(), m.ino()))
    }

    /// The spellings of the cwd that the path may start with: what
    /// `getcwd(3)` reports (a real path), and `$PWD` (the path the shell used
    /// to get there, possibly through symlinks) but only when it names the cwd.
    ///
    /// `$PWD` is just an environment variable: it is stale after a `chdir(2)`
    /// that did not update it, and anyone can set it. It is used only when
    /// `stat($PWD)` and `stat(".")` agree on `(st_dev, st_ino)`; otherwise
    /// cutting it off the path would spell a directory relative to the wrong
    /// place.
    fn cwd_spellings(cwd: &Path, cwd_id: (u64, u64)) -> Vec<PathBuf> {
        let mut spellings = vec![cwd.to_path_buf()];
        if let Some(pwd) = std::env::var_os("PWD") {
            let pwd = PathBuf::from(pwd);
            if pwd.is_absolute() && pwd != cwd && dir_id(&pwd) == Some(cwd_id) {
                spellings.push(pwd);
            }
        }
        spellings
    }

    impl Relative {
        /// Spell `path` (whose directory part is `dir`) relative to the cwd.
        ///
        /// Two ways, the first one first:
        ///
        /// 1. **Below the cwd.** When `path` starts, component by component,
        ///    with a spelling of the cwd (`getcwd(3)`, or a `$PWD` that names
        ///    the cwd), cut that prefix off. The rest only descends from the
        ///    cwd along `path` itself, so it resolves the same way `path` does
        ///    (through the same symlinks) and nothing has to be resolved first.
        /// 2. **Through real paths.** Only when 1 gives nothing of at most
        ///    [`DIRECT_MAX`] bytes: resolve `dir` with `realpath(3)` and climb
        ///    out of the cwd as `getcwd(3)` reports it (both real, so every
        ///    `..` climbs out of a real directory, as the kernel does). Comparing
        ///    the path as given instead would miss the common part when one
        ///    side goes through a symlink (`/var` against `/private/var` on
        ///    macOS), and a `..` would climb out of a symlink's target rather
        ///    than the directory the path names.
        ///
        /// The shorter result is kept. `None` when `path` is not absolute, the
        /// cwd cannot be read, neither way gives a spelling that a sockaddr of
        /// this target carries ([`SPELLING_MAX`]), or the relative directory
        /// turns out not to be the absolute one (a cwd that was removed, or one
        /// changed meanwhile by another thread).
        pub(crate) fn new(path: &Path, dir: &Path) -> Option<Self> {
            if !path.is_absolute() {
                return None;
            }
            let (_, name) = path::split(path).ok()?;
            let cwd_before = dir_id(Path::new("."))?;
            let cwd = std::env::current_dir().ok()?;

            let mut best: Option<PathBuf> = None;
            fn keep(best: &mut Option<PathBuf>, rel: PathBuf) {
                if best.as_ref().is_none_or(|b| path::len(&rel) < path::len(b)) {
                    *best = Some(rel);
                }
            }
            for prefix in cwd_spellings(&cwd, cwd_before) {
                if let Some(rel) = path::below(path, &prefix) {
                    keep(&mut best, rel);
                }
            }
            let below_fits = best.as_ref().is_some_and(|b| path::len(b) <= DIRECT_MAX);
            if !below_fits {
                if let Ok(real_dir) = std::fs::canonicalize(dir) {
                    if let Some(rel) = path::relative(&real_dir.join(name), &cwd) {
                        keep(&mut best, rel);
                    }
                }
            }
            let rel = best?;

            if path::len(&rel) > SPELLING_MAX {
                return None;
            }
            let (rel_dir, _) = path::split(&rel).ok()?;
            if dir_id(rel_dir)? != dir_id(dir)? {
                return None;
            }
            if dir_id(Path::new("."))? != cwd_before {
                return None;
            }
            Some(Self {
                path: rel,
                cwd: cwd_before,
            })
        }

        /// Run `f` with the relative spelling, then confirm the cwd is still
        /// the one the spelling was made against.
        ///
        /// A relative path is resolved against the cwd at the moment of the
        /// call, and the cwd is shared by every thread of the process. If
        /// another thread changes it in between, the call went to some other
        /// directory: the result is discarded (a connected socket is closed) and
        /// an error is returned. The check cannot see a change that is undone
        /// before it runs; the guarantee is for processes that do not change
        /// the cwd while connecting.
        pub(crate) fn run<T>(&self, f: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
            let result = f(&self.path);
            if dir_id(Path::new(".")) != Some(self.cwd) {
                return Err(io::Error::other(
                    "the current directory changed while connecting through a path \
                     relative to it; the result was discarded",
                ));
            }
            result
        }
    }

    /// Connect through a symlink `<base>/sockat.XXXXXXXXXX` to `target`, the
    /// socket file (the path as given, made absolute against the cwd): a
    /// connect follows a symlink in the last component, so the spelling's
    /// length depends on neither the directory nor the file name, and the
    /// kernel resolves the link as it resolves the path.
    ///
    /// Used when the spelling fits the plain sockaddr and `base` is a place
    /// where nobody else can replace the symlink ([`links::place_kind_of`]:
    /// only this user can write to it, or it is shared). Both are
    /// checked before anything is created. Nothing is left under `base`: the
    /// symlink is removed when this returns (including on panic) whatever the
    /// outcome. `Err` with why when `base` cannot be used ([`names`]).
    pub(crate) fn symlink_route(
        req: Request,
        base: &Path,
        target: &Path,
    ) -> Result<Call, &'static str> {
        if links::place_kind_of(base).is_none() {
            return Err(names::NO_SAFE_BASE);
        }
        if path::len(&links::placeholder(base)) > DIRECT_MAX {
            return Err(names::TOO_LONG);
        }
        let Ok(link) = links::TempLink::create(base, target) else {
            return Err(names::NO_SYMLINK);
        };
        Ok(Call::made(Route::Symlink, plain_op(req, &link.path)))
    }
}

/// The temporary symlink in a place (SPEC "symlink の置き場と名前"), for the
/// symlink route of a connect (`posix::symlink_route`) and of `bind_any`
/// (`links::bind_through`), and who can replace an entry in a place.
pub(crate) mod links {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// A place where only this user (and root) can remove or rename the
    /// entries this user creates, by who else can write to it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum PlaceKind {
        /// Owned by this user, without write permission for group and others
        /// (`mode & 0o022 == 0`, as a private `TMPDIR` or `XDG_RUNTIME_DIR`
        /// is): nobody else can create, remove or rename entries there.
        OwnerOnly,
        /// Sticky (`S_ISVTX`) and owned by root or this user, as `/tmp` is:
        /// an entry can be removed or renamed only by its owner, the
        /// directory's owner and root, but anyone can create one, so a name
        /// freed there can be taken by another user.
        Shared,
    }

    /// The kind of a directory of `mode` owned by `owner` for the user `us`,
    /// or `None` when another user can replace an entry `us` creates there
    /// (another user's, or writable by group or others and not sticky, or
    /// sticky but another user's, who can then replace anything). A
    /// directory of `us` that is both counts as [`PlaceKind::OwnerOnly`].
    /// What remains is a replacement by `us` itself or by root, which is
    /// outside what sockat guards against.
    pub(crate) fn place_kind(mode: u32, owner: u32, us: u32) -> Option<PlaceKind> {
        const STICKY: u32 = 0o1000;
        const GROUP_OR_OTHER_WRITE: u32 = 0o022;
        if owner == us && mode & GROUP_OR_OTHER_WRITE == 0 {
            return Some(PlaceKind::OwnerOnly);
        }
        if mode & STICKY != 0 && (owner == us || owner == 0) {
            return Some(PlaceKind::Shared);
        }
        None
    }

    /// [`place_kind`] of `base` followed through symlinks (macOS's `/tmp` is
    /// one to `/private/tmp`), for the effective uid. `None` also when it
    /// cannot be looked up or is not a directory (a file of ours with mode
    /// 0600 passes the mode and owner test, yet is no place for anything).
    pub(crate) fn place_kind_of(base: &Path) -> Option<PlaceKind> {
        let m = std::fs::metadata(base).ok().filter(|m| m.is_dir())?;
        place_kind(m.mode(), m.uid(), sys::effective_uid())
    }

    /// `<base>/sockat.XXXXXXXXXX`: the length of the symlink's spelling
    /// before it is made.
    pub(crate) fn placeholder(base: &Path) -> PathBuf {
        base.join(format!("{}{}", own::PREFIX, "X".repeat(own::RANDOM_LEN)))
    }

    /// The symlink route of `bind_any`, tried once every other bind route is
    /// unusable: a symlink `<base>/sockat.XXXXXXXXXX` to the real path of the
    /// socket's directory, the bind by `<link>/<name>`, and the symlink
    /// removed when the bind returns, whatever the outcome.
    ///
    /// The places of [`Config::bind_link_places`] that only this user can
    /// write to come first, in order, then the shared ones, in order (the
    /// route is [`Route::SymlinkBind`] with `shared` saying which). A place
    /// that is neither, where the spelling would not fit the plain sockaddr,
    /// or where the symlink cannot be made, is passed over with nothing made
    /// in it. [`Step::Next`] (and so `ENAMETOOLONG`) when every place is.
    pub(super) fn bind_through(t: &Target<'_>) -> Step {
        let kinds: Vec<(&Path, PlaceKind)> = t
            .cfg
            .bind_link_places
            .iter()
            .filter_map(|base| place_kind_of(base).map(|k| (base.as_path(), k)))
            .collect();
        let of = |kind| kinds.iter().filter(move |(_, k)| *k == kind);
        let mut why = names::NO_SAFE_BASE;
        for &(base, kind) in of(PlaceKind::OwnerOnly).chain(of(PlaceKind::Shared)) {
            if path::len(&placeholder(base).join(t.name)) > DIRECT_MAX {
                why = names::TOO_LONG;
                continue;
            }
            let Ok(link) = TempLink::create(base, t.real_dir()) else {
                why = names::NO_SYMLINK;
                continue;
            };
            let route = Route::SymlinkBind {
                shared: kind == PlaceKind::Shared,
            };
            let via = link.path.join(t.name);
            return Step::Done(Call::made(route, plain_op(t.req, &via)));
        }
        t.skip(names::SYMLINK, why);
        Step::Next
    }

    /// A symlink under `base` with a random name, removed on drop.
    pub(crate) struct TempLink {
        pub(crate) path: PathBuf,
    }

    impl TempLink {
        /// Create `<base>/sockat.XXXXXXXXXX` pointing to `target`, with a
        /// fresh random name for each try.
        pub(crate) fn create(base: &Path, target: &Path) -> io::Result<Self> {
            Self::create_named(base, target, || own::named(own::PREFIX))
        }

        /// Create a symlink under `base` pointing to `target`, named by
        /// `next_name` ([`own::create_unique`]: `symlink(2)` fails with
        /// `EEXIST` instead of replacing an existing entry, which is the
        /// `O_EXCL` of symlinks).
        pub(crate) fn create_named(
            base: &Path,
            target: &Path,
            next_name: impl FnMut() -> io::Result<String>,
        ) -> io::Result<Self> {
            own::create_unique(
                |name| std::os::unix::fs::symlink(target, base.join(name)),
                next_name,
            )
            .map(|name| Self {
                path: base.join(name),
            })
        }
    }

    impl Drop for TempLink {
        fn drop(&mut self) {
            // Best effort: the call is made by then, and failing it now would
            // not bring the symlink back either. In a sticky /tmp or a
            // private place nobody else can have replaced the entry.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
