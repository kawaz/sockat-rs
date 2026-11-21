//! The order in which the ways of reaching a long socket path are tried (hub
//! SPEC "長いパスの経路と試す順番", DR-0017). [`run`] and [`resolve`] are the
//! only entry points the public API uses; the pieces are `pub(crate)` so the
//! tests can force each one.
//!
//! A path longer than [`DIRECT_MAX`] goes:
//!
//! * **bind**: link-dir (`B/D/E/<name>`, B spelled `/proc/<pid>/fd/<B's fd>`
//!   first, then by its real path), link-file (`B/D/E`, where a bind follows
//!   a symlink in the last component), then thread-cwd (Linux, Apple) or at
//!   (FreeBSD) with `E/<name>` relative to D, or `ENAMETOOLONG`;
//! * **connect**: the routes that need neither writing nor a thread
//!   (proc-direct or relative on Linux, relative and widened on Apple,
//!   at-direct on FreeBSD, relative elsewhere), thread-cwd-direct (Linux,
//!   Apple), then link-file through an E made directly in B and removed right
//!   after the connect, or `ENAMETOOLONG`.
//!
//! The E of link-dir and link-file stays until it is cleaned up (the kernel
//! keeps the bind's spelling for the life of the socket); D and B stay.

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::base::{self, Automatic, Base, Dir, Link, Place, TempLink};
use crate::path;
use crate::sys::{self, Op, SockAddr, SockType};
use crate::{Options, Spelling, Until};

/// The longest path std accepts, `sun_path` less the NUL: 107 on Linux,
/// Android and illumos, 103 on Apple and the BSDs. A path of at most this many
/// bytes is handed to std as is, so std alone decides what happens to every
/// path it can take. It is also the limit of every spelling sockat makes.
pub(crate) const DIRECT_MAX: usize = sys::SUN_PATH_CAP - 1;

/// The names of the routes in the SPEC ("経路の名前"), and the reasons a
/// route is passed over ([`Skipped`]). Diagnostics: they promise nothing.
// Design rationale: the whole table of the SPEC, every target's names and
// reasons in one place, rather than each behind the cfg of the routes that
// use it; a target uses only some of them.
#[allow(dead_code)]
pub(crate) mod names {
    pub(crate) const STANDARD: &str = "standard";
    pub(crate) const LINK_DIR: &str = "link-dir";
    pub(crate) const LINK_FILE: &str = "link-file";
    pub(crate) const THREAD_CWD: &str = "thread-cwd";
    pub(crate) const AT: &str = "at";
    pub(crate) const PROC_DIRECT: &str = "proc-direct";
    pub(crate) const RELATIVE: &str = "relative";
    pub(crate) const WIDENED: &str = "widened";
    pub(crate) const AT_DIRECT: &str = "at-direct";
    pub(crate) const THREAD_CWD_DIRECT: &str = "thread-cwd-direct";

    /// The spelling would not fit the sockaddr.
    pub(crate) const TOO_LONG: &str = "spelling too long";
    /// No B could be made or found.
    pub(crate) const NO_BASE: &str = "no base";
    /// `usable_cross_uid`: no place lets another uid search the way to B.
    pub(crate) const NO_CROSS_UID_BASE: &str = "no base usable across uids";
    /// D could not be made in B.
    pub(crate) const NO_D: &str = "D not made";
    /// E could not be made.
    pub(crate) const NO_E: &str = "E not made";
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
    /// The socket's directory cannot be opened.
    pub(crate) const SOCKDIR_UNOPENABLE: &str = "socket directory unopenable";
    /// The cwd cannot be read (the path is relative).
    pub(crate) const CWD_UNREADABLE: &str = "cwd unreadable";
    /// No relative spelling fits, or the cwd turned out to move.
    pub(crate) const NO_RELATIVE: &str = "no relative spelling";
    /// The kernel does not take the widened sockaddr.
    pub(crate) const WIDENED_REFUSED: &str = "kernel refused widened";
    /// The bind through link-file failed and nothing is at the path: the
    /// kernel did not follow the symlink.
    pub(crate) const NOT_FOLLOWED: &str = "bind did not follow the symlink";
}

/// A route that was tried and could not be used, with why: one of the routes
/// passed over before the one that reached the path (`skipped` of
/// `Bound`, `Connected`, `Report`, `ForBind`, `ForConnect` and `ExError`), in
/// the order tried, one per route name. Only for diagnostics: routes may be
/// added, renamed or changed, and so may the reasons, so do not branch on
/// them.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Skipped {
    /// The route's name in the SPEC (for example `"thread-cwd"`).
    pub name: &'static str,
    /// Why it could not be used (for example `"seccomp filter"`).
    pub reason: &'static str,
}

/// What a call does.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Request<'a> {
    /// Bind a new socket of this type (and listen, for a stream).
    Bind(SockType),
    /// Connect.
    Connect(Connect<'a>),
}

/// What a connect connects.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Connect<'a> {
    /// A new stream socket, `O_NONBLOCK` before the connect when asked.
    ///
    /// A Unix-domain connect completes or fails within the call even on a
    /// nonblocking socket: neither Linux nor XNU answers `EINPROGRESS` for it
    /// (`net/unix/af_unix.c`; XNU's `uipc_usrreq.c` marks the socket connected
    /// before `connect(2)` looks for a connect in progress). Should a kernel
    /// ever answer it, it is returned as the error it is and the socket is
    /// closed, rather than handing back a stream not connected yet.
    Stream {
        /// `connect_nonblocking`: a full backlog fails at once (`EAGAIN` on
        /// Linux, `ECONNREFUSED` on macOS) instead of waiting.
        nonblocking: bool,
    },
    /// The caller's datagram socket.
    Datagram(&'a UnixDatagram),
}

impl Request<'_> {
    fn op(self) -> Op {
        match self {
            Request::Bind(_) => Op::Bind,
            Request::Connect(_) => Op::Connect,
        }
    }

    fn is_bind(self) -> bool {
        matches!(self, Request::Bind(_))
    }
}

/// Whether a bind follows a symlink in the last component of the path (hub
/// SPEC-routes "link-file"): the bind's link-file is in the order only where
/// it may, and `resolve_for_bind` uses it only where it was measured to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Follows {
    /// Measured to follow (macOS, NetBSD): a failed bind is the failure.
    Yes,
    /// Measured not to (Linux / Android, FreeBSD, OpenBSD, illumos): link-file
    /// is not in the bind's order.
    No,
    /// Not measured: told at run time by what is at the path after a failed
    /// bind.
    Unknown,
}

/// Whether a bind follows a symlink on this target.
#[cfg(any(target_vendor = "apple", target_os = "netbsd"))]
const FOLLOWS: Follows = Follows::Yes;
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "illumos"
))]
const FOLLOWS: Follows = Follows::No;
#[cfg(not(any(
    target_vendor = "apple",
    target_os = "netbsd",
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "illumos"
)))]
const FOLLOWS: Follows = Follows::Unknown;

