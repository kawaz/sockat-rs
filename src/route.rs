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
use crate::sys::{self, Op};

/// A path of at most this many bytes is handed to std as is.
///
/// The limit is the same on every target and below both `sun_path` sizes
/// (104 on BSDs / Apple and 108 on Linux, NUL included), so whether a path
/// takes the plain route does not depend on the platform it runs on.
pub(crate) const DIRECT_MAX: usize = 100;

/// Which way a bind / connect went. Only the tests read it; the public API
/// drops it (users are not meant to care).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Route {
    /// The path itself, through std.
    Direct,
    /// `/proc/self/fd/<dirfd>/<name>`, through std (Linux).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    ProcFd,
    /// The path spelled relative to the cwd, through std (Apple).
    #[cfg(target_vendor = "apple")]
    Relative,
    /// The path itself in a widened `sockaddr_un` (Apple).
    #[cfg(target_vendor = "apple")]
    Wide,
    /// The relative spelling in a widened `sockaddr_un` (Apple).
    #[cfg(target_vendor = "apple")]
    WideRelative,
    /// Through a short temporary symlink to the directory, through std (Apple).
    #[cfg(target_vendor = "apple")]
    Symlink,
    /// Through a temporary symlink, in a widened `sockaddr_un` (Apple).
    #[cfg(target_vendor = "apple")]
    SymlinkWide,
    /// `bindat(2)` / `connectat(2)` on the directory fd (FreeBSD).
    #[cfg(target_os = "freebsd")]
    At,
    /// A forked child that `fchdir`s into the directory.
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    Fork,
}

/// Bind or connect a stream socket to `path`, trying the routes in order.
pub(crate) fn run(op: Op, path: &Path) -> io::Result<(OwnedFd, Route)> {
    #[cfg(target_vendor = "apple")]
    return apple::run(op, path, &apple::Config::from_env());
    #[cfg(any(target_os = "linux", target_os = "android"))]
    return linux::run(op, path);
    #[cfg(target_os = "freebsd")]
    return freebsd::run(op, path);
    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd"
    )))]
    return other::run(op, path);
}

/// Bind (and listen) or connect through std with `path` as is.
pub(crate) fn std_op(op: Op, path: &Path) -> io::Result<OwnedFd> {
    match op {
        Op::Bind => UnixListener::bind(path).map(OwnedFd::from),
        Op::Connect => UnixStream::connect(path).map(OwnedFd::from),
    }
}

/// Open `dir` to use as the base of a relative bind / connect.
///
/// `O_RDONLY | O_DIRECTORY`, so the fork route needs read permission on the
/// directory (search and write permission are what a plain bind needs). Linux
/// uses `O_PATH` instead, which needs neither.
#[cfg_attr(
    any(target_os = "linux", target_os = "android"),
    allow(
        dead_code,
        reason = "Linux opens the directory with O_PATH; the tests use this"
    )
)]
fn open_dir(dir: &Path) -> io::Result<OwnedFd> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(dir)
        .map(OwnedFd::from)
}

