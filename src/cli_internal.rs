//! The entry the `sockat` command (the `sockat-cli` package of this
//! repository) uses to make and check B as this library does. **Not part of
//! the API**: it is behind the feature `__cli-internal`, hidden from the
//! documentation, and may change or go in any release without notice. Do not
//! use it (DR-0033).
//!
//! It gives only what the command needs to have the same B as the library
//! (hub DR-0018 "resolve の既定の綴り" 3: B is made and checked as the
//! library does): the places, the process's B of the places 1-3, B at a
//! given path, `sweep` over the places, and the few constants, the split of
//! a path, the `$PWD` candidate of a relative path and the random names the
//! command spells and names with, so they are the library's own.
//!
//! And for the command's `bind` and `connect`, which try the routes (hub
//! DR-0018 "手法の名指し", DR-0034): `bind_ex` and `connect_ex` by the routes
//! named, in the order named, with the errno names of the hub's conformance
//! CLI. The library's API has no way to choose a route (DR-0011).

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::base::{self, Automatic, Place};
use crate::route::{self, Follows};
use crate::sys::{self, SockType};
use crate::{Bound, Connected, ExError, Options};

/// The longest spelling the kernel takes: `sun_path` less the NUL (107 on
/// Linux, Android and illumos, 103 on Apple and the BSDs).
pub const LIMIT: usize = route::DIRECT_MAX;

/// Whether a bind is known to follow a symlink at the last component of its
/// path on this target (Apple, NetBSD). `false` where it is known not to, and
/// where it is not known.
pub const BIND_FOLLOWS_SYMLINK: bool = matches!(route::FOLLOWS, Follows::Yes);

/// The places 1-3 for B, in order (hub SPEC-routes "実体の置き場").
#[derive(Debug, Clone)]
pub struct Places(Vec<(Place, PathBuf)>);

impl Places {
    /// The places of this process: `/tmp`, `$XDG_RUNTIME_DIR`, `$TMPDIR`
    /// (those [`crate::sweep`] and the library's calls use).
    pub fn from_env() -> Self {
        Self(route::Config::from_env().places)
    }

    /// The places with `tmp` in place of `/tmp`, `$XDG_RUNTIME_DIR` and
    /// `$TMPDIR` as in the environment: for the command's tests, which must
    /// not use the `/tmp/sockat-<uid>` the user's processes share.
    pub fn with_tmp(tmp: PathBuf) -> Self {
        Self(base::places_with_tmp(
            tmp,
            std::env::var_os("TMPDIR"),
            std::env::var_os("XDG_RUNTIME_DIR"),
        ))
    }

    /// The route order's inputs as in the library's calls, with these
    /// places.
    fn config(&self) -> route::Config {
        route::Config {
            places: self.0.clone(),
            ..route::Config::from_env()
        }
    }
}

/// A B, held by its fd for as long as this lives (and, for the places 1-3,
/// for the rest of the process, as the library holds it).
#[derive(Debug, Clone)]
pub struct Base(Arc<base::Base>);

impl Base {
    /// B's real path, which spellings through it start with.
    pub fn path(&self) -> &Path {
        &self.0.path
    }
}

/// What [`automatic`] found.
#[derive(Debug)]
pub enum Found {
    /// The process's B, and `fits` held for it.
    Base(Base),
    /// The process's B is (or would be made) where `fits` does not hold:
    /// nothing is made, and the next place is not tried.
    TooLong,
    /// B can be made in none of the places. `occupied`: a place was passed
    /// over because its B's default name was taken by something that fails
    /// the checks.
    Missing {
        /// A default name was taken in a place passed over.
        occupied: bool,
    },
}

/// The process's B of the places 1-3, as the library's calls choose it (hub
/// SPEC-routes "実体の置き場" and "B の作り方"): the first place where B can
/// be made, unless `fits` (given B's real path, made or to be made) says a
/// spelling through it would not fit there.
pub fn automatic(places: &Places, fits: impl Fn(&Path) -> bool) -> Found {
    let occupied = std::cell::Cell::new(false);
    let held = base::Held::process();
    match base::automatic(&places.0, &held, false, fits, &occupied) {
        Automatic::Base(b) => Found::Base(Base(b)),
        Automatic::TooLong => Found::TooLong,
        Automatic::Missing => Found::Missing {
            occupied: occupied.get(),
        },
    }
}

/// A socket path split into its directory and file name, as the library
/// splits it (at the last `/`; `s.sock` is in `.`).
///
/// # Errors
///
/// `InvalidInput` when it does not end with a name (`dir/`, `dir/.`,
/// `dir/..`, `..`, `/`), which the library refuses whatever the length.
pub fn split(path: &Path) -> io::Result<(&Path, &OsStr)> {
    crate::path::split(path)
}

/// The `$PWD` candidate of the relative `path` whose directory is `dir`, as
/// the library makes it for a bind and a resolve (hub SPEC-routes "絶対パスの
/// 候補の作り方"): `pwd` and `path` joined and folded by their letters, when
/// `pwd` is absolute and is the cwd, and the folded directory is `dir` (the
/// same `(st_dev, st_ino)`). Its length is not looked at.
pub fn pwd_candidate(path: &Path, dir: &Path, pwd: Option<&OsStr>) -> Option<PathBuf> {
    route::pwd_candidate(path, dir, pwd)
}