/// What the routes read from the environment, made explicit so the tests can
/// vary it without touching the process environment.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// The places 1-3 for B, in order ([`base::places_from`]).
    pub(crate) places: Vec<(Place, PathBuf)>,
    /// The B of the places 1-3 held: this process's ([`base::Held::process`])
    /// outside the tests, which hold their own.
    pub(crate) held: Arc<base::Held>,
    /// Where `/proc` is: `/proc` outside the tests, which point it elsewhere
    /// to stand in for a system without it.
    pub(crate) proc: PathBuf,
    /// Whether to try the thread-cwd means at all (Linux, Apple). Always
    /// `true` outside the tests, which turn it off to stand in for a system
    /// where it cannot be used.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    pub(crate) thread_cwd: bool,
    /// Whether to try the widened `sockaddr_un` for a connect (Apple).
    /// Always `true` outside the tests.
    #[cfg(target_vendor = "apple")]
    pub(crate) wide: bool,
    /// Whether a bind follows a symlink here ([`FOLLOWS`] outside the tests).
    pub(crate) follows: Follows,
    /// How the socket's directory is opened once a route needs its fd:
    /// [`open_socket_dir`] outside the tests, which put a failing one in its
    /// place.
    #[cfg_attr(
        not(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd"
        )),
        allow(dead_code)
    )]
    pub(crate) open_socket_dir: fn(&Path) -> io::Result<OwnedFd>,
    /// Called once where a relative path has been resolved against the cwd
    /// and is yet to be again. Does nothing outside the tests, which change
    /// the cwd there, as another thread could.
    pub(crate) between_cwd_lookups: fn(),
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
    pub(crate) fn from_vars(tmpdir: Option<OsString>, runtime_dir: Option<OsString>) -> Self {
        Self {
            places: base::places_from(tmpdir, runtime_dir),
            held: base::Held::process(),
            proc: PathBuf::from("/proc"),
            #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
            thread_cwd: true,
            #[cfg(target_vendor = "apple")]
            wide: true,
            follows: FOLLOWS,
            open_socket_dir,
            between_cwd_lookups: || {},
        }
    }
}

/// The socket file a bind made: its real path and `(st_dev, st_ino)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Remembered {
    pub(crate) path: PathBuf,
    pub(crate) id: (u64, u64),
}

/// What a call reached the path by.
pub(crate) struct Reached {
    /// The new socket (`None` for a connect of the caller's datagram socket).
    pub(crate) socket: Option<OwnedFd>,
    pub(crate) route: &'static str,
    pub(crate) base: Option<&'static str>,
    /// What the bind's spelling is (`None` for a connect).
    pub(crate) spelling: Option<Spelling>,
    /// The E the bind's spelling goes through (link-dir, link-file), kept.
    pub(crate) link: Option<Arc<Link>>,
    /// The socket file, when asked for (`bind_ex`).
    pub(crate) file: Option<Remembered>,
    pub(crate) skipped: Vec<Skipped>,
}

/// Why a call did not reach the path.
#[derive(Debug)]
pub(crate) struct Failed {
    pub(crate) error: io::Error,
    /// The route that made the call and failed for real (`None` when no
    /// route could be used, the path was refused before any, or B at
    /// `Options.base` could not be had).
    pub(crate) route: Option<&'static str>,
    pub(crate) skipped: Vec<Skipped>,
}

impl Failed {
    fn before_routes(error: io::Error) -> Self {
        Self {
            error,
            route: None,
            skipped: Vec::new(),
        }
    }
}

/// What a route that made the call came to.
struct Hit {
    socket: Option<OwnedFd>,
    base: Option<&'static str>,
    spelling: Option<Spelling>,
    link: Option<Arc<Link>>,
    file: Option<Remembered>,
}

impl Hit {
    fn socket(socket: Option<OwnedFd>) -> Self {
        Self {
            socket,
            base: None,
            spelling: None,
            link: None,
            file: None,
        }
    }
}

/// Where a route stands after trying.
enum Step {
    /// The route made the call (or failed for real, or B at `Options.base`
    /// could not be had: `route` `None`); this is the result.
    Done {
        route: Option<&'static str>,
        result: io::Result<Hit>,
    },
    /// The route could not be used (noted in the target); nothing it made is
    /// left. Try the next.
    Next,
}

impl Step {
    fn done(route: &'static str, result: io::Result<Hit>) -> Self {
        Step::Done {
            route: Some(route),
            result,
        }
    }

    fn base_error(e: io::Error) -> Self {
        Step::Done {
            route: None,
            result: Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// entry points
// ---------------------------------------------------------------------------

/// Bind or connect `path` by the first route that can be used.
///
/// A path of at most [`DIRECT_MAX`] bytes goes to std as is, before anything
/// else is looked at (the environment included). A NUL byte is looked at
/// before the length: a path with one is `InvalidInput` at any length, except
/// a leading NUL on Linux / Android (a name in the abstract namespace, which
/// goes to std as is).
///
/// With `remember` (`bind_ex`), the socket file is looked up right after the
/// bind ([`Remembered`]). A relative path is resolved against the cwd both by
/// the bind and by that look-up, at different moments, so the cwd's
/// `(st_dev, st_ino)` is taken before both and after both; when they differ
/// the listener is closed and an error returned with the route's name (the
/// socket file stays), so `remove()` cannot hit an unrelated file. A change
/// undone before the second look is not seen.
pub(crate) fn run(
    req: Request<'_>,
    path: &Path,
    options: &Options,
    remember: bool,
    cfg: Option<&Config>,
) -> Result<Reached, Failed> {
    if !remember || path.is_absolute() || path::is_abstract(path) {
        return run_unchecked(req, path, options, remember, cfg);
    }
    let before = dir_id(Path::new("."));
    let reached = run_unchecked(req, path, options, remember, cfg)?;
    if before.is_some() && dir_id(Path::new(".")) == before {
        return Ok(reached);
    }
    if let Some(link) = &reached.link {
        let _ = link.cleanup();
    }
    Err(Failed {
        error: io::Error::other(
            "the current directory changed while binding a path relative to it; \
             the socket was closed and the socket file left in place",
        ),
        route: Some(reached.route),
        skipped: reached.skipped,
    })
}

/// `(st_dev, st_ino)` of the directory `dir`, following a symlink.
fn dir_id(dir: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(dir).ok()?;
    Some((m.dev(), m.ino()))
}

/// The spelling of a path handed to std as is: `CwdRelative` for a relative
/// path (not a name in the abstract namespace), `Original` otherwise.
fn standard_spelling(path: &Path) -> Spelling {
    if path.is_absolute() || path::is_abstract(path) {
        Spelling::Original
    } else {
        Spelling::CwdRelative
    }
}

/// [`run`] without the check of the cwd.
fn run_unchecked(
    req: Request<'_>,
    path: &Path,
    options: &Options,
    remember: bool,
    cfg: Option<&Config>,
) -> Result<Reached, Failed> {
    let direct = |req: Request<'_>, path: &Path| {
        let failed = |error| Failed {
            error,
            route: Some(names::STANDARD),
            skipped: Vec::new(),
        };
        let socket = plain(req, path).map_err(failed)?;
        if let Some(cfg) = cfg {
            (cfg.between_cwd_lookups)();
        }
        if req.is_bind() {
            after_bind();
        }
        let file = match remember && req.is_bind() {
            true => Some(remembered_at(path).map_err(failed)?),
            false => None,
        };
        Ok(Reached {
            socket,
            route: names::STANDARD,
            base: None,
            spelling: req.is_bind().then(|| standard_spelling(path)),
            link: None,
            file,
            skipped: Vec::new(),
        })
    };
    refuse_bad_base(options)?;
    if path::is_abstract(path) {
        return direct(req, path);
    }
    path::refuse_nul(path).map_err(Failed::before_routes)?;
    if path::len(path) <= DIRECT_MAX {
        return direct(req, path);
    }
    let from_env;
    let cfg = match cfg {
        Some(cfg) => cfg,
        None => {
            from_env = Config::from_env();
            &from_env
        }
    };
    let t = Target::new(req, path, options, cfg)?;
    let step = match req {
        Request::Bind(_) => bind_order(&t, remember),
        Request::Connect(_) => connect_order(&t),
    };
    let skipped = t.skipped.into_inner();
    match step {
        Step::Done {
            route,
            result: Ok(hit),
        } => Ok(Reached {
            socket: hit.socket,
            route: route.unwrap_or(names::STANDARD),
            base: hit.base,
            spelling: hit.spelling,
            link: hit.link,
            file: hit.file,
            skipped,
        }),
        Step::Done {
            route,
            result: Err(error),
        } => Err(Failed {
            error,
            route,
            skipped,
        }),
        Step::Next => Err(Failed {
            error: too_long(),
            route: None,
            skipped,
        }),
    }
}

/// `InvalidInput` for an `Options.base` that is relative or does not end with
/// a name, whatever the length of the path (hub SPEC "エラー": an argument of
/// the wrong form, like a NUL in the path), before anything else.
fn refuse_bad_base(options: &Options) -> Result<(), Failed> {
    match &options.base {
        Some(b) => base::split_given(b)
            .map(drop)
            .map_err(Failed::before_routes),
        None => Ok(()),
    }
}

/// `ENAMETOOLONG`: no route reaches the path.
fn too_long() -> io::Error {
    io::Error::from_raw_os_error(libc::ENAMETOOLONG)
}

#[cfg(test)]
thread_local! {
    /// Test-only: called on the calling thread right after a bind succeeds,
    /// before the socket file is looked up ([`after_bind`]). The tests put a
    /// replacement of the file there, as another process could.
    // The initializer is `const`; clippy on illumos says otherwise of the
    // macro's expansion there.
    #[allow(clippy::missing_const_for_thread_local)]
    pub(crate) static AFTER_BIND: Cell<Option<fn()>> = const { Cell::new(None) };
}

/// Where a bind has succeeded and the socket file is yet to be looked up.
/// Does nothing outside the tests ([`AFTER_BIND`]).
fn after_bind() {
    #[cfg(test)]
    if let Some(f) = AFTER_BIND.get() {
        f();
    }
}

/// The [`Remembered`] of the socket file a bind reached by `path` just now:
/// `realpath(3)` of `path` and `lstat(2)` of that, which must be a socket
/// (`ENOTSOCK` otherwise). A symlink in the last component is followed, as
/// the bind followed it on macOS and NetBSD (hub SPEC "remove"): what is
/// remembered is the socket it led to, never the user's symlink. Anything but
/// a socket there is not what the bind made (it was replaced), and is not
/// remembered, so `remove` cannot remove it.
fn remembered_at(path: &Path) -> io::Result<Remembered> {
    let path = std::fs::canonicalize(path)?;
    let m = std::fs::symlink_metadata(&path)?;
    if !std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()) {
        return Err(io::Error::from_raw_os_error(libc::ENOTSOCK));
    }
    Ok(Remembered {
        path,
        id: (m.dev(), m.ino()),
    })
}

// ---------------------------------------------------------------------------
// the target of one long path
// ---------------------------------------------------------------------------

/// One long path being bound / connected / resolved, with what the routes
/// have found out about it so far.
struct Target<'a> {
    req: Request<'a>,
    /// The path as given (relative, widened and link-file spell it).
    path: &'a Path,
    dir: &'a Path,
    name: &'a OsStr,
    /// The real path of the socket's directory, what the E of link-dir,
    /// thread-cwd and at point to.
    real_dir: PathBuf,
    options: &'a Options,
    cfg: &'a Config,
    /// The socket's directory opened by `cfg.open_socket_dir`, once a route
    /// needs its fd; `None` when it cannot be opened.
    #[cfg_attr(
        not(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd"
        )),
        allow(dead_code)
    )]
    dir_fd: OnceCell<Option<OwnedFd>>,
    /// Where `/proc/self/fd` is when it can be used, and the number the fd of
    /// `/` got when it was checked ([`linux::probe`]).
    #[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
    proc_self: OnceCell<Option<(PathBuf, RawFd)>>,
    /// Whether a connect may still use the widened `sockaddr_un` (Apple).
    #[cfg_attr(not(target_vendor = "apple"), allow(dead_code))]
    wide: Cell<bool>,
    /// The Bs (by `(st_dev, st_ino)`) in which D could not be made during
    /// this call: the routes that use D cannot be used for the rest of it.
    no_d: RefCell<Vec<(u64, u64)>>,
    skipped: RefCell<Vec<Skipped>>,
}