/// Bind (and listen) or connect a fresh socket to `name` inside `dir` from a
/// forked child that `fchdir`s into `dir` ([`sys::op_in_dir_forked`]).
///
/// `addr` holds `name` (built by the caller, which knows which address forms
/// the target accepts).
#[cfg(any(
    test,
    not(any(target_os = "linux", target_os = "android", target_os = "freebsd"))
))]
pub(crate) fn fork_op(op: Op, dir: &Path, addr: &sys::SockAddr) -> io::Result<OwnedFd> {
    use std::os::fd::AsFd;
    let sock = sys::socket()?;
    let dir = open_dir(dir)?;
    sys::op_in_dir_forked(op, sock.as_fd(), dir.as_fd(), addr)?;
    if op == Op::Bind {
        sys::listen(sock.as_fd())?;
    }
    Ok(sock)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) mod linux {
    use super::*;
    use std::ffi::OsString;
    use std::os::fd::AsRawFd;

    /// Linux: the path itself when it is short, otherwise
    /// `/proc/self/fd/<dirfd>/<name>` with the directory opened `O_PATH`.
    ///
    /// The kernel resolves the `/proc/self/fd/<n>` magic link to the directory
    /// the fd refers to, so the socket is created in (or looked up in) that
    /// directory without ever changing the cwd. Nothing else is tried: this
    /// route has no length limit on the directory, needs no temporary file and
    /// no fork, and works in multi-threaded processes.
    pub(crate) fn run(op: Op, path: &Path) -> io::Result<(OwnedFd, Route)> {
        if path::len(path) <= DIRECT_MAX {
            return std_op(op, path).map(|fd| (fd, Route::Direct));
        }
        proc_fd(op, path).map(|fd| (fd, Route::ProcFd))
    }

    pub(crate) fn proc_fd(op: Op, path: &Path) -> io::Result<OwnedFd> {
        let (dir, name) = path::split(path)?;
        let dir = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
            .open(dir)?;
        let fd_dir = format!("/proc/self/fd/{}", dir.as_raw_fd());
        let mut via = OsString::from(&fd_dir);
        via.push("/");
        via.push(name);
        let via = Path::new(&via);
        if path::len(via) >= sys::SUN_PATH_CAP {
            // The file name alone is too long to fit even behind the shortest
            // directory spelling there is.
            return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
        }
        // Without /proc the bind / connect below would fail with ENOENT, which
        // reads as "the directory does not exist" although it does.
        if let Err(e) = std::fs::metadata(&fd_dir) {
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "binding / connecting a socket path longer than {DIRECT_MAX} bytes needs /proc \
                     ({fd_dir}: {e})"
                ),
            ));
        }
        // `dir` stays open until the call returns (dropped at the end of scope).
        std_op(op, via)
    }
}

#[cfg(target_os = "freebsd")]
pub(crate) mod freebsd {
    use super::*;
    use crate::sys::SockAddr;
    use std::os::fd::AsFd;

    /// FreeBSD: the path itself when it is short, otherwise `bindat(2)` /
    /// `connectat(2)` with the file name relative to the directory fd.
    pub(crate) fn run(op: Op, path: &Path) -> io::Result<(OwnedFd, Route)> {
        if path::len(path) <= DIRECT_MAX {
            return std_op(op, path).map(|fd| (fd, Route::Direct));
        }
        let (dir, name) = path::split(path)?;
        let addr = SockAddr::standard(name)?;
        let dir = open_dir(dir)?;
        let sock = sys::socket()?;
        sys::op_at(op, sock.as_fd(), dir.as_fd(), &addr)?;
        if op == Op::Bind {
            sys::listen(sock.as_fd())?;
        }
        Ok((sock, Route::At))
    }
}

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd"
)))]
mod other {
    use super::*;
    use crate::sys::SockAddr;

    /// Other Unix targets: the path itself when it is short, otherwise the
    /// fork route (the only primitive POSIX alone guarantees).
    pub(crate) fn run(op: Op, path: &Path) -> io::Result<(OwnedFd, Route)> {
        if path::len(path) <= DIRECT_MAX {
            return std_op(op, path).map(|fd| (fd, Route::Direct));
        }
        let (dir, name) = path::split(path)?;
        let addr = SockAddr::standard(name)?;
        fork_op(op, dir, &addr).map(|fd| (fd, Route::Fork))
    }
}

#[cfg(target_vendor = "apple")]
pub(crate) mod apple {
    use super::*;
    use crate::sys::SockAddr;
    use std::ffi::OsStr;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;
    use std::path::PathBuf;

    use crate::sys::WIDE_PATH_MAX;

    /// What the Apple route order reads from the environment, made explicit so
    /// the tests can vary it without touching the process environment.
    #[derive(Debug, Clone)]
    pub(crate) struct Config {
        /// Where the temporary symlink goes: `${TMPDIR:-/tmp}`. `None` when
        /// `TMPDIR` is set to a relative path (it would depend on the cwd),
        /// which skips the symlink route.
        pub(crate) tmp_base: Option<PathBuf>,
        /// Whether to try the widened `sockaddr_un` at all. Always `true`
        /// outside the tests, which turn it off to stand in for a kernel that
        /// refuses the widened form.
        pub(crate) wide: bool,
    }