/// The form of a B the caller names (as `Options.base`): absolute, without
/// NUL, ending with a name. Nothing is looked at on the file system.
///
/// # Errors
///
/// `InvalidInput` when it is not of that form.
pub fn check_base_form(base: &Path) -> io::Result<()> {
    base::split_given(base).map(|_| ())
}

/// The checks of `Options.base` without making anything: the form, the
/// directories on the way, and B itself when it is there.
///
/// # Errors
///
/// `InvalidInput` for the form, `ENOENT`, `ENOTDIR`, `EPERM` as for
/// `Options.base`.
pub fn check_given(base: &Path) -> io::Result<()> {
    base::check_given(base, false)
}

/// B at `base` (as `Options.base`), made when it is not there.
///
/// # Errors
///
/// Those of [`check_given`], and of making B.
pub fn given(base: &Path) -> io::Result<Base> {
    base::given(base, false).map(Base)
}

/// [`crate::sweep`] over `places`.
///
/// # Errors
///
/// Those of [`crate::sweep`].
pub fn sweep(places: &Places) -> io::Result<()> {
    base::sweep(&places.0)
}

/// A random name of 10 ASCII letters and digits from the system's secure
/// generator (the form of the names the library draws).
///
/// # Errors
///
/// When the system's generator fails.
pub fn random_name() -> io::Result<OsString> {
    base::random_name().map(OsString::from)
}

/// What the command's `bind` and `connect` do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// A bind of a stream socket, which listens (`bind_ex`).
    Bind,
    /// A connect of a stream socket (`connect_ex`).
    Connect,
}

/// The name that stands for the order of the operation (the command's
/// `--route default`): the same as naming no route.
pub const DEFAULT: &str = route::DEFAULT;

/// The routes `op` can be made to take on this target, in the order of its
/// routes (hub SPEC "bind の順", "connect の順"): the names [`bind`] and
/// [`connect`] take besides [`DEFAULT`]. Whether one can be used is told
/// when it is tried (the environment, the path).
pub fn routes(op: Op) -> &'static [&'static str] {
    route::nameable(op == Op::Bind)
}

/// The routes to name to the library: none for the order of the operation
/// (no name, or [`DEFAULT`] alone), so it is the library's call itself.
fn named<'a>(routes: Option<&'a [&'static str]>) -> Option<&'a [&'static str]> {
    routes.filter(|r| *r != [DEFAULT])
}

/// `bind_ex` (a stream socket, listening, with the socket file taken), B in
/// `places`, by the routes `routes` in that order when given (a route that
/// cannot be used for this path or here is passed over, and noted in
/// `skipped`; [`DEFAULT`] is the order of a bind at that place).
///
/// # Errors
///
/// Those of `bind_ex`: `ENAMETOOLONG` when none of the routes named can be
/// used, `InvalidInput` for a name not in [`routes`].
pub fn bind(
    places: &Places,
    path: &Path,
    options: Options,
    routes: Option<&[&'static str]>,
) -> Result<Bound<UnixListener>, ExError> {
    let cfg = places.config();
    crate::bound(crate::bind_by(
        SockType::Stream,
        path,
        &options,
        true,
        Some(&cfg),
        named(routes),
    )?)
}

/// `connect_ex` (a blocking stream), B in `places`, by the routes `routes`
/// as for [`bind`].
///
/// # Errors
///
/// Those of `connect_ex`, and as for [`bind`].
pub fn connect(
    places: &Places,
    path: &Path,
    options: Options,
    routes: Option<&[&'static str]>,
) -> Result<Connected<UnixStream>, ExError> {
    let cfg = places.config();
    crate::connect_by(path, false, &options, Some(&cfg), named(routes))
}

/// B another process holds open (`--base-fd`), `held` being
/// `/proc/<pid>/fd/<fd>`: opened with the search permission alone, as B is
/// for `Options.base` (`O_PATH` on Linux, so a B its owner cannot read is
/// taken as well), and checked as B itself is for `Options.base` (hub
/// SPEC-routes "B の作り方"): a directory of this user that group and others
/// cannot write to. The way to it is the process's fd, which nobody else can
/// change.
///
/// # Errors
///
/// Those of opening it, `ENOTDIR` when it is not a directory, `EPERM` when it
/// is someone else's or group or others can write to it, and those of
/// `fstat(2)`.
pub fn open_held(held: &Path) -> io::Result<std::fs::File> {
    let dir = std::fs::File::from(route::open_socket_dir(held)?);
    let m = dir.metadata()?;
    if !m.is_dir() {
        return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    if m.uid() != sys::effective_uid() || m.mode() & 0o022 != 0 {
        return Err(io::Error::from_raw_os_error(libc::EPERM));
    }
    Ok(dir)
}

/// The name of `e` as the hub's conformance CLI writes it (the hub's
/// `docs/design/conformance.md`): the C name of its errno (`errno-<n>` for
/// one not known), `invalid-argument` for an error in the arguments without
/// one, `general` for anything else.
pub fn error_name(e: &io::Error) -> String {
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
/// (`EAGAIN` over `EWOULDBLOCK`), as in the conformance CLI
/// (`examples/conformance.rs`).
// Design rationale: a copy of the conformance CLI's table rather than one
// table for both: that example uses only the public API (DR-0030), and the
// command has no `libc` of its own.
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
