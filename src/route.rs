//! The order in which the ways of reaching a long socket path are tried
//! (DR-0001). [`run`] is the only entry point used by the public API; the
//! per-route functions are `pub(crate)` so the tests can force each route.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

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

/// What to do at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    /// Bind and listen, as `sockat::bind` does (forking when nothing else
    /// reaches the path).
    Bind,
    /// Connect, as the options say.
    Connect(ConnectOptions),
}

impl Request {
    /// The system call this comes down to.
    fn op(self) -> Op {
        match self {
            Request::Bind => Op::Bind,
            Request::Connect(_) => Op::Connect,
        }
    }

    /// Whether the socket is made `O_NONBLOCK` before the call.
    fn nonblocking(self) -> bool {
        matches!(self, Request::Connect(o) if o.nonblocking)
    }

    /// Whether the widened `sockaddr_un` may carry this request's spelling
    /// (Apple): a connect only. A socket bound through the widened form has
    /// an address that does not fit `sun_path`, which `getsockname(2)` then
    /// reports cut short, and readers of it break (CPython appends unrelated
    /// bytes to it, Go's `net.FileConn` loses the peer's `RemoteAddr`). A
    /// connect binds nothing of its own, so its own address is not affected
    /// (DR-0024).
    #[cfg(target_vendor = "apple")]
    fn may_widen(self) -> bool {
        matches!(self, Request::Connect(_))
    }

    /// Whether the fork route may be taken.
    #[cfg(any(test, not(target_os = "freebsd")))]
    fn may_fork(self) -> bool {
        match self {
            Request::Bind => true,
            Request::Connect(o) => o.fork,
        }
    }
}

/// The error for a path that only the fork route reaches, when the options
/// rule the fork route out: `ENAMETOOLONG`, the error of a plain connect to a
/// path longer than `sun_path`. That is what the path is once every other way
/// around the limit is too long or unavailable.
#[cfg(any(test, not(target_os = "freebsd")))]
fn fork_ruled_out() -> io::Error {
    io::Error::from_raw_os_error(libc::ENAMETOOLONG)
}

/// The longest path std accepts, `sun_path` less the NUL: 107 on Linux,
/// Android and illumos, 103 on Apple and the BSDs. A path of at most this many
/// bytes is handed to std as is, so std alone decides what happens to every
/// path it can take (DR-0002). It is also the limit of every other
/// spelling that goes in the plain sockaddr.
pub(crate) const DIRECT_MAX: usize = sys::SUN_PATH_CAP - 1;

/// Which way a bind / connect went. The public API does not show it (users
/// are not meant to care); what it may show is whether the connect took the
/// fork route, [`Route::forked`], whose consequence the server sees.
///
/// "In the plain sockaddr" means through std, except for a nonblocking
/// connect, which std cannot make: that one is sockat's own socket, set up as
/// std's, with the same address bytes ([`plain_op`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// The path itself, in the plain sockaddr.
    Direct,
    /// `/proc/self/fd/<dirfd>/<name>`, in the plain sockaddr (Linux).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    ProcFd,
    /// The path spelled relative to the cwd, in the plain sockaddr.
    #[cfg(not(target_os = "freebsd"))]
    Relative,
    /// The path itself in a widened `sockaddr_un` (Apple, a connect).
    #[cfg(target_vendor = "apple")]
    Wide,
    /// The relative spelling in a widened `sockaddr_un` (Apple, a connect).
    #[cfg(target_vendor = "apple")]
    WideRelative,
    /// Through a short temporary symlink to the socket's directory, in the
    /// plain sockaddr (bind).
    #[cfg(not(target_os = "freebsd"))]
    SymlinkToDir,
    /// Through a short temporary symlink to the socket file itself, in the
    /// plain sockaddr (connect).
    #[cfg(not(target_os = "freebsd"))]
    SymlinkToFile,
    /// [`Route::SymlinkToFile`] in a widened `sockaddr_un` (Apple).
    #[cfg(target_vendor = "apple")]
    SymlinkToFileWide,
    /// Through a symlink to the socket file inside a private directory made
    /// in a place that is not safe and held by an fd,
    /// `/proc/self/fd/<fd>/s`, in the plain sockaddr (Linux, a connect;
    /// [`posix::HeldDir`]).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    SymlinkInHeldDir,
    /// `bindat(2)` / `connectat(2)` on the directory fd (FreeBSD).
    #[cfg(target_os = "freebsd")]
    At,
    /// The file name, on a thread of this process whose own working directory
    /// was moved into the directory (Linux, Apple; [`sys::op_in_dir_on_thread`]).
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    ThreadCwd,
    /// A forked child that `fchdir`s into the directory.
    #[cfg(not(target_os = "freebsd"))]
    Fork {
        /// The pid of the child that made the call ([`sys::op_in_dir_forked`]).
        /// The public API does not return it, only [`Route::forked`]: it is
        /// the pid of an exited process, of no use to the caller or the
        /// server (DR-0010). The tests read it to check that the
        /// peer the server sees is this child.
        child: libc::pid_t,
    },
}