    impl Config {
        pub(crate) fn from_env() -> Self {
            Self::from_tmpdir(std::env::var_os("TMPDIR"))
        }

        /// The configuration for a `TMPDIR` of `tmpdir`: `${TMPDIR:-/tmp}`,
        /// except that a relative value disables the symlink route.
        pub(crate) fn from_tmpdir(tmpdir: Option<std::ffi::OsString>) -> Self {
            let tmp_base = match tmpdir {
                Some(t) if !t.is_empty() => {
                    let t = PathBuf::from(t);
                    t.is_absolute().then_some(t)
                }
                _ => Some(PathBuf::from("/tmp")),
            };
            Self {
                tmp_base,
                wide: true,
            }
        }
    }

    /// Apple: try, in order (DR-0001 decision 3):
    ///
    /// 1. the path itself, when it is at most [`DIRECT_MAX`] bytes;
    /// 2. the path spelled relative to the cwd, when that is at most
    ///    [`DIRECT_MAX`] bytes;
    /// 3. a widened `sockaddr_un` with the path itself, or failing that with
    ///    the relative spelling, when one of them is at most 253 bytes;
    /// 4. a short symlink to the socket's directory under `${TMPDIR:-/tmp}`;
    /// 5. a forked child that `fchdir`s into the directory.
    ///
    /// A failure of a route ends the search with that error, except where it
    /// shows that the route itself cannot be used (the kernel refusing the
    /// widened form, the symlink not being creatable); then the next route is
    /// tried.
    pub(crate) fn run(op: Op, path: &Path, cfg: &Config) -> io::Result<(OwnedFd, Route)> {
        if path::len(path) <= DIRECT_MAX {
            return std_op(op, path).map(|fd| (fd, Route::Direct));
        }
        let (dir, name) = path::split(path)?;
        let rel = Relative::new(path, dir);

        if let Some(rel) = rel.as_ref().filter(|r| path::len(&r.path) <= DIRECT_MAX) {
            return rel.run(|p| std_op(op, p)).map(|fd| (fd, Route::Relative));
        }

        let mut wide = cfg.wide;
        if wide {
            let attempt = if path::len(path) <= WIDE_PATH_MAX {
                Some(wide_op(op, path).map(|fd| (fd, Route::Wide)))
            } else {
                rel.as_ref()
                    .filter(|r| path::len(&r.path) <= WIDE_PATH_MAX)
                    .map(|rel| {
                        rel.run(|p| wide_op(op, p))
                            .map(|fd| (fd, Route::WideRelative))
                    })
            };
            match attempt {
                None => {}
                Some(Ok(done)) => return Ok(done),
                Some(Err(e)) if refuses_wide(&e) => wide = false,
                Some(Err(e)) => return Err(e),
            }
        }

        if let Some(base) = &cfg.tmp_base {
            match symlink_route(op, base, dir, name, wide) {
                SymlinkOutcome::Done(done) => return done,
                SymlinkOutcome::WideRefused => wide = false,
                SymlinkOutcome::Unusable => {}
            }
        }

        // The plain sockaddr for the file name, or the widened one for a name
        // too long for it when the kernel takes that.
        let addr = match SockAddr::standard(name) {
            Err(e) if wide && e.raw_os_error() == Some(libc::ENAMETOOLONG) => SockAddr::wide(name)?,
            addr => addr?,
        };
        fork_op(op, dir, &addr).map(|fd| (fd, Route::Fork))
    }

    /// Bind (and listen) or connect a fresh socket to `addr`.
    pub(crate) fn raw_op(op: Op, addr: &SockAddr) -> io::Result<OwnedFd> {
        let sock = sys::socket()?;
        sys::op(op, sock.as_fd(), addr)?;
        if op == Op::Bind {
            sys::listen(sock.as_fd())?;
        }
        Ok(sock)
    }

    /// Bind (and listen) or connect a fresh socket to `path` in a widened
    /// `sockaddr_un`.
    pub(crate) fn wide_op(op: Op, path: &Path) -> io::Result<OwnedFd> {
        raw_op(op, &SockAddr::wide(path.as_os_str())?)
    }