impl<'a> Target<'a> {
    /// The socket's directory is looked up first (`stat`), so a missing
    /// directory is `ENOENT` and a file in its place `ENOTDIR` whatever the
    /// file name, as a bind / connect through the path would answer. Its real
    /// path is taken next; when it cannot be resolved that error is returned
    /// as is. A failure up to there is one before any route, with nothing
    /// passed over: the length is judged only once the directory is looked
    /// up, so `standard` is passed over only from then on.
    ///
    /// `Options.base` is checked next, before any route, for a bind and a
    /// connect alike, without making anything ([`base::check_given`]); B is
    /// made only once a route that uses it is reached.
    fn new(
        req: Request<'a>,
        path: &'a Path,
        options: &'a Options,
        cfg: &'a Config,
    ) -> Result<Self, Failed> {
        let before = Failed::before_routes;
        let (dir, name) = path::split(path).map_err(before)?;
        if !std::fs::metadata(dir).map_err(before)?.is_dir() {
            return Err(before(io::Error::from_raw_os_error(libc::ENOTDIR)));
        }
        let real_dir = std::fs::canonicalize(dir).map_err(before)?;
        if let Some(given) = &options.base {
            let cross = matches!(req, Request::Bind(_)) && options.usable_cross_uid;
            base::check_given(given, cross).map_err(before)?;
        }
        (cfg.between_cwd_lookups)();
        Ok(Self {
            req,
            path,
            dir,
            name,
            real_dir,
            options,
            cfg,
            dir_fd: OnceCell::new(),
            proc_self: OnceCell::new(),
            wide: Cell::new(wide_allowed(cfg)),
            no_d: RefCell::new(Vec::new()),
            skipped: RefCell::new(vec![Skipped {
                name: names::STANDARD,
                reason: names::TOO_LONG,
            }]),
        })
    }

    /// Note that the route `name` was tried and cannot be used, for `reason`:
    /// one entry per route name, with the last reason (link-dir tried with
    /// procB and the real path is one entry).
    fn skip(&self, name: &'static str, reason: &'static str) {
        let mut skipped = self.skipped.borrow_mut();
        match skipped.iter_mut().find(|s| s.name == name) {
            Some(s) => s.reason = reason,
            None => skipped.push(Skipped { name, reason }),
        }
    }

    /// This process's D in `b`; `None` when it cannot be made, and from then
    /// on for the rest of the call, without trying again (hub SPEC-routes
    /// "実体の置き場": the routes that use D cannot be used in this call, and
    /// no other B is looked for).
    fn dir(&self, b: &Base) -> Option<Arc<Dir>> {
        if self.no_d.borrow().contains(&b.id) {
            return None;
        }
        let d = b.dir().ok();
        if d.is_none() {
            self.no_d.borrow_mut().push(b.id);
        }
        d
    }

    /// Whether the bind asks for a spelling another uid can follow (the
    /// connects never look at it: a connect's spelling is kept nowhere).
    fn cross_uid(&self) -> bool {
        self.req.is_bind() && self.options.usable_cross_uid
    }

    /// The socket's directory's fd, opened the first time a route asks for
    /// it; `None` when it cannot be opened.
    #[cfg_attr(
        not(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple",
            target_os = "freebsd"
        )),
        allow(dead_code)
    )]
    fn dir_fd(&self) -> Option<BorrowedFd<'_>> {
        self.dir_fd
            .get_or_init(|| (self.cfg.open_socket_dir)(self.dir).ok())
            .as_ref()
            .map(|fd| fd.as_fd())
    }

    /// Where `/proc/self/fd` is when it can be used (Linux).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn proc_self(&self) -> Option<&Path> {
        self.proc_self
            .get_or_init(|| {
                let p = self.cfg.proc.join("self").join("fd");
                linux::probe(&p).map(|fd| (p, fd))
            })
            .as_ref()
            .map(|(p, _)| p.as_path())
    }

    /// `path` made absolute against the cwd: what link-file points to.
    fn absolute(&self) -> Option<PathBuf> {
        if self.path.is_absolute() {
            return Some(self.path.to_path_buf());
        }
        std::env::current_dir().ok().map(|cwd| cwd.join(self.path))
    }
}