impl Route {
    /// Whether the route forked: the call was made by a forked child.
    ///
    /// For a connect, the peer pid the server sees is then that of the child,
    /// a process that has already exited (DR-0007).
    pub(crate) fn forked(self) -> bool {
        match self {
            #[cfg(not(target_os = "freebsd"))]
            Route::Fork { .. } => true,
            _ => false,
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
    if path::is_abstract(path) {
        return plain_op(req, path).map(|fd| (fd, Route::Direct));
    }
    path::refuse_nul(path)?;
    if path::len(path) <= DIRECT_MAX {
        return plain_op(req, path).map(|fd| (fd, Route::Direct));
    }
    #[cfg(target_os = "freebsd")]
    return freebsd::run_long(req, path);
    #[cfg(not(target_os = "freebsd"))]
    return run_long(req, path, &posix::Config::from_env());
}

/// The routes for a path longer than [`DIRECT_MAX`], with what they read from
/// the environment given as `cfg` (the tests pass their own).
///
/// Linux: `/proc/self/fd/<dirfd>/<name>` with the directory opened `O_PATH`;
/// when that cannot be used, the POSIX order ([`posix::run`]) with that
/// directory fd for the thread-cwd and fork routes, and, when /proc is there
/// (only the spelling was too long), the held-directory form of the symlink
/// route for a connect. Every other target but FreeBSD: the POSIX order.
///
/// The kernel resolves the `/proc/self/fd/<n>` magic link to the directory the
/// fd refers to, so the socket is created in (or looked up in) that directory
/// without ever changing the cwd. That route has no length limit on the
/// directory, needs no temporary file and no fork, and works in multi-threaded
/// processes, so whenever it can be used nothing else is tried. It cannot be
/// used when `<proc_self_fd>/<dirfd>` is not there (no /proc mounted, as in
/// some chroots and minimal containers) or when the spelling does not fit
/// `sun_path` (a file name of about 90 bytes or more). What a bind / connect
/// through /proc returns is the result, never retried on another route.
#[cfg(not(target_os = "freebsd"))]
pub(crate) fn run_long(
    req: Request,
    path: &Path,
    cfg: &posix::Config,
) -> io::Result<(OwnedFd, Route)> {
    let (dir, name) = path::split(path)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::fd::AsFd;
        let opened = open_dir(dir)?;
        // `opened` stays open until this returns (dropped at the end of
        // scope), so the /proc spelling keeps naming it.
        let fd_dir = linux::proc_fd_dir(opened.as_fd(), &cfg.proc_self_fd);
        if let Some(via) = fd_dir
            .as_ref()
            .map(|d| d.join(name))
            .filter(|via| path::len(via) <= DIRECT_MAX)
        {
            return plain_op(req, &via).map(|fd| (fd, Route::ProcFd));
        }
        let proc = fd_dir.is_some().then_some(cfg.proc_self_fd.as_path());
        posix::run(req, path, dir, name, Some(opened.as_fd()), proc, cfg)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    posix::run(req, path, dir, name, None, None, cfg)
}

/// Bind (and listen) or connect with `path` as is, in the plain sockaddr.
///
/// A bind and a blocking connect go to std unchanged. std cannot make a
/// nonblocking connect (`UnixStream::connect` creates and connects its socket
/// in one call), so that one is made on sockat's own socket, set up as std's
/// ([`sys::socket`]) plus `O_NONBLOCK`, with the same address bytes std would
/// build ([`SockAddr::standard`]).
pub(crate) fn plain_op(req: Request, path: &Path) -> io::Result<OwnedFd> {
    match req {
        Request::Bind => UnixListener::bind(path).map(OwnedFd::from),
        Request::Connect(o) if !o.nonblocking => UnixStream::connect(path).map(OwnedFd::from),
        Request::Connect(_) => raw_op(req, &SockAddr::standard(path.as_os_str())?),
    }
}

/// Bind (and listen) or connect a fresh socket (`O_NONBLOCK` for a
/// nonblocking connect) to `addr`.
pub(crate) fn raw_op(req: Request, addr: &SockAddr) -> io::Result<OwnedFd> {
    use std::os::fd::AsFd;
    let sock = sys::socket(req.nonblocking())?;
    sys::op(req.op(), sock.as_fd(), addr)?;
    if req == Request::Bind {
        sys::listen(sock.as_fd())?;
    }
    Ok(sock)
}

/// Open `dir` to use as the base of a relative bind / connect.
///
/// Apple: `O_SEARCH`, which needs only search permission on the directory,
/// the same as a plain bind / connect through the path (bind also needs write
/// permission, checked by the bind itself). An `O_SEARCH` directory fd can be
/// `fchdir`ed into (checked on macOS 26.5: a mode 0300 directory, which
/// `O_RDONLY` cannot open, binds fine). A kernel that does not take `O_SEARCH`
/// on a directory answers `EISDIR` (as it does for `O_EXEC` alone) or
/// `EINVAL`; then the directory is opened `O_RDONLY`, which needs read
/// permission.
///
/// The same fd serves the thread-cwd route: `pthread_fchdir_np` takes an
/// `O_SEARCH` directory fd as `fchdir` does (checked on macOS 26.5: the mode
/// 0300 directory again).
///
/// Linux: `O_PATH | O_DIRECTORY`, which needs only search permission. The fd
/// serves the /proc route (`/proc/self/fd/<fd>` names the directory), the
/// thread-cwd route and the fork route (`fchdir(2)` takes an `O_PATH`
/// directory fd since Linux 3.5).
///
/// Other targets: `O_RDONLY | O_DIRECTORY`, so the fork route and `bindat`
/// need read permission on the directory.
fn open_dir(dir: &Path) -> io::Result<OwnedFd> {
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
/// nonblocking connect) to `name` inside `dir` from a forked child that
/// `fchdir`s into `dir` ([`sys::op_in_dir_forked`]). Returns the socket and
/// the child's pid.
///
/// `addr` holds `name` (built by the caller, which knows which address forms
/// the target accepts).
#[cfg(test)]
pub(crate) fn fork_op(
    req: Request,
    dir: &Path,
    addr: &SockAddr,
) -> io::Result<(OwnedFd, libc::pid_t)> {
    use std::os::fd::AsFd;
    fork_in(req, open_dir(dir)?.as_fd(), addr)
}

/// [`fork_op`] with the directory already opened by [`open_dir`].
///
/// When `req` rules the fork route out, this is where it ends, with
/// [`fork_ruled_out`]: before any socket is created, and after the directory
/// was opened, so a missing or unreadable directory still comes out as the
/// real error it is.
///
/// The socket is made `O_NONBLOCK` here, before the fork, so the child's
/// connect runs on a nonblocking socket: the flag belongs to the open file
/// description that parent and child share.
///
/// After a bind, the listen is made here, by this process, never by the
/// child: the process that listens is the one a client's peer-credential
/// query (`LOCAL_PEERPID`, `SO_PEERCRED`) names as the server, so a child
/// that listened would leave clients seeing an exited process (DR-0025).
#[cfg(any(test, not(target_os = "freebsd")))]
fn fork_in(
    req: Request,
    dir: std::os::fd::BorrowedFd<'_>,
    addr: &SockAddr,
) -> io::Result<(OwnedFd, libc::pid_t)> {
    use std::os::fd::AsFd;
    if !req.may_fork() {
        return Err(fork_ruled_out());
    }
    let sock = sys::socket(req.nonblocking())?;
    let child = sys::op_in_dir_forked(req.op(), sock.as_fd(), dir, addr)?;
    if req == Request::Bind {
        sys::listen(sock.as_fd())?;
    }
    Ok((sock, child))
}

/// Bind (and listen) or connect a fresh socket (`O_NONBLOCK` for a
/// nonblocking connect) to `addr`, the file name, resolved relative to `dir`
/// on a thread whose own working directory is moved there
/// ([`sys::op_in_dir_on_thread`]). `Unusable` when the thread's working
/// directory cannot be moved on its own; the request is then still to be made.
///
/// Every request may take it (no side effect a server or another thread can
/// see), so it comes before the fork route and before a connect that does not
/// fork gives up. The listen after a bind is made here, on the calling thread.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn thread_cwd_in(
    req: Request,
    dir: std::os::fd::BorrowedFd<'_>,
    addr: &SockAddr,
) -> sys::OnThread<OwnedFd> {
    use std::os::fd::AsFd;
    let sock = match sys::socket(req.nonblocking()) {
        Ok(sock) => sock,
        Err(e) => return sys::OnThread::Done(Err(e)),
    };
    match sys::op_in_dir_on_thread(req.op(), sock.as_fd(), dir, addr) {
        sys::OnThread::Done(Ok(())) => {
            let listened = match req {
                Request::Bind => sys::listen(sock.as_fd()),
                Request::Connect(_) => Ok(()),
            };
            sys::OnThread::Done(listened.map(|()| sock))
        }
        sys::OnThread::Done(Err(e)) => sys::OnThread::Done(Err(e)),
        sys::OnThread::Unusable(e) => sys::OnThread::Unusable(e),
    }
}

/// [`thread_cwd_in`] with the directory `dir` opened by [`open_dir`], as the
/// route order opens it.
#[cfg(all(
    test,
    any(target_os = "linux", target_os = "android", target_vendor = "apple")
))]
pub(crate) fn thread_cwd_op(req: Request, dir: &Path, addr: &SockAddr) -> sys::OnThread<OwnedFd> {
    use std::os::fd::AsFd;
    match open_dir(dir) {
        Ok(opened) => thread_cwd_in(req, opened.as_fd(), addr),
        Err(e) => sys::OnThread::Done(Err(e)),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) mod linux {
    use super::*;
    use std::os::fd::{AsRawFd, BorrowedFd};
    use std::path::PathBuf;

    /// Where the kernel lists the fds of the calling process.
    pub(crate) const PROC_SELF_FD: &str = "/proc/self/fd";

    /// `<proc_self_fd>/<dir>`, or `None` when it cannot be looked up (no
    /// /proc). Without this check a bind / connect through
    /// `<proc_self_fd>/<dir>/<name>` would fail with `ENOENT`, which reads as
    /// "the directory does not exist" although it does. The /proc route also
    /// needs that spelling to fit `sun_path` (a file name of about 90 bytes or
    /// less); std would refuse a longer one with its own `InvalidInput`.
    pub(crate) fn proc_fd_dir(dir: BorrowedFd<'_>, proc_self_fd: &Path) -> Option<PathBuf> {
        let fd_dir = proc_self_fd.join(dir.as_raw_fd().to_string());
        std::fs::metadata(&fd_dir).ok()?;
        Some(fd_dir)
    }
}

#[cfg(target_os = "freebsd")]
pub(crate) mod freebsd {
    use super::*;
    use std::os::fd::AsFd;

    /// FreeBSD, for a path longer than [`DIRECT_MAX`]: `bindat(2)` /
    /// `connectat(2)` with the file name relative to the directory fd.
    pub(crate) fn run_long(req: Request, path: &Path) -> io::Result<(OwnedFd, Route)> {
        let (dir, name) = path::split(path)?;
        // The directory first, so a missing directory is ENOENT even when the
        // name fits no sockaddr (DR-0012).
        let dir = open_dir(dir)?;
        let addr = SockAddr::standard(name)?;
        let sock = sys::socket(req.nonblocking())?;
        sys::op_at(req.op(), sock.as_fd(), dir.as_fd(), &addr)?;
        if req == Request::Bind {
            sys::listen(sock.as_fd())?;
        }
        Ok((sock, Route::At))
    }
}

/// The widened `sockaddr_un` that XNU takes (Apple), for a connect only
/// ([`Request::may_widen`]).
#[cfg(target_vendor = "apple")]
pub(crate) mod apple {
    use super::*;

    /// Connect a fresh socket (`O_NONBLOCK` for a nonblocking connect) to
    /// `path` in a widened `sockaddr_un`. There is no bind counterpart: a
    /// socket bound so has an address that does not fit `sun_path` (DR-0024).
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

/// The routes that need nothing but POSIX (plus, on Apple, the widened
/// `sockaddr_un`, and on Linux and Apple a working directory of the thread's
/// own), tried in order on every target without a dirfd-relative primitive:
/// Apple, NetBSD, OpenBSD, illumos and the other Unix targets, and Linux when
/// the /proc route cannot be used (DR-0001, DR-0008, DR-0020).
#[cfg(not(target_os = "freebsd"))]
pub(crate) mod posix {
    use super::*;
    use std::ffi::OsStr;
    use std::os::fd::{AsFd, BorrowedFd};
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    #[cfg(target_vendor = "apple")]
    use crate::sys::WIDE_PATH_MAX;

    /// The longest spelling a sockaddr carries on this target: the widened
    /// form's 253 bytes on Apple, [`DIRECT_MAX`] elsewhere.
    #[cfg(target_vendor = "apple")]
    const SPELLING_MAX: usize = WIDE_PATH_MAX;
    #[cfg(not(target_vendor = "apple"))]
    const SPELLING_MAX: usize = DIRECT_MAX;

    /// What the route order reads from the environment, made explicit so the
    /// tests can vary it without touching the process environment.
    #[derive(Debug, Clone)]
    pub(crate) struct Config {
        /// Where the temporary symlink may go, in the order tried: `TMPDIR`
        /// when it is set to an absolute path, then `/tmp`.
        pub(crate) tmp_bases: Vec<PathBuf>,
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
        /// Whether to try the thread-cwd route at all (Linux, Apple). Always
        /// `true` outside the tests, which turn it off to stand in for a
        /// system where it cannot be used (a seccomp filter, `unshare` refused, no
        /// `pthread_fchdir_np`). Where it is tried and turns out unusable,
        /// the next route is tried just the same.
        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        pub(crate) thread_cwd: bool,
    }

    impl Config {
        pub(crate) fn from_env() -> Self {
            Self::from_tmpdir(std::env::var_os("TMPDIR"))
        }

        /// The configuration for a `TMPDIR` of `tmpdir`: `TMPDIR`, then
        /// `/tmp`. An unset or empty `TMPDIR` leaves `/tmp` alone; a relative
        /// one is left out (the link's place would depend on the cwd); one
        /// that is `/tmp` itself is tried once.
        pub(crate) fn from_tmpdir(tmpdir: Option<std::ffi::OsString>) -> Self {
            let tmp = Path::new("/tmp");
            let mut tmp_bases: Vec<PathBuf> = tmpdir
                .map(PathBuf::from)
                .filter(|t| t.is_absolute() && t != tmp)
                .into_iter()
                .collect();
            tmp_bases.push(tmp.to_path_buf());
            Self {
                tmp_bases,
                #[cfg(target_vendor = "apple")]
                wide: true,
                #[cfg(any(target_os = "linux", target_os = "android"))]
                proc_self_fd: PathBuf::from(linux::PROC_SELF_FD),
                #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
                thread_cwd: true,
            }
        }
    }

    /// Try, in order (DR-0001), for a path longer than
    /// [`DIRECT_MAX`] whose directory part is `dir` and file name `name`:
    ///
    /// 1. the path spelled relative to the cwd, when that is at most
    ///    [`DIRECT_MAX`] bytes;
    /// 2. (Apple, a connect) a widened `sockaddr_un` with the path itself,
    ///    or failing that with the relative spelling, when one of them is at
    ///    most 253 bytes;
    /// 3. a short symlink ([`symlink_route`]: to the socket's directory for a
    ///    bind, to the socket file for a connect) under the first of `TMPDIR`
    ///    and `/tmp` ([`Config::tmp_bases`]) where the path through it fits
    ///    (in the plain sockaddr, or on Apple for a connect the widened one)
    ///    and the symlink can be created. A place that is not safe is passed
    ///    over, except, for a connect when /proc can be used (`proc`: where
    ///    `/proc/self/fd` is, Linux), that it is used through a private
    ///    directory held by an fd ([`HeldDir`]);
    /// 4. (Linux, Apple) the file name on a thread whose own working directory
    ///    is moved into the directory ([`thread_cwd_in`]), for every request;
    /// 5. a forked child that `fchdir`s into the directory, unless the request
    ///    rules it out (then `ENAMETOOLONG`).
    ///
    /// The last two use the directory opened (`opened` when the caller has it
    /// open already, as Linux does) and the file name in a sockaddr: the plain
    /// one, or on Apple for a connect the widened one for a name too long for
    /// it while the kernel takes that form.
    ///
    /// A bind on Apple never uses the widened form (DR-0024): every one of its
    /// spellings is judged against [`DIRECT_MAX`], so a file name over 103
    /// bytes is `ENAMETOOLONG` there.
    ///
    /// A failure of a route ends the search with that error, except where it
    /// shows that the route itself cannot be used (the kernel refusing the
    /// widened form, the symlink not being creatable, the thread's working
    /// directory not being movable on its own); then the next route (or the
    /// next place for the symlink) is tried.
    pub(crate) fn run(
        req: Request,
        path: &Path,
        dir: &Path,
        name: &OsStr,
        opened: Option<BorrowedFd<'_>>,
        proc: Option<&Path>,
        cfg: &Config,
    ) -> io::Result<(OwnedFd, Route)> {
        let rel = Relative::new(path, dir);

        if let Some(rel) = rel.as_ref().filter(|r| path::len(&r.path) <= DIRECT_MAX) {
            return rel
                .run(|p| plain_op(req, p))
                .map(|fd| (fd, Route::Relative));
        }

        // Whether the widened form may still carry a spelling: for a connect
        // only (a bind never widens, [`Request::may_widen`]), and until the
        // kernel refuses it.
        #[cfg(target_vendor = "apple")]
        let mut wide = cfg.wide && req.may_widen();
        #[cfg(target_vendor = "apple")]
        if let (true, Request::Connect(o)) = (wide, req) {
            let attempt = if path::len(path) <= WIDE_PATH_MAX {
                Some(apple::wide_connect(o, path).map(|fd| (fd, Route::Wide)))
            } else {
                rel.as_ref()
                    .filter(|r| path::len(&r.path) <= WIDE_PATH_MAX)
                    .map(|rel| {
                        rel.run(|p| apple::wide_connect(o, p))
                            .map(|fd| (fd, Route::WideRelative))
                    })
            };
            match attempt {
                None => {}
                Some(Ok(done)) => return Ok(done),
                Some(Err(e)) if apple::refuses_wide(&e) => wide = false,
                Some(Err(e)) => return Err(e),
            }
        }

        for base in &cfg.tmp_bases {
            #[cfg(target_vendor = "apple")]
            let limit = if wide { WIDE_PATH_MAX } else { DIRECT_MAX };
            #[cfg(not(target_vendor = "apple"))]
            let limit = DIRECT_MAX;
            match symlink_route(req, base, path, dir, name, limit, proc) {
                SymlinkOutcome::Done(done) => return done,
                #[cfg(target_vendor = "apple")]
                SymlinkOutcome::WideRefused => wide = false,
                SymlinkOutcome::Unusable => {}
            }
        }

        // The directory is opened before the file name's sockaddr is built,
        // so a missing directory is ENOENT even when the name fits no
        // sockaddr (DR-0012).
        let owned;
        let dir_fd = match opened {
            Some(opened) => opened,
            None => {
                owned = open_dir(dir)?;
                owned.as_fd()
            }
        };

        // The plain sockaddr for the file name, or on Apple the widened one
        // for a connect's name too long for it when the kernel takes that.
        #[cfg(target_vendor = "apple")]
        let addr = match SockAddr::standard(name) {
            Err(e) if wide && e.raw_os_error() == Some(libc::ENAMETOOLONG) => SockAddr::wide(name)?,
            addr => addr?,
        };
        #[cfg(not(target_vendor = "apple"))]
        let addr = SockAddr::standard(name)?;

        #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
        if cfg.thread_cwd {
            match thread_cwd_in(req, dir_fd, &addr) {
                sys::OnThread::Done(done) => return done.map(|fd| (fd, Route::ThreadCwd)),
                sys::OnThread::Unusable(_) => {}
            }
        }

        let (fd, child) = fork_in(req, dir_fd, &addr)?;
        Ok((fd, Route::Fork { child }))
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
        /// an error is returned. A socket file that such a bind created is left
        /// where it is, because the directory it went to is unknown. The check
        /// cannot see a change that is undone before it runs; the guarantee is
        /// for processes that do not change the cwd while binding / connecting.
        pub(crate) fn run<T>(&self, f: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
            let result = f(&self.path);
            if dir_id(Path::new(".")) != Some(self.cwd) {
                return Err(io::Error::other(
                    "the current directory changed while binding / connecting through a path \
                     relative to it; the result was discarded",
                ));
            }
            result
        }
    }

    pub(crate) enum SymlinkOutcome {
        /// The route ran; this is its result.
        Done(io::Result<(OwnedFd, Route)>),
        /// The connect's path through the symlink needed the widened form and
        /// the kernel refused it (Apple).
        #[cfg(target_vendor = "apple")]
        WideRefused,
        /// The route could not be used (too long, no usable base, symlink not
        /// creatable, the held directory not usable).
        Unusable,
    }

    /// What the symlink name starts with, so a link left behind by a process
    /// that died is recognisable as sockat's.
    pub(crate) const LINK_PREFIX: &str = "sockat.";

    /// Length of the random part of the symlink name (`sockat.XXXXXXXXXX`).
    pub(crate) const LINK_RANDOM_LEN: usize = 10;

    /// Bind / connect `path` (directory part `dir`, file name `name`) through
    /// a symlink `<base>/sockat.XXXXXXXXXX`:
    ///
    /// | request | the link points to | spelling handed to the kernel | route |
    /// |---|---|---|---|
    /// | bind | `dir` | `<link>/<name>` | [`Route::SymlinkToDir`] |
    /// | connect | `path` | `<link>` | [`Route::SymlinkToFile`] |
    ///
    /// A connect follows a symlink in the last component, so the link to the
    /// file reaches the socket by a spelling whose length depends on neither
    /// the directory nor the file name. A bind does not: Linux refuses it with
    /// `EADDRINUSE`, the link being there already. Both links hold the
    /// caller's own spelling (made absolute against the cwd), so the kernel
    /// resolves them as it resolves `path`.
    ///
    /// Used when the spelling is at most `limit` bytes ([`DIRECT_MAX`] for the
    /// plain sockaddr, or on Apple for a connect the widened form's 253: a
    /// spelling over [`DIRECT_MAX`] then goes in the widened form) and `base`
    /// is a place where nobody else can replace the symlink ([`safe_base`]).
    /// Both are checked before anything is created. Nothing is left under
    /// `base`: the symlink is removed when this returns (including on panic)
    /// whatever the outcome.
    ///
    /// A `base` that is not safe is used for a connect only, and only when
    /// `proc` says where `/proc/self/fd` is (Linux with /proc): through a
    /// private directory created in it and held by an fd, with the link to
    /// the socket file inside ([`HeldDir`], [`Route::SymlinkInHeldDir`]). The
    /// spelling `/proc/self/fd/<fd>/s` does not depend on `base`, so `limit`
    /// is not applied to `base` then. Otherwise nothing is created in it.
    pub(crate) fn symlink_route(
        req: Request,
        base: &Path,
        path: &Path,
        dir: &Path,
        name: &OsStr,
        limit: usize,
        proc: Option<&Path>,
    ) -> SymlinkOutcome {
        let to_file = matches!(req, Request::Connect(_));
        let target = if to_file { path } else { dir };
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join(target),
                Err(_) => return SymlinkOutcome::Unusable,
            }
        };
        if !safe_base(base) {
            return match proc {
                #[cfg(any(target_os = "linux", target_os = "android"))]
                Some(proc) if to_file => held_route(req, base, &target, proc),
                _ => SymlinkOutcome::Unusable,
            };
        }
        let placeholder = base.join(format!("{LINK_PREFIX}{}", "X".repeat(LINK_RANDOM_LEN)));
        let spelling_len = if to_file {
            path::len(&placeholder)
        } else {
            path::len(&placeholder.join(name))
        };
        if spelling_len > limit {
            return SymlinkOutcome::Unusable;
        }
        let Ok(link) = TempLink::create(base, &target) else {
            return SymlinkOutcome::Unusable;
        };
        let (via, plain_route) = if to_file {
            (link.path.clone(), Route::SymlinkToFile)
        } else {
            (link.path.join(name), Route::SymlinkToDir)
        };
        // Only a connect's spelling can be over DIRECT_MAX here: a bind's
        // `limit` is DIRECT_MAX, since a bind never widens.
        #[cfg(target_vendor = "apple")]
        if let (true, Request::Connect(o)) = (path::len(&via) > DIRECT_MAX, req) {
            return match apple::wide_connect(o, &via) {
                Err(e) if apple::refuses_wide(&e) => SymlinkOutcome::WideRefused,
                done => SymlinkOutcome::Done(done.map(|fd| (fd, Route::SymlinkToFileWide))),
            };
        }
        SymlinkOutcome::Done(plain_op(req, &via).map(|fd| (fd, plain_route)))
    }

    /// Whether `base`, followed through symlinks (macOS's `/tmp` is one to
    /// `/private/tmp`), is a directory where no other user (root aside) can
    /// remove or rename an entry, so the symlink cannot be replaced between
    /// its creation and the bind / connect through it ([`only_we_can_replace`]
    /// on its mode and owner). `false` when it cannot be looked up or is not a
    /// directory (a file of ours with mode 0600 passes the mode and owner
    /// test, yet is no place for anything).
    pub(crate) fn safe_base(base: &Path) -> bool {
        std::fs::metadata(base).is_ok_and(|m| {
            m.is_dir() && only_we_can_replace(m.mode(), m.uid(), sys::effective_uid())
        })
    }

    /// Whether, in a directory of `mode` owned by `owner`, only the user `us`
    /// and root can remove or rename the entries `us` creates:
    ///
    /// * **sticky** (`S_ISVTX`, as `/tmp` is) **and owned by root or `us`**:
    ///   an entry can then be removed or renamed only by its owner, the
    ///   directory's owner and root. The owner matters: in a sticky directory
    ///   of another user, that user can replace anything;
    /// * **owned by `us` with no write permission for group and others**
    ///   (`mode & 0o022 == 0`, as a private `TMPDIR` is): nobody else can
    ///   create, remove or rename entries there at all.
    ///
    /// What remains is a replacement by `us` itself or by root, which is
    /// outside what sockat guards against.
    pub(crate) fn only_we_can_replace(mode: u32, owner: u32, us: u32) -> bool {
        const STICKY: u32 = 0o1000;
        const GROUP_OR_OTHER_WRITE: u32 = 0o022;
        let ours = owner == us || owner == 0;
        (mode & STICKY != 0 && ours) || (owner == us && mode & GROUP_OR_OTHER_WRITE == 0)
    }

    /// A symlink under `base` with a random name, removed on drop.
    pub(crate) struct TempLink {
        pub(crate) path: PathBuf,
    }

    impl TempLink {
        /// How many names to try before giving up. Each try is a fresh random
        /// name from 62^10 possibilities, so exhausting this means something
        /// other than bad luck (for example a base that refuses every name).
        pub(crate) const ATTEMPTS: usize = 16;

        /// Create `<base>/sockat.XXXXXXXXXX` pointing to `target`, with a
        /// fresh random name for each try.
        pub(crate) fn create(base: &Path, target: &Path) -> io::Result<Self> {
            Self::create_named(base, target, random_name)
        }

        /// Create a symlink under `base` pointing to `target`, named by
        /// `next_name` (called once per try; its error ends the attempt).
        ///
        /// `symlink(2)` fails with `EEXIST` instead of replacing an existing
        /// entry, which is the `O_EXCL` of symlinks: the link that is created is
        /// always one nobody else had, and a name already taken (by anyone) is
        /// skipped for the next one. Any other error ends the attempt.
        pub(crate) fn create_named(
            base: &Path,
            target: &Path,
            next_name: impl FnMut() -> io::Result<String>,
        ) -> io::Result<Self> {
            create_unique(
                base,
                |path| std::os::unix::fs::symlink(target, path),
                next_name,
            )
            .map(|path| Self { path })
        }
    }

    /// Create an entry under `base` with `create`, named by `next_name`
    /// (called once per try; its error ends the attempt), and return its path.
    ///
    /// `create` must fail with `EEXIST` rather than replace an existing entry,
    /// as `symlink(2)` and `mkdir(2)` do (their `O_EXCL`): the entry created is
    /// always one nobody else had, and a name already taken (by anyone) is
    /// skipped for the next one, [`TempLink::ATTEMPTS`] times at most. Any
    /// other error ends the attempt.
    fn create_unique(
        base: &Path,
        mut create: impl FnMut(&Path) -> io::Result<()>,
        mut next_name: impl FnMut() -> io::Result<String>,
    ) -> io::Result<PathBuf> {
        for _ in 0..TempLink::ATTEMPTS {
            let path = base.join(next_name()?);
            match create(&path) {
                Ok(()) => return Ok(path),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not find a free name for a temporary entry",
        ))
    }

    /// Connect to `target` through a [`HeldDir`] made in `base`, a place that
    /// is not safe: by `<proc>/<fd>/s`, `proc` being where `/proc/self/fd` is.
    /// Unusable when the directory cannot be made or held, or the spelling
    /// does not fit the plain sockaddr; what was made is removed again then,
    /// and after the connect whatever its outcome.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn held_route(req: Request, base: &Path, target: &Path, proc: &Path) -> SymlinkOutcome {
        let Ok(held) = HeldDir::create(base, target, proc) else {
            return SymlinkOutcome::Unusable;
        };
        let via = held.spelling(proc);
        if path::len(&via) > DIRECT_MAX {
            return SymlinkOutcome::Unusable;
        }
        SymlinkOutcome::Done(plain_op(req, &via).map(|fd| (fd, Route::SymlinkInHeldDir)))
    }

    /// A private directory `<base>/sockat.XXXXXXXXXX` made in a place that is
    /// not safe (other users can rename or remove its entries), held by an fd
    /// and holding the symlink `s` to the socket file (Linux, Android).
    ///
    /// The symlink is reached by `/proc/self/fd/<fd>/s`, through the fd rather
    /// than through the directory's name, so another user renaming the
    /// directory away and putting their own in its place changes nothing: the
    /// fd keeps naming the directory sockat made. A replacement between
    /// `mkdir` and `open` is what the checks of [`HeldDir::hold`] are for.
    ///
    /// A value exists only once the directory passed the checks of
    /// [`HeldDir::hold`] (what fails them is someone else's and is left alone,
    /// its contents included), and `linked` is set only once this value's own
    /// `symlinkat` made `s`. On drop: `s` is removed through the fd if this
    /// value made it, then the directory by its name, but only while that
    /// name is still the held directory (same `(st_dev, st_ino)` as the fd),
    /// so a directory put in its place is left alone. Failures are ignored,
    /// as for [`TempLink`].
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub(crate) struct HeldDir {
        pub(crate) path: PathBuf,
        dir: std::fs::File,
        linked: bool,
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    impl HeldDir {
        /// The name of the symlink inside the directory.
        pub(crate) const LINK: &'static std::ffi::CStr = c"s";

        /// Make a private directory in `base` ([`HeldDir::make_private_dir`])
        /// and hold it with a symlink to `target` in it ([`HeldDir::hold`]),
        /// `proc` being where `/proc/self/fd` is.
        pub(crate) fn create(base: &Path, target: &Path, proc: &Path) -> io::Result<Self> {
            Self::hold(Self::make_private_dir(base, random_name)?, target, proc)
        }

        /// Create a directory of mode 0700 under `base`, named as the
        /// temporary symlinks are (`next_name`, a taken name skipped).
        pub(crate) fn make_private_dir(
            base: &Path,
            next_name: impl FnMut() -> io::Result<String>,
        ) -> io::Result<PathBuf> {
            use std::os::unix::fs::DirBuilderExt;
            create_unique(
                base,
                |path| std::fs::DirBuilder::new().mode(0o700).create(path),
                next_name,
            )
        }

        /// Open the directory at `path` and, if it is the one sockat made,
        /// create the symlink `s` to `target` in it.
        ///
        /// It is opened `O_PATH | O_DIRECTORY | O_NOFOLLOW`: a symlink put at
        /// `path` is refused, and no permission on the directory is needed to
        /// open it. What was opened must be a directory owned by the effective
        /// uid with no permission for group and others (`mode & 0o077 == 0`),
        /// as made by [`HeldDir::make_private_dir`]: anything else was put
        /// there by someone else between the `mkdir` and the `open`, and is
        /// neither used nor removed (`InvalidData`).
        ///
        /// Only the group and other bits are judged because the umask may have
        /// taken some of the owner's from the 0700 asked for (umask 0277 makes
        /// it 0500, umask 0477 makes it 0300, without read). The owner's are
        /// then set to 0700 by `chmod("<proc>/<fd>")`, `proc` being where
        /// `/proc/self/fd` is: an `O_PATH` fd takes no `fchmod`, and the /proc
        /// spelling reaches the very directory the fd holds, so nothing put at
        /// `path` meanwhile can be changed instead. So such a umask neither
        /// stops the form nor leaves a directory behind on every call.
        ///
        /// The symlink is created with `symlinkat(2)` on the fd. When the
        /// `chmod` or `symlinkat` fails, the directory is removed again (by
        /// drop).
        pub(crate) fn hold(path: PathBuf, target: &Path, proc: &Path) -> io::Result<Self> {
            use std::os::fd::AsFd;
            let opened = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW)
                .open(&path)?;
            let m = opened.metadata()?;
            if !(m.is_dir() && m.uid() == sys::effective_uid() && m.mode() & 0o077 == 0) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the directory made for the temporary symlink was replaced",
                ));
            }
            let mut held = Self {
                path,
                dir: opened,
                linked: false,
            };
            std::fs::set_permissions(
                held.fd_dir(proc),
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )?;
            sys::symlink_at(target, held.dir.as_fd(), Self::LINK)?;
            held.linked = true;
            Ok(held)
        }

        /// `<proc>/<fd>/s`, `proc` being where `/proc/self/fd` is.
        pub(crate) fn spelling(&self, proc: &Path) -> PathBuf {
            use std::os::unix::ffi::OsStrExt;
            self.fd_dir(proc)
                .join(OsStr::from_bytes(Self::LINK.to_bytes()))
        }

        /// `<proc>/<fd>`: the held directory itself, by its fd.
        fn fd_dir(&self, proc: &Path) -> PathBuf {
            use std::os::fd::AsRawFd;
            proc.join(self.dir.as_raw_fd().to_string())
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    impl Drop for HeldDir {
        fn drop(&mut self) {
            use std::os::fd::AsFd;
            if self.linked {
                let _ = sys::unlink_at(self.dir.as_fd(), Self::LINK);
            }
            let id = |m: std::fs::Metadata| (m.dev(), m.ino());
            let held = self.dir.metadata().map(id);
            let named = std::fs::symlink_metadata(&self.path).map(id);
            if let (Ok(held), Ok(named)) = (held, named) {
                if held == named {
                    let _ = std::fs::remove_dir(&self.path);
                }
            }
        }
    }

    /// `sockat.` followed by [`LINK_RANDOM_LEN`] random letters and digits,
    /// from the system's secure generator ([`sys::random_bytes`]; when it
    /// fails, as `getrandom(2)` does on a Linux older than 3.17, the symlink
    /// route cannot be used).
    fn random_name() -> io::Result<String> {
        const ALPHABET: &[u8; 62] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut random = [0u8; LINK_RANDOM_LEN];
        sys::random_bytes(&mut random)?;
        let mut name = String::from(LINK_PREFIX);
        // 256 is not a multiple of 62, so this is slightly biased; the name only
        // has to be hard to collide with, not uniform.
        name.extend(random.iter().map(|b| ALPHABET[*b as usize % 62] as char));
        Ok(name)
    }

    impl Drop for TempLink {
        fn drop(&mut self) {
            // Best effort: the socket is already bound / connected, and failing
            // it now would not bring the symlink back either. In a sticky /tmp
            // or a private TMPDIR nobody else can have replaced the entry.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