    /// Whether an error from a widened-address call means the kernel does not
    /// take the widened form (as opposed to a real outcome such as `ENOENT`,
    /// `EADDRINUSE` or `ECONNREFUSED`). XNU reports an address it will not take
    /// with `EINVAL`, or `ENAMETOOLONG` for one longer than `SOCK_MAXADDRLEN`.
    fn refuses_wide(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENAMETOOLONG))
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
        /// cwd cannot be read, neither way gives a spelling of at most 253
        /// bytes, or the relative directory turns out not to be the absolute
        /// one (a cwd that was removed, or one changed meanwhile by another
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

            if path::len(&rel) > WIDE_PATH_MAX {
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
        /// The path through the symlink needed the widened form and the kernel
        /// refused it.
        WideRefused,
        /// The route could not be used (too long, no usable base, symlink not
        /// creatable).
        Unusable,
    }

    /// Length of the random part of the symlink name (`tmp.XXXXXXXXXX`).
    const LINK_RANDOM_LEN: usize = 10;

    /// Bind / connect through `<base>/tmp.XXXXXXXXXX/<name>`, where the
    /// symlink `tmp.XXXXXXXXXX` points to the socket's directory.
    ///
    /// The length of the path through the symlink is checked before anything
    /// is created. The symlink is a link to the directory, so the kernel
    /// creates (or looks up) the socket inside the real directory; nothing is
    /// left under `base` once the symlink is removed, which happens when this
    /// returns (including on panic) whatever the outcome.
    pub(crate) fn symlink_route(
        op: Op,
        base: &Path,
        dir: &Path,
        name: &OsStr,
        wide: bool,
    ) -> SymlinkOutcome {
        let limit = if wide { WIDE_PATH_MAX } else { DIRECT_MAX };
        let placeholder = format!("tmp.{}", "X".repeat(LINK_RANDOM_LEN));
        if path::len(&base.join(placeholder).join(name)) > limit {
            return SymlinkOutcome::Unusable;
        }
        let target = if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            match std::env::current_dir() {
                Ok(cwd) => cwd.join(dir),
                Err(_) => return SymlinkOutcome::Unusable,
            }
        };
        let Ok(link) = TempLink::create(base, &target) else {
            return SymlinkOutcome::Unusable;
        };
        let via = link.path.join(name);
        if path::len(&via) <= DIRECT_MAX {
            SymlinkOutcome::Done(std_op(op, &via).map(|fd| (fd, Route::Symlink)))
        } else {
            match wide_op(op, &via) {
                Err(e) if refuses_wide(&e) => SymlinkOutcome::WideRefused,
                done => SymlinkOutcome::Done(done.map(|fd| (fd, Route::SymlinkWide))),
            }
        }
    }

    /// A symlink under `base` with a random name, removed on drop.
    pub(crate) struct TempLink {
        pub(crate) path: PathBuf,
    }

    impl TempLink {
        /// How many names to try before giving up. Each try is a fresh random
        /// name from 62^10 possibilities, so exhausting this means something
        /// other than bad luck (for example a base that refuses every name).
        const ATTEMPTS: usize = 16;

        /// Create `<base>/tmp.XXXXXXXXXX` pointing to `target`.
        ///
        /// `symlink(2)` fails with `EEXIST` instead of replacing an existing
        /// entry, which is the `O_EXCL` of symlinks: the link that is created is
        /// always one nobody else had, and a name already taken (by anyone) is
        /// skipped.
        pub(crate) fn create(base: &Path, target: &Path) -> io::Result<Self> {
            const ALPHABET: &[u8; 62] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
            for _ in 0..Self::ATTEMPTS {
                let mut random = [0u8; LINK_RANDOM_LEN];
                sys::random_bytes(&mut random);
                let mut name = String::from("tmp.");
                // 256 is not a multiple of 62, so this is slightly biased; the
                // name only has to be hard to collide with, not uniform.
                name.extend(random.iter().map(|b| ALPHABET[*b as usize % 62] as char));
                let path = base.join(name);
                match std::os::unix::fs::symlink(target, &path) {
                    Ok(()) => return Ok(Self { path }),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not find a free name for a temporary symlink",
            ))
        }
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