#[cfg(target_vendor = "apple")]
fn wide_allowed(cfg: &Config) -> bool {
    cfg.wide
}

#[cfg(not(target_vendor = "apple"))]
fn wide_allowed(_cfg: &Config) -> bool {
    false
}

// ---------------------------------------------------------------------------
// B and the spellings through it
// ---------------------------------------------------------------------------

/// The length of `/proc/<pid>/fd/<fd>` before B is open: the fd is taken to
/// get a number of as many digits as the lowest free one now plus one.
fn proc_prefix_estimate(cfg: &Config) -> usize {
    let fd_digits = std::fs::File::open("/")
        .map(|f| (f.as_raw_fd() + 1).to_string().len())
        .unwrap_or(2);
    path::len(&cfg.proc) + 1 + sys::process_id().to_string().len() + "/fd/".len() + fd_digits
}

/// `<prefix>/<D>/<E>/<name>` (link-dir) or `<prefix>/<D>/<E>` (link-file)
/// fits the sockaddr, with an E name of `e_len` bytes.
fn fits_through(prefix_len: usize, e_len: usize, name: Option<&OsStr>) -> bool {
    let mut len = prefix_len + 1 + base::RANDOM_LEN + 1 + e_len;
    if let Some(name) = name {
        len += 1 + name.len();
    }
    len <= DIRECT_MAX
}

/// The forms of B's spelling to try for `b`, in order (hub SPEC-routes "綴り
/// の形"): `/proc/<pid>/fd/<B's fd>` when `/proc` can be used and another uid
/// need not follow it, then B's real path (not for a B in the socket's
/// directory, whose real path is longer than the path itself: it is spelled
/// only through procB). Each with the diagnostic `base`, how long it is
/// followed, and whether another uid can follow it.
/// The last item is whether a peer with a `/tmp` of its own follows it: always
/// for procB (its spelling goes through no `/tmp`), for B's real path when
/// neither the place as given nor the real path goes through `/tmp` or
/// `/var/tmp`.
fn forms(
    cfg: &Config,
    b: &Base,
    cross_uid: bool,
) -> Vec<(PathBuf, &'static str, Until, bool, bool)> {
    let mut out = Vec::new();
    if !cross_uid {
        if let Some(p) = base::proc_spelling(&cfg.proc, b) {
            out.push((p, b.place.proc_name(), Until::Process, false, true));
        }
    }
    if b.place == Place::SocketDir {
        return out;
    }
    out.push((
        b.path.clone(),
        b.place.real_name(),
        Until::Cleanup,
        b.others_search(cross_uid),
        !b.through_tmp,
    ));
    out
}

/// The B for the spellings through B (link-dir, link-file, resolve): at
/// `Options.base` when given (an error there is the result), else the
/// process's B of the places 1-3 ([`base::automatic`]), else (not for another
/// uid) one in the socket's directory when `/proc` can spell it. `link_file`
/// says whether a link-file spelling may be tried after link-dir. Without a
/// B, why: no spelling would fit through the process's B (nothing is made
/// then), or none could be had.
fn spelled_base(
    t: &Target<'_>,
    link_file: bool,
) -> Result<Result<Arc<Base>, &'static str>, io::Error> {
    let cross = t.cross_uid();
    if let Some(given) = &t.options.base {
        return base::given(given, cross).map(Ok);
    }
    let proc_len = proc_prefix_estimate(t.cfg);
    // Where /proc cannot be used at all, no B can be spelled by it, so a
    // spelling that only procB would fit is known not to fit.
    let proc_usable = std::fs::metadata(t.cfg.proc.join("self").join("fd")).is_ok();
    let by_proc = !cross && proc_usable && fits_through(proc_len, 1, Some(t.name));
    let fits = |b: &Path| {
        let real = path::len(b);
        fits_through(real, 1, Some(t.name)) || by_proc || (link_file && fits_through(real, 1, None))
    };
    let none = if cross {
        names::NO_CROSS_UID_BASE
    } else {
        names::NO_BASE
    };
    match base::automatic(&t.cfg.places, &t.cfg.held, cross, fits) {
        Automatic::Base(b) => Ok(Ok(b)),
        Automatic::TooLong => Ok(Err(names::TOO_LONG)),
        Automatic::Missing if !cross && by_proc => Ok(base::in_socket_dir(&t.real_dir).ok_or(none)),
        Automatic::Missing => Ok(Err(none)),
    }
}

/// What a spelling through B and D is: a counted E in D, and the spelling
/// `<B's form>/<D>/<E>` with `<name>` after it for link-dir.
struct Through {
    spelling: PathBuf,
    link: Arc<Link>,
    base: &'static str,
    kind: Spelling,
}

/// Make E for link-dir (pointing to the socket's directory's real path,
/// `link_file` `None`) or link-file (pointing to `link_file`, the socket
/// file) in this process's D in `b`, by the first form of B that fits.
/// `Err` with why when none can be used (noted by the caller).
fn through(t: &Target<'_>, b: &Base, link_file: Option<&Path>) -> Result<Through, &'static str> {
    let name = match link_file {
        None => Some(t.name),
        Some(_) => None,
    };
    let forms = forms(t.cfg, b, t.cross_uid());
    let mut why = match forms.is_empty() {
        true => names::PROC_UNUSABLE,
        false => names::TOO_LONG,
    };
    for (prefix, diag, until, others, private_tmp) in forms {
        if !fits_through(path::len(&prefix), 1, name) {
            why = names::TOO_LONG;
            continue;
        }
        let d = t.dir(b).ok_or(names::NO_D)?;
        if !fits_through(path::len(&prefix), d.next_e_len(), name) {
            why = names::TOO_LONG;
            continue;
        }
        let link = d
            .link(link_file.unwrap_or(&t.real_dir))
            .map_err(|_| names::NO_E)?;
        let mut spelling = prefix.join(link.below_b());
        if let Some(name) = name {
            spelling.push(name);
        }
        if path::len(&spelling) > DIRECT_MAX {
            let _ = link.cleanup();
            continue;
        }
        return Ok(Through {
            spelling,
            link: Arc::new(link),
            base: diag,
            kind: Spelling::Linked {
                removable: link_file.is_none(),
                until,
                usable_cross_uid: others,
                usable_with_private_tmp: private_tmp,
            },
        });
    }
    Err(why)
}

// ---------------------------------------------------------------------------
// bind
// ---------------------------------------------------------------------------

/// The routes of a bind (hub SPEC "bind の順"): link-dir, link-file where a
/// bind may follow a symlink, thread-cwd (Linux, Apple) or at (FreeBSD).
/// [`Step::Next`] (and so `ENAMETOOLONG`) when none could be used.
fn bind_order(t: &Target<'_>, remember: bool) -> Step {
    let link_file = t.cfg.follows != Follows::No;
    let b = match spelled_base(t, link_file) {
        Ok(b) => b,
        Err(e) => return Step::base_error(e),
    };
    match &b {
        Err(why) => t.skip(names::LINK_DIR, why),
        Ok(b) => match through(t, b, None) {
            Ok(th) => return bind_through(t, th, names::LINK_DIR, remember),
            Err(why) => t.skip(names::LINK_DIR, why),
        },
    }
    if link_file {
        if let Step::Done { route, result } = link_file_bind(t, b.as_deref(), remember) {
            return Step::Done { route, result };
        }
    }
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    return thread_cwd_bind(t, remember);
    #[cfg(target_os = "freebsd")]
    return at_bind(t, remember);
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd"
    )))]
    Step::Next
}

/// Bind by the spelling of `th`. On success the E stays (the kernel keeps the
/// spelling); on failure it is removed before the error is returned (hub
/// SPEC-routes "経路の失敗の扱い"). With `remember`, the socket file is looked
/// up ([`remember_spelled`]) while E is there; when that fails, the socket is
/// closed and E removed. A failure to remove E there is passed over: the
/// error returned is the call's (as for every E removed inside a call).
fn bind_through(t: &Target<'_>, th: Through, route: &'static str, remember: bool) -> Step {
    let socket = match plain(t.req, &th.spelling) {
        Ok(socket) => socket,
        Err(e) => {
            let _ = th.link.cleanup();
            return Step::done(route, Err(e));
        }
    };
    after_bind();
    let file = match remember {
        false => None,
        true => match remember_spelled(t, &th.spelling) {
            Ok(r) => Some(r),
            Err(e) => {
                drop(socket);
                let _ = th.link.cleanup();
                return Step::done(route, Err(e));
            }
        },
    };
    Step::done(
        route,
        Ok(Hit {
            socket,
            base: Some(th.base),
            spelling: Some(th.kind),
            link: Some(th.link),
            file,
        }),
    )
}

/// The socket file bound just now through `spelling`: [`remembered_at`] the
/// real path of the socket's directory joined with the file name (a symlink
/// there followed, as the bind followed it), which the spelling must reach.
fn remember_spelled(t: &Target<'_>, spelling: &Path) -> io::Result<Remembered> {
    let r = remembered_at(&t.real_dir.join(t.name))?;
    let by_spelling = std::fs::metadata(spelling)?;
    if (by_spelling.dev(), by_spelling.ino()) != r.id {
        return Err(io::Error::other(
            "the socket file was replaced right after the bind",
        ));
    }
    Ok(r)
}

/// The bind's link-file: E in D pointing to the socket file (the path made
/// absolute), and the bind by `<B>/<D>/<E>` (hub SPEC-routes "link-file").
///
/// Where a bind is measured to follow the symlink ([`Follows::Yes`]) a failed
/// bind is the failure. Elsewhere ([`Follows::Unknown`]) what is at the path
/// after a failed bind decides: something there makes the failure real
/// (`EADDRINUSE`, ...), returned as it is; nothing there, or nothing that can
/// be told, means the kernel did not follow the symlink, and the route is
/// passed over (a real failure fails the next route the same way and is
/// returned there). E is removed either way.
fn link_file_bind(t: &Target<'_>, b: Result<&Base, &&'static str>, remember: bool) -> Step {
    let b = match b {
        Ok(b) => b,
        Err(why) => {
            t.skip(names::LINK_FILE, why);
            return Step::Next;
        }
    };
    let Some(target) = t.absolute() else {
        t.skip(names::LINK_FILE, names::CWD_UNREADABLE);
        return Step::Next;
    };
    let th = match through(t, b, Some(&target)) {
        Ok(th) => th,
        Err(why) => {
            t.skip(names::LINK_FILE, why);
            return Step::Next;
        }
    };
    let socket = match plain(t.req, &th.spelling) {
        Ok(socket) => socket,
        Err(e) => {
            let _ = th.link.cleanup();
            if t.cfg.follows == Follows::Unknown && std::fs::symlink_metadata(t.path).is_err() {
                t.skip(names::LINK_FILE, names::NOT_FOLLOWED);
                return Step::Next;
            }
            return Step::done(names::LINK_FILE, Err(e));
        }
    };
    after_bind();
    let file = match remember {
        false => None,
        true => match remembered_at(&t.real_dir.join(t.name)) {
            Ok(r) => Some(r),
            Err(e) => {
                drop(socket);
                let _ = th.link.cleanup();
                return Step::done(names::LINK_FILE, Err(e));
            }
        },
    };
    Step::done(
        names::LINK_FILE,
        Ok(Hit {
            socket,
            base: Some(th.base),
            spelling: Some(th.kind),
            link: Some(th.link),
            file,
        }),
    )
}

/// The B whose D thread-cwd and at work in: at `Options.base` when given,
/// else this process's B of the places 1-3, else one in the socket's
/// directory (hub SPEC-routes "thread-cwd と at の手順").
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
))]
fn d_base(t: &Target<'_>) -> Result<Option<Arc<Base>>, io::Error> {
    if let Some(given) = &t.options.base {
        return base::given(given, t.cross_uid()).map(Some);
    }
    // Nothing is spelled through B here, so any B fits (never TooLong).
    Ok(
        match base::automatic(&t.cfg.places, &t.cfg.held, false, |_| true) {
            Automatic::Base(b) => Some(b),
            Automatic::TooLong => None,
            Automatic::Missing => base::in_socket_dir(&t.real_dir),
        },
    )
}

/// What thread-cwd and at do around the call (hub SPEC-routes "thread-cwd と
/// at の手順"): this process's D held by its fd, a random E in it pointing to
/// the socket's directory's real path, the call with `E/<name>` relative to
/// D by `call`, and E removed right after, whatever the outcome. The spelling
/// is `Dangling`.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
))]
fn in_d(
    t: &Target<'_>,
    route: &'static str,
    remember: bool,
    call: impl FnOnce(BorrowedFd<'_>, &SockAddr) -> Option<io::Result<Option<OwnedFd>>>,
) -> Step {
    let rel_len = base::RANDOM_LEN + 1 + t.name.len();
    if rel_len > DIRECT_MAX {
        t.skip(route, names::TOO_LONG);
        return Step::Next;
    }
    let b = match d_base(t) {
        Ok(Some(b)) => b,
        Ok(None) => {
            t.skip(route, names::NO_BASE);
            return Step::Next;
        }
        Err(e) => return Step::base_error(e),
    };
    let Some(d) = t.dir(&b) else {
        t.skip(route, names::NO_D);
        return Step::Next;
    };
    let Ok(e) = TempLink::make(d.fd(), &t.real_dir) else {
        t.skip(route, names::NO_E);
        return Step::Next;
    };
    let rel = Path::new(&e.name).join(t.name);
    let Ok(addr) = SockAddr::standard(rel.as_os_str()) else {
        t.skip(route, names::TOO_LONG);
        return Step::Next;
    };
    let Some(result) = call(d.fd(), &addr) else {
        return Step::Next;
    };
    if result.is_ok() && t.req.is_bind() {
        after_bind();
    }
    let file = match (&result, remember) {
        (Ok(_), true) => Some(remembered_at(&t.real_dir.join(t.name))),
        _ => None,
    };
    drop(e);
    let hit = |socket| Hit {
        socket,
        base: None,
        spelling: Some(Spelling::Dangling),
        link: None,
        file: None,
    };
    match (result, file) {
        (Ok(socket), None) => Step::done(route, Ok(hit(socket))),
        (Ok(socket), Some(Ok(file))) => Step::done(
            route,
            Ok(Hit {
                file: Some(file),
                ..hit(socket)
            }),
        ),
        (Ok(_), Some(Err(e))) | (Err(e), _) => Step::done(route, Err(e)),
    }
}

/// Why the thread-cwd means cannot be used, as far as that can be told before
/// anything is made for it (`None` when it may be).
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn thread_cwd_check(t: &Target<'_>) -> Option<&'static str> {
    if !t.cfg.thread_cwd {
        return Some(names::THREAD_CWD_OFF);
    }
    if !sys::thread_cwd_available() {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return Some(names::SECCOMP);
        #[cfg(target_vendor = "apple")]
        return Some(names::NO_PTHREAD_FCHDIR);
    }
    None
}

/// The bind's thread-cwd: a thread of its own whose working directory alone
/// is D ([`sys::op_in_dir_on_thread`]).
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn thread_cwd_bind(t: &Target<'_>, remember: bool) -> Step {
    if let Some(why) = thread_cwd_check(t) {
        t.skip(names::THREAD_CWD, why);
        return Step::Next;
    }
    in_d(t, names::THREAD_CWD, remember, |d, addr| {
        match on_thread(t.req, d, addr) {
            sys::OnThread::Done(result) => Some(result),
            sys::OnThread::Unusable(_) => {
                t.skip(names::THREAD_CWD, names::THREAD_CWD_REFUSED);
                None
            }
        }
    })
}

/// The bind's at: `bindat(2)` on D's fd (FreeBSD).
#[cfg(target_os = "freebsd")]
fn at_bind(t: &Target<'_>, remember: bool) -> Step {
    in_d(t, names::AT, remember, |d, addr| {
        Some(freebsd::at(t.req, d, addr))
    })
}

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

/// A route of the connect order.
type ConnectRoute = fn(&Target<'_>) -> Step;

/// The routes of a connect, per target (hub SPEC "connect の順"):
///
/// | target | order |
/// |---|---|
/// | Linux, Android (`/proc` usable) | proc-direct, thread-cwd-direct, link-file |
/// | Linux, Android (no `/proc`) | relative, thread-cwd-direct, link-file |
/// | Apple | relative, widened, thread-cwd-direct, link-file |
/// | FreeBSD | at-direct, link-file |
/// | NetBSD, OpenBSD, illumos, other Unix | relative, link-file |
fn connect_order(t: &Target<'_>) -> Step {
    for route in connect_routes(t) {
        if let done @ Step::Done { .. } = route(t) {
            return done;
        }
    }
    Step::Next
}

/// See [`connect_order`].
#[cfg(any(target_os = "linux", target_os = "android"))]
fn connect_routes(t: &Target<'_>) -> &'static [ConnectRoute] {
    match t.proc_self() {
        Some(_) => &[linux::proc_direct, thread_cwd_direct, link_file_connect],
        None => &[posix::relative, thread_cwd_direct, link_file_connect],
    }
}

/// See [`connect_order`].
#[cfg(target_vendor = "apple")]
fn connect_routes(_t: &Target<'_>) -> &'static [ConnectRoute] {
    &[
        posix::relative,
        apple::widened,
        thread_cwd_direct,
        link_file_connect,
    ]
}

/// See [`connect_order`].
#[cfg(target_os = "freebsd")]
fn connect_routes(_t: &Target<'_>) -> &'static [ConnectRoute] {
    &[freebsd::at_direct, link_file_connect]
}

/// See [`connect_order`].
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
)))]
fn connect_routes(_t: &Target<'_>) -> &'static [ConnectRoute] {
    &[posix::relative, link_file_connect]
}

/// The connect's thread-cwd-direct: a thread of its own whose working
/// directory alone is the socket's directory connects by the file name (no
/// D, no E: a connect's spelling is kept nowhere).
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn thread_cwd_direct(t: &Target<'_>) -> Step {
    let skip = |why| {
        t.skip(names::THREAD_CWD_DIRECT, why);
        Step::Next
    };
    if let Some(why) = thread_cwd_check(t) {
        return skip(why);
    }
    let Ok(addr) = SockAddr::standard(t.name) else {
        return skip(names::TOO_LONG);
    };
    let Some(dir) = t.dir_fd() else {
        return skip(names::SOCKDIR_UNOPENABLE);
    };
    match on_thread(t.req, dir, &addr) {
        sys::OnThread::Done(result) => {
            Step::done(names::THREAD_CWD_DIRECT, result.map(Hit::socket))
        }
        sys::OnThread::Unusable(_) => skip(names::THREAD_CWD_REFUSED),
    }
}

/// The connect's link-file (hub SPEC-routes "link-file"): no D; a random E
/// directly in B (at `Options.base`, else this process's B of the places
/// 1-3; never one in the socket's directory, whose real path would be longer
/// than the path) pointing to the socket file, the connect by `<B>/<E>`
/// (B's real path), and E removed right after. Nothing is left and nothing
/// waits.
fn link_file_connect(t: &Target<'_>) -> Step {
    let skip = |why| {
        t.skip(names::LINK_FILE, why);
        Step::Next
    };
    let fits = |b: &Path| path::len(b) + 1 + base::RANDOM_LEN <= DIRECT_MAX;
    let b = match &t.options.base {
        Some(given) => match base::given(given, false) {
            Ok(b) => b,
            Err(e) => return Step::base_error(e),
        },
        None => match base::automatic(&t.cfg.places, &t.cfg.held, false, fits) {
            Automatic::Base(b) => b,
            Automatic::TooLong => return skip(names::TOO_LONG),
            Automatic::Missing => return skip(names::NO_BASE),
        },
    };
    if !fits(&b.path) {
        return skip(names::TOO_LONG);
    }
    let Some(target) = t.absolute() else {
        return skip(names::CWD_UNREADABLE);
    };
    let Ok(e) = TempLink::make(b.fd(), &target) else {
        return skip(names::NO_E);
    };
    let spelling = b.path.join(&e.name);
    let result = plain(t.req, &spelling);
    drop(e);
    Step::done(names::LINK_FILE, result.map(Hit::socket))
}

// ---------------------------------------------------------------------------
// resolve
// ---------------------------------------------------------------------------

/// What a resolve is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Use {
    Bind,
    Connect,
}

/// What a resolve came to: the spelling and what it goes through.
pub(crate) struct Resolved {
    pub(crate) path: PathBuf,
    pub(crate) spelling: Spelling,
    pub(crate) route: &'static str,
    pub(crate) base: Option<&'static str>,
    pub(crate) skipped: Vec<Skipped>,
    pub(crate) link: Option<Arc<Link>>,
    /// The cwd's `(st_dev, st_ino)` when the spelling is `CwdRelative`.
    pub(crate) cwd: Option<(u64, u64)>,
}

/// Resolve `path` into a spelling to hand to std (hub SPEC "resolve"):
///
/// * a path std accepts: itself (`Original` / `CwdRelative`);
/// * for a bind: link-dir (procB, then B's real path), then link-file where
///   a bind is measured to follow a symlink;
/// * for a connect: link-file through an E in this process's D (procB, then
///   B's real path);
/// * otherwise `ENAMETOOLONG`.
pub(crate) fn resolve(
    for_: Use,
    path: &Path,
    options: &Options,
    cfg: Option<&Config>,
) -> Result<Resolved, Failed> {
    let short = |path: &Path| Resolved {
        path: path.to_path_buf(),
        spelling: standard_spelling(path),
        route: names::STANDARD,
        base: None,
        skipped: Vec::new(),
        link: None,
        cwd: match standard_spelling(path) {
            Spelling::CwdRelative => dir_id(Path::new(".")),
            _ => None,
        },
    };
    refuse_bad_base(options)?;
    if path::is_abstract(path) {
        return Ok(short(path));
    }
    path::refuse_nul(path).map_err(Failed::before_routes)?;
    if path::len(path) <= DIRECT_MAX {
        return Ok(short(path));
    }
    let from_env;
    let cfg = match cfg {
        Some(cfg) => cfg,
        None => {
            from_env = Config::from_env();
            &from_env
        }
    };
    let req = match for_ {
        Use::Bind => Request::Bind(SockType::Stream),
        Use::Connect => Request::Connect(Connect::Stream { nonblocking: false }),
    };
    let t = Target::new(req, path, options, cfg)?;
    let found = match for_ {
        Use::Bind => resolve_bind(&t),
        Use::Connect => resolve_connect(&t),
    };
    let skipped = t.skipped.into_inner();
    match found {
        Ok(Some(th)) => Ok(Resolved {
            path: th.spelling,
            spelling: th.kind,
            route: if matches!(
                th.kind,
                Spelling::Linked {
                    removable: true,
                    ..
                }
            ) {
                names::LINK_DIR
            } else {
                names::LINK_FILE
            },
            base: match for_ {
                Use::Bind => Some(th.base),
                Use::Connect => None,
            },
            skipped,
            link: Some(th.link),
            cwd: None,
        }),
        Ok(None) => Err(Failed {
            error: too_long(),
            route: None,
            skipped,
        }),
        Err(error) => Err(Failed {
            error,
            route: None,
            skipped,
        }),
    }
}

/// `resolve_for_bind`'s routes: link-dir, then link-file where a bind is
/// measured to follow a symlink (resolving does not bind, so it cannot tell
/// at run time).
fn resolve_bind(t: &Target<'_>) -> io::Result<Option<Through>> {
    let link_file = t.cfg.follows == Follows::Yes;
    let b = match spelled_base(t, link_file)? {
        Ok(b) => b,
        Err(why) => {
            t.skip(names::LINK_DIR, why);
            if link_file {
                t.skip(names::LINK_FILE, why);
            }
            return Ok(None);
        }
    };
    match through(t, &b, None) {
        Ok(th) => return Ok(Some(th)),
        Err(why) => t.skip(names::LINK_DIR, why),
    }
    if link_file {
        let Some(target) = t.absolute() else {
            t.skip(names::LINK_FILE, names::CWD_UNREADABLE);
            return Ok(None);
        };
        match through(t, &b, Some(&target)) {
            Ok(th) => return Ok(Some(th)),
            Err(why) => t.skip(names::LINK_FILE, why),
        }
    }
    Ok(None)
}

/// `resolve_for_connect`'s route: link-file through an E in this process's D.
fn resolve_connect(t: &Target<'_>) -> io::Result<Option<Through>> {
    let b = match spelled_base(t, true)? {
        Ok(b) => b,
        Err(why) => {
            t.skip(names::LINK_FILE, why);
            return Ok(None);
        }
    };
    let Some(target) = t.absolute() else {
        t.skip(names::LINK_FILE, names::CWD_UNREADABLE);
        return Ok(None);
    };
    match through(t, &b, Some(&target)) {
        Ok(th) => Ok(Some(th)),
        Err(why) => {
            t.skip(names::LINK_FILE, why);
            Ok(None)
        }
    }
}

/// The socket file a resolved bind spelling reaches now (`ForBind`'s
/// `socket_file()`): [`remembered_at`] the spelling (followed to the end, a
/// symlink in the last component of the path too). For a `CwdRelative`
/// spelling the cwd must still be the one it was resolved against.
pub(crate) fn remember_resolved(
    spelling: &Path,
    cwd: Option<(u64, u64)>,
) -> io::Result<Remembered> {
    if cwd.is_some() && dir_id(Path::new(".")) != cwd {
        return Err(io::Error::other(
            "the current directory changed since the path relative to it was resolved",
        ));
    }
    remembered_at(spelling)
}

// ---------------------------------------------------------------------------
// the calls themselves
// ---------------------------------------------------------------------------

/// Bind (and listen, for a stream) or connect with `path` as is, in the plain
/// sockaddr.
///
/// A bind, a blocking stream connect and a datagram connect go to std
/// unchanged. std cannot make a nonblocking connect (`UnixStream::connect`
/// creates and connects its socket in one call), so that one is made on
/// sockat's own socket, set up as std's ([`sys::socket`]) plus `O_NONBLOCK`,
/// with the same address bytes std would build ([`SockAddr::standard`]).
pub(crate) fn plain(req: Request<'_>, path: &Path) -> io::Result<Option<OwnedFd>> {
    match req {
        Request::Bind(SockType::Stream) => UnixListener::bind(path).map(|l| Some(l.into())),
        Request::Bind(SockType::Datagram) => UnixDatagram::bind(path).map(|s| Some(s.into())),
        Request::Connect(Connect::Stream { nonblocking: false }) => {
            UnixStream::connect(path).map(|s| Some(s.into()))
        }
        Request::Connect(Connect::Stream { nonblocking: true }) => {
            raw(req, &SockAddr::standard(path.as_os_str())?)
        }
        Request::Connect(Connect::Datagram(sock)) => sock.connect(path).map(|()| None),
    }
}

/// The socket of a call: a new one for a bind or a stream connect, the
/// caller's for a datagram connect.
enum Sock<'a> {
    New(OwnedFd),
    Callers(BorrowedFd<'a>),
}

impl<'a> Sock<'a> {
    fn for_request(req: Request<'a>) -> io::Result<Self> {
        Ok(match req {
            Request::Bind(ty) => Sock::New(sys::socket(ty, false)?),
            Request::Connect(Connect::Stream { nonblocking }) => {
                Sock::New(sys::socket(SockType::Stream, nonblocking)?)
            }
            Request::Connect(Connect::Datagram(sock)) => Sock::Callers(sock.as_fd()),
        })
    }

    fn fd(&self) -> BorrowedFd<'_> {
        match self {
            Sock::New(fd) => fd.as_fd(),
            Sock::Callers(fd) => *fd,
        }
    }

    /// The listen after a stream bind, and the new socket to hand back.
    fn finish(self, req: Request<'_>) -> io::Result<Option<OwnedFd>> {
        if matches!(req, Request::Bind(SockType::Stream)) {
            sys::listen(self.fd())?;
        }
        Ok(match self {
            Sock::New(fd) => Some(fd),
            Sock::Callers(_) => None,
        })
    }
}

/// Bind or connect to `addr` on the socket of `req`.
pub(crate) fn raw(req: Request<'_>, addr: &SockAddr) -> io::Result<Option<OwnedFd>> {
    let sock = Sock::for_request(req)?;
    sys::op(req.op(), sock.fd(), addr)?;
    sock.finish(req)
}

/// Bind or connect to `addr` resolved relative to `dir`, on a thread whose
/// own working directory is moved there ([`sys::op_in_dir_on_thread`]).
/// `Unusable` when the thread's working directory cannot be moved on its own.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
pub(crate) fn on_thread(
    req: Request<'_>,
    dir: BorrowedFd<'_>,
    addr: &SockAddr,
) -> sys::OnThread<Option<OwnedFd>> {
    let sock = match Sock::for_request(req) {
        Ok(sock) => sock,
        Err(e) => return sys::OnThread::Done(Err(e)),
    };
    match sys::op_in_dir_on_thread(req.op(), sock.fd(), dir, addr) {
        sys::OnThread::Done(Ok(())) => sys::OnThread::Done(sock.finish(req)),
        sys::OnThread::Done(Err(e)) => sys::OnThread::Done(Err(e)),
        sys::OnThread::Unusable(e) => sys::OnThread::Unusable(e),
    }
}

/// Open the socket's directory to use as the base of `*at` calls, `fchdir` and
/// `/proc/self/fd/<fd>` (a symlink at its last component is followed, as a
/// bind / connect through the path would).
///
/// Apple: `O_SEARCH`, which needs only search permission (a kernel that does
/// not take it on a directory answers `EISDIR` or `EINVAL`, and then
/// `O_RDONLY`). Linux: `O_PATH | O_DIRECTORY`, which needs only search
/// permission. Other targets: `O_RDONLY | O_DIRECTORY`, which needs read
/// permission.
pub(crate) fn open_socket_dir(dir: &Path) -> io::Result<OwnedFd> {
    let open = |flags: libc::c_int| {
        OpenOptions::new()
            .read(true)
            .custom_flags(flags | libc::O_DIRECTORY)
            .open(dir)
            .map(OwnedFd::from)
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    return open(libc::O_PATH);
    #[cfg(target_vendor = "apple")]
    match open(libc::O_SEARCH) {
        Err(e) if matches!(e.raw_os_error(), Some(libc::EISDIR | libc::EINVAL)) => {}
        searched => return searched,
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    open(0)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) mod linux {
    use super::*;

    /// Whether `proc_self_fd` lists the fds of this process: `<it>/<fd>` of
    /// `/`, opened by [`open_socket_dir`], can be looked up. The number that
    /// fd got, or `None` when `/proc` cannot be used.
    pub(crate) fn probe(proc_self_fd: &Path) -> Option<RawFd> {
        let root = open_socket_dir(Path::new("/")).ok()?;
        fd_dir(proc_self_fd, root.as_fd())?;
        Some(root.as_raw_fd())
    }

    /// `<proc_self_fd>/<fd>`, or `None` when it cannot be looked up.
    pub(crate) fn fd_dir(proc_self_fd: &Path, fd: BorrowedFd<'_>) -> Option<PathBuf> {
        let fd_dir = proc_self_fd.join(fd.as_raw_fd().to_string());
        std::fs::metadata(&fd_dir).ok()?;
        Some(fd_dir)
    }

    /// The connect's proc-direct: `/proc/self/fd/<the socket's directory's
    /// fd>/<name>`, in the plain sockaddr. Unusable when `/proc` cannot be
    /// used, the socket's directory cannot be opened, or the spelling does
    /// not fit.
    pub(super) fn proc_direct(t: &Target<'_>) -> Step {
        let skip = |why| {
            t.skip(names::PROC_DIRECT, why);
            Step::Next
        };
        let Some(proc) = t.proc_self() else {
            return skip(names::PROC_UNUSABLE);
        };
        let Some(dir_fd) = t.dir_fd() else {
            return skip(names::SOCKDIR_UNOPENABLE);
        };
        let Some(fd_dir) = fd_dir(proc, dir_fd) else {
            return skip(names::PROC_UNUSABLE);
        };
        let via = fd_dir.join(t.name);
        if path::len(&via) > DIRECT_MAX {
            return skip(names::TOO_LONG);
        }
        Step::done(names::PROC_DIRECT, plain(t.req, &via).map(Hit::socket))
    }
}

#[cfg(target_os = "freebsd")]
pub(crate) mod freebsd {
    use super::*;

    /// The connect's at-direct: `connectat(2)` with the file name on the
    /// socket's directory's fd. Unusable when the file name does not fit or
    /// the socket's directory cannot be opened.
    pub(super) fn at_direct(t: &Target<'_>) -> Step {
        let Ok(addr) = SockAddr::standard(t.name) else {
            t.skip(names::AT_DIRECT, names::TOO_LONG);
            return Step::Next;
        };
        match t.dir_fd() {
            Some(dir_fd) => Step::done(names::AT_DIRECT, at(t.req, dir_fd, &addr).map(Hit::socket)),
            None => {
                t.skip(names::AT_DIRECT, names::SOCKDIR_UNOPENABLE);
                Step::Next
            }
        }
    }

    /// `bindat(2)` / `connectat(2)` on the socket of `req` to `addr` relative
    /// to the directory fd `dir`, then the listen after a stream bind.
    pub(crate) fn at(
        req: Request<'_>,
        dir: BorrowedFd<'_>,
        addr: &SockAddr,
    ) -> io::Result<Option<OwnedFd>> {
        let sock = Sock::for_request(req)?;
        sys::op_at(req.op(), sock.fd(), dir, addr)?;
        sock.finish(req)
    }
}

/// The widened `sockaddr_un` that XNU takes (Apple), for a connect only.
#[cfg(target_vendor = "apple")]
pub(crate) mod apple {
    use super::*;

    /// The connect's widened: the path itself in a widened `sockaddr_un` when
    /// it is at most [`sys::WIDE_PATH_MAX`] bytes, else its spelling relative
    /// to the cwd when that is. Unusable when the kernel refuses the widened
    /// form or neither spelling fits. A bind never uses it: the address of a
    /// socket bound so does not fit `sun_path` and breaks its readers.
    pub(super) fn widened(t: &Target<'_>) -> Step {
        if !t.wide.get() {
            t.skip(names::WIDENED, names::WIDENED_REFUSED);
            return Step::Next;
        }
        let wide = |p: &Path| raw(t.req, &SockAddr::wide(p.as_os_str())?);
        let attempt = if path::len(t.path) <= sys::WIDE_PATH_MAX {
            Some(wide(t.path))
        } else {
            posix::Relative::new(t.path, t.dir)
                .filter(|r| path::len(&r.path) <= sys::WIDE_PATH_MAX)
                .map(|rel| rel.run(wide))
        };
        match attempt {
            None => t.skip(names::WIDENED, names::TOO_LONG),
            Some(Err(e)) if refuses_wide(&e) => {
                t.skip(names::WIDENED, names::WIDENED_REFUSED);
                t.wide.set(false);
            }
            Some(result) => return Step::done(names::WIDENED, result.map(Hit::socket)),
        }
        Step::Next
    }

    /// Whether an error from a widened-address call means the kernel does not
    /// take the widened form (as opposed to a real outcome such as `ENOENT` or
    /// `ECONNREFUSED`). XNU reports an address it will not take with
    /// `EINVAL`, or `ENAMETOOLONG` for one longer than `SOCK_MAXADDRLEN`.
    pub(crate) fn refuses_wide(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENAMETOOLONG))
    }
}

/// The connect's relative: the path spelled relative to the cwd (every target
/// but FreeBSD).
#[cfg(not(target_os = "freebsd"))]
pub(crate) mod posix {
    use super::*;

    #[cfg(target_vendor = "apple")]
    use crate::sys::WIDE_PATH_MAX;

    /// The longest spelling a sockaddr carries for a connect on this target:
    /// the widened form's 253 bytes on Apple, [`DIRECT_MAX`] elsewhere.
    #[cfg(target_vendor = "apple")]
    const SPELLING_MAX: usize = WIDE_PATH_MAX;
    #[cfg(not(target_vendor = "apple"))]
    const SPELLING_MAX: usize = DIRECT_MAX;

    /// The relative route: the path spelled relative to the cwd when that
    /// fits the plain sockaddr ([`Relative`]).
    pub(super) fn relative(t: &Target<'_>) -> Step {
        match Relative::new(t.path, t.dir).filter(|r| path::len(&r.path) <= DIRECT_MAX) {
            Some(rel) => Step::done(
                names::RELATIVE,
                rel.run(|p| plain(t.req, p)).map(Hit::socket),
            ),
            None => {
                t.skip(names::RELATIVE, names::NO_RELATIVE);
                Step::Next
            }
        }
    }

    /// A relative spelling of an absolute socket path, checked to name the
    /// same directory as the absolute one.
    pub(crate) struct Relative {
        pub(crate) path: PathBuf,
        /// `(st_dev, st_ino)` of the cwd when the spelling was made.
        cwd: (u64, u64),
    }

    /// The spellings of the cwd that the path may start with: what
    /// `getcwd(3)` reports (a real path), and `$PWD` (the path the shell used
    /// to get there, possibly through symlinks) but only when it names the
    /// cwd: `$PWD` is stale after a `chdir(2)` that did not update it, and
    /// anyone can set it.
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
        /// Spell `path` (whose directory part is `dir`) relative to the cwd
        /// (hub SPEC-routes "相対パスの作り方"):
        ///
        /// 1. **Below the cwd.** When `path` starts, component by component,
        ///    with a spelling of the cwd, cut that prefix off. The rest only
        ///    descends from the cwd along `path` itself, so it resolves the
        ///    same way `path` does.
        /// 2. **Through real paths.** Only when 1 gives nothing of at most
        ///    [`DIRECT_MAX`] bytes: resolve `dir` with `realpath(3)` and climb
        ///    out of the cwd as `getcwd(3)` reports it (both real, so every
        ///    `..` climbs out of a real directory, as the kernel does).
        ///
        /// The shorter result is kept. `None` when `path` is not absolute,
        /// the cwd cannot be read, neither way gives a spelling a sockaddr of
        /// this target carries, or the relative directory turns out not to be
        /// the absolute one (a cwd removed, or changed meanwhile by another
        /// thread).
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
        /// the one the spelling was made against. If another thread changed
        /// it in between, the call went to some other directory: the result
        /// is discarded (a new socket is closed) and an error is returned. A
        /// change undone before the check is not seen.
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
}
