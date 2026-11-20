//! Bind and connect Unix domain sockets whose path is longer than `sun_path`
//! allows.
//!
//! `struct sockaddr_un` keeps the socket path in a fixed array: 104 bytes on
//! macOS and the BSDs, 108 on Linux and illumos, the terminating NUL included.
//! std's [`UnixListener::bind`], [`UnixStream::connect`] and
//! [`UnixDatagram::bind`](std::os::unix::net::UnixDatagram::bind) refuse anything longer, although the file system has
//! no such limit. Deep temporary directories (macOS's `$TMPDIR` alone is about
//! 50 bytes; test harnesses and sandboxes nest further) run into it easily.
//! The limit is on the argument of `bind(2)` / `connect(2)`, so sockat hands
//! the kernel a short spelling that names the same place.
//!
//! Use [`bind`] and [`connect`] where you would use std's, and
//! [`connect_nonblocking`] in async code; [`datagram`] has the same for
//! `SOCK_DGRAM`. They take a path of any length and return std's own types. A
//! path std accepts goes to std unchanged, so it behaves exactly as with std:
//!
//! | Platform | `sun_path` (NUL included) | Handed to std |
//! |---|---|---|
//! | Linux, Android, illumos | 108 | up to 107 bytes |
//! | macOS, iOS, FreeBSD, NetBSD, OpenBSD | 104 | up to 103 bytes |
//!
//! ```no_run
//! use std::io::{Read, Write};
//!
//! let path = "/a/very/long/directory/.../server.sock";
//! let listener = sockat::bind(path)?;
//! let mut client = sockat::connect(path)?;
//! let (mut server, _) = listener.accept()?;
//! client.write_all(b"ping")?;
//! let mut buf = [0; 4];
//! server.read_exact(&mut buf)?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! The behaviour is the language-independent
//! [specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md);
//! the details, with examples, are in the
//! [manual](https://github.com/kawaz/sockat-rs/blob/main/docs/MANUAL.md).
//!
//! # How a long path is reached
//!
//! A bind's spelling is kept by the kernel for the life of the socket and
//! reported by `getsockname` / `getpeername`; libraries remove the socket file
//! by it and a datagram peer replies to it. So a long path is bound by a short
//! spelling that keeps naming the same place: through a symlink E in a
//! directory D of this process, in a directory B of this user.
//!
//! * **B** is `/tmp/sockat-<uid>` (else `$XDG_RUNTIME_DIR/sockat`, else
//!   `$TMPDIR/sockat-<uid>`, else `.sockat.<random>` in the socket's
//!   directory), mode 0711, checked to be this user's and to be reached only
//!   through directories no other user can change. It is reused across
//!   processes and never removed by sockat. [`Options::base`] puts it where
//!   the application says.
//! * **D** is a directory with a random name in B, one per process and B, on
//!   which the process holds a shared `flock(2)` for its life.
//! * **E** is a symlink in D: to the socket's directory (**link-dir**,
//!   spelling `B/D/E/<file name>`) or to the socket file (**link-file**,
//!   `B/D/E`, where a bind follows a symlink: macOS, NetBSD).
//!
//! B is spelled `/proc/<pid>/fd/<B's fd>` where `/proc` can be used, else by
//! its real path. When no such spelling fits, a bind goes through
//! **thread-cwd** (Linux, Apple: a thread of its own moves its own working
//! directory into D) or **at** (FreeBSD: `bindat(2)`) with a random E that is
//! removed right away (the spelling then names nothing).
//!
//! A connect's spelling is kept nowhere, so a connect leaves nothing: it
//! tries the routes that need neither writing nor a thread first
//! (**proc-direct** `/proc/self/fd/<dirfd>/<file name>` on Linux,
//! **relative** to the cwd, **widened** sockaddr up to 253 bytes on Apple,
//! **at-direct** `connectat(2)` on FreeBSD), then **thread-cwd-direct**
//! (Linux, Apple), then **link-file** through an E made directly in B and
//! removed right after the connect.
//!
//! | Platform | bind | connect |
//! |---|---|---|
//! | Linux, Android | link-dir, thread-cwd | proc-direct (relative without `/proc`), thread-cwd-direct, link-file |
//! | macOS, iOS (Apple) | link-dir, link-file, thread-cwd | relative, widened, thread-cwd-direct, link-file |
//! | FreeBSD | link-dir, at | at-direct, link-file |
//! | NetBSD | link-dir, link-file | relative, link-file |
//! | OpenBSD, illumos | link-dir | relative, link-file |
//!
//! The first route that can be used makes the call; a real failure there
//! (`ENOENT`, `EADDRINUSE`, `ECONNREFUSED`, ...) is the result, not a reason
//! to try the next. No route forks, and none changes the working directory of
//! the process or of the calling thread.
//!
//! # What a bind leaves, and what can be cleaned up
//!
//! [`bind`] keeps its E until the process ends; dead processes' D (and their
//! E) are removed by the system's cleanup of temporary directories, or by
//! [`sweep`]. [`bind_ex`] returns a [`SocketFile`]: [`SocketFile::remove`]
//! removes the socket file (only while it is still the one bound) and the E,
//! [`SocketFile::cleanup`] the E alone. [`resolve_for_bind`] and
//! [`resolve_for_connect`] return a spelling to hand to std yourself, with
//! the same clean-up. A connect never leaves anything.
//!
//! What a bind's spelling is good for is in [`Spelling`]: whether removing by
//! it removes the socket file, until when it can be followed, and whether
//! another uid can follow it.
//!
//! # Patterns not to use
//!
//! * removing or `stat`ing the socket file by the address the kernel reports
//!   without looking at [`Spelling`]: a link-file spelling removes E, not the
//!   file, and a `Dangling` one names nothing. Keep the path, or use
//!   [`SocketFile::remove`];
//! * using a spelling after its clean-up or after the process ended;
//! * binding a relative path and removing by it after the cwd changes;
//! * relying on closing the listener to remove the socket file, or on
//!   [`SocketFile::remove`] to close the listener;
//! * calling [`SocketFile::remove`] after the listener is closed (a file made
//!   again at the path may then not be told apart);
//! * branching on `route`, `base` or `skipped`: they are diagnostics; branch
//!   on [`Spelling`].
//!
//! What to do instead, run as a test:
//!
//! ```
//! use std::io;
//! use std::path::Path;
//!
//! fn serve(path: &Path) -> io::Result<()> {
//!     let path = std::path::absolute(path)?; // absolute before the bind
//!     let bound = sockat::bind_ex(&path, Default::default())?;
//!     if let sockat::Spelling::Linked { removable: false, .. } = bound.spelling {
//!         // removing by local_addr()'s spelling would remove E: use the path or remove()
//!     }
//!     // ... accept on bound.socket (it may go to tokio etc.) ...
//!     bound.file.remove()?; // before the listener is closed
//!     drop(bound.socket);
//!     Ok(())
//! }
//! # let dir = tempfile::Builder::new().prefix("sockat-").tempdir_in("/tmp")?;
//! # let deep = dir.path().join("d".repeat(150));
//! # std::fs::create_dir(&deep)?;
//! # serve(&deep.join("s.sock"))?;
//! # assert_eq!(std::fs::read_dir(&deep)?.count(), 0);
//! # Ok::<(), io::Error>(())
//! ```
//!
//! # Errors
//!
//! The errors of `bind(2)`, `listen(2)` and `connect(2)` as
//! [`std::io::Error`]s: `ENOENT` for a missing directory or socket,
//! `ENOTDIR`, `EADDRINUSE`, `ECONNREFUSED`, `EAGAIN` for a full backlog on a
//! nonblocking connect (Linux; macOS answers `ECONNREFUSED`). `ENAMETOOLONG`
//! when no route reaches the path. `InvalidInput` for a path longer than std
//! accepts that does not end with a file name, and for a path of any length
//! with a NUL byte (on Linux a leading NUL, the abstract namespace, goes to
//! std as it is). [`Options::base`] that fails its checks is `ENOENT`,
//! `ENOTDIR` or `EPERM`, never another base (`InvalidInput` when it is
//! relative or does not end with a name). The `_ex` functions return an
//! [`ExError`], which converts into the `io::Error` by `?`.
//!
//! # Async runtimes
//!
//! The results are std types, so any runtime that adopts std sockets takes
//! them. [`connect_nonblocking`] does not wait (a Unix-domain connect
//! completes or fails within the call, and its clean-up is an `unlink`) and
//! returns the stream nonblocking:
//!
//! ```no_run
//! # async fn demo(path: std::path::PathBuf) -> std::io::Result<()> {
//! let listener = sockat::bind(&path)?;
//! listener.set_nonblocking(true)?;
//! let listener = tokio::net::UnixListener::from_std(listener)?;
//!
//! let stream = sockat::connect_nonblocking(&path)?;
//! let stream = tokio::net::UnixStream::from_std(stream)?;
//! # Ok(())
//! # }
//! ```

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(not(unix))]
compile_error!("sockat supports Unix targets only (Windows is out of scope)");

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use route::{Connect, Request, Use};
use sys::SockType;

mod base;
pub mod datagram;
mod path;
mod route;
mod sys;

#[cfg(test)]
mod tests;

pub use route::Skipped;

/// What the `_ex` functions and resolve take besides the path.
///
/// [`Options::default`] is what [`bind`] and [`connect`] do.
///
/// ```no_run
/// let options = sockat::Options {
///     base: Some("/tmp/my-app-1000/sockat".into()),
///     ..Default::default()
/// };
/// let bound = sockat::bind_ex("/a/very/long/directory/.../server.sock", options)?;
/// # Ok::<(), sockat::ExError>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Ask a bind for a spelling another uid can follow as well: B only where
    /// every directory on the way to it and B itself let others search (`/tmp`
    /// does; `$XDG_RUNTIME_DIR` and macOS's `$TMPDIR` are 0700), and never the
    /// `/proc/<pid>/fd/...` spelling. When that cannot be had, the bind goes
    /// through thread-cwd / at (a `Dangling` spelling) rather than give a
    /// spelling another uid cannot follow; [`Spelling`] tells. The connects
    /// do not look at it (their spelling is kept nowhere).
    pub usable_cross_uid: bool,
    /// Where B is (B itself, for example `/tmp/my-app-1000/sockat`), instead
    /// of the automatic bases: an application's own directory, which it
    /// cleans up itself ([`sweep_base`]). Made when missing, checked (the
    /// directories on the way no other user can change, B this user's and not
    /// writable by group and others), and an error (`ENOENT`, `ENOTDIR`,
    /// `EPERM`; a symlink there is `EPERM`) rather than another base when
    /// the check fails; it must be absolute and end with a name (else
    /// `InvalidInput`, whatever the length of the path). The check is
    /// made before any route is tried, for a long path only; B is made only
    /// when a route that uses it is reached. Used for the binds and the
    /// connects.
    pub base: Option<PathBuf>,
}

/// What a bind's spelling (the address the kernel keeps and reports by
/// `getsockname` / `getpeername`) is good for.
///
/// `#[non_exhaustive]`: read a kind you do not know as "do not rely on the
/// spelling".
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Spelling {
    /// The absolute path itself (std took it).
    Original,
    /// The relative path itself (std took it): after the cwd changes it names
    /// another place. A hazard std has on its own, reported all the same.
    CwdRelative,
    /// Through B, D and E, naming the same place as the path while it can be
    /// followed.
    Linked {
        /// Whether unlinking the spelling removes the socket file: `true`
        /// through a symlink to the directory (link-dir), `false` through a
        /// symlink to the file (link-file: the unlink removes E). Remove a
        /// link-file socket by [`SocketFile::remove`] or the path.
        removable: bool,
        /// Until when the spelling can be followed.
        until: Until,
        /// Whether a process of another uid can follow it.
        usable_cross_uid: bool,
        /// Whether a peer can follow it even when this process or the peer
        /// has a `/tmp` of its own (systemd's `PrivateTmp`): `false` when B is
        /// under `/tmp` or `/var/tmp`. A peer in another mount namespace as a
        /// whole (a container) is not covered.
        usable_with_private_tmp: bool,
    },
    /// Through an E removed right after the bind (thread-cwd, at): it names
    /// nothing. Removing by it removes nothing.
    Dangling,
}

/// Until when a [`Spelling::Linked`] spelling can be followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Until {
    /// While this process lives (`/proc/<pid>/fd/<B's fd>/...`): only
    /// processes of the same uid in the same PID namespace (and root) follow
    /// it.
    Process,
    /// Until E is cleaned up ([`SocketFile::cleanup`], [`ForBind::cleanup`]),
    /// or for [`bind`], until D is removed after the process ends (B's real
    /// path).
    Cleanup,
}

/// What [`bind_ex`] and [`datagram::bind_ex`] return.
///
/// The fields are public so the value can be taken apart as it comes (a
/// pattern needs `..`: later versions may add fields):
///
/// ```no_run
/// let path = "/a/very/long/directory/.../server.sock";
/// let sockat::Bound { socket, file, .. } = sockat::bind_ex(path, Default::default())?;
/// // ... serve on `socket` (it may go to tokio etc.) ...
/// file.remove()?; // before the listener is closed
/// drop(socket);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[non_exhaustive]
#[derive(Debug)]
pub struct Bound<S> {
    /// The socket, std's own type, as [`bind`] / [`datagram::bind`] return it.
    pub socket: S,
    /// Removes the socket file and cleans up E later; call it before the
    /// socket is closed. Closing the socket stays with whoever has it.
    pub file: SocketFile,
    /// What the spelling the kernel keeps is good for. Branch on this.
    pub spelling: Spelling,
    /// The route that bound it (such as `"link-dir"`). Diagnostics only:
    /// routes may be added, renamed or changed.
    pub route: &'static str,
    /// The B the spelling goes through (`proc`, `tmp`, `runtime`, `tmpdir`,
    /// `sockdir`, `given`), `None` when it goes through none. Diagnostics only.
    pub base: Option<&'static str>,
    /// The routes tried and passed over before [`Bound::route`], in the order
    /// tried, each with why; `standard` first for a long path. Diagnostics
    /// only.
    pub skipped: Vec<Skipped>,
}

/// What [`connect_ex`] and [`connect_nonblocking_ex`] return.
#[non_exhaustive]
#[derive(Debug)]
pub struct Connected<S> {
    /// The connected socket, std's own type.
    pub socket: S,
    /// The route that connected it. Diagnostics only.
    pub route: &'static str,
    /// The routes passed over before it. Diagnostics only.
    pub skipped: Vec<Skipped>,
}

/// What [`datagram::connect_ex`] returns: how the caller's socket was
/// connected.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The route that connected it. Diagnostics only.
    pub route: &'static str,
    /// The routes passed over before it. Diagnostics only.
    pub skipped: Vec<Skipped>,
}

/// Why an `_ex` function or a resolve did not reach the path: the
/// [`io::Error`], and the routes tried, for diagnostics.
///
/// It converts into the [`io::Error`] as it is (`From`), so `?` in a function
/// returning [`io::Result`] works and [`io::Error::raw_os_error`] still gives
/// the errno. `Display` and `source` are those of the [`io::Error`].
#[non_exhaustive]
#[derive(Debug)]
pub struct ExError {
    /// The error, as the plain function returns it.
    pub error: io::Error,
    /// The route that made the call and failed for real (`EADDRINUSE`,
    /// `ECONNREFUSED`, ...); `None` when no route could be used
    /// (`ENAMETOOLONG`), the path was refused before any was tried, or
    /// [`Options::base`] failed. Diagnostics only.
    pub route: Option<&'static str>,
    /// The routes tried and passed over, in the order tried; empty when the
    /// path was refused before any route was tried. Diagnostics only.
    pub skipped: Vec<Skipped>,
}

impl std::fmt::Display for ExError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for ExError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

impl From<ExError> for io::Error {
    fn from(e: ExError) -> Self {
        e.error
    }
}

impl From<route::Failed> for ExError {
    fn from(f: route::Failed) -> Self {
        ExError {
            error: f.error,
            route: f.route,
            skipped: f.skipped,
        }
    }
}

/// The socket file a bind made (its real path and `(st_dev, st_ino)`, taken
/// right after the bind), and the E its spelling goes through.
///
/// Dropping it does nothing: the file is removed only by
/// [`SocketFile::remove`], E only by it or [`SocketFile::cleanup`].
#[derive(Debug)]
pub struct SocketFile {
    path: PathBuf,
    id: (u64, u64),
    link: Option<Arc<base::Link>>,
    removed: AtomicBool,
}

impl SocketFile {
    fn new(r: route::Remembered, link: Option<Arc<base::Link>>) -> Self {
        Self {
            path: r.path,
            id: r.id,
            link,
            removed: AtomicBool::new(false),
        }
    }

    /// Remove the socket file, the first time only, if what is at its real
    /// path now is still the file the bind made (the same `(st_dev,
    /// st_ino)`, looked up without following a symlink), and clean up E
    /// ([`SocketFile::cleanup`]). Later calls do nothing.
    ///
    /// Anything else there (another socket bound since, a file put in its
    /// place) is left alone, and so is nothing at all: both are `Ok(())`. The
    /// socket is not closed; call this before closing it. The promise covers
    /// what replaced the file while the socket was open: once it is closed, a
    /// file removed and made again at the path may get the same inode number
    /// (ext4, the BSDs' UFS / FFS) and be removed, as removing by the path
    /// would. A file put at the path between the look-up and the removal, by
    /// someone who can write to the directory, is removed instead (POSIX has
    /// no way to remove a file by its inode).
    ///
    /// # Errors
    ///
    /// Those of `lstat(2)` (other than `ENOENT` and `ENOTDIR`) and `unlink(2)`
    /// on the path, and of removing E; a call after an error tries again.
    pub fn remove(&self) -> io::Result<()> {
        if self.removed.load(Ordering::Acquire) {
            return Ok(());
        }
        let gone = |e: &io::Error| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
        let removed = match file_id(&self.path) {
            Ok(id) if id == self.id => match std::fs::remove_file(&self.path) {
                Err(e) if gone(&e) => Ok(()),
                removed => removed,
            },
            Ok(_) => Ok(()),
            Err(e) if gone(&e) => Ok(()),
            Err(e) => Err(e),
        };
        // E is cleaned up even when the file could not be removed; the
        // file's error is the one returned (hub SPEC "remove").
        let cleaned = self.cleanup();
        removed?;
        cleaned?;
        self.removed.store(true, Ordering::Release);
        Ok(())
    }

    /// Remove the E the spelling goes through, the first time only (later
    /// calls do nothing). The spelling names nothing afterwards; the socket
    /// file and the socket stay. Nothing to do for a spelling through no E.
    ///
    /// # Errors
    ///
    /// Those of `unlink(2)` on E (other than `ENOENT`).
    pub fn cleanup(&self) -> io::Result<()> {
        match &self.link {
            Some(link) => link.cleanup(),
            None => Ok(()),
        }
    }
}

/// `(st_dev, st_ino)` of `path`, without following a symlink at it.
fn file_id(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    Ok((m.dev(), m.ino()))
}

/// The new socket of a call that makes one.
fn new_socket(socket: Option<OwnedFd>) -> io::Result<OwnedFd> {
    socket.ok_or_else(|| io::Error::other("no socket was made"))
}

/// Bind a socket of `ty` to `path` and, with `remember`, take the socket file.
pub(crate) fn bind_with(
    ty: SockType,
    path: &Path,
    options: &Options,
    remember: bool,
) -> Result<(OwnedFd, Option<SocketFile>, route::Reached), ExError> {
    let mut r = route::run(Request::Bind(ty), path, options, remember, None)?;
    // Neither happens (a bind makes its socket, and looks the file up when
    // asked); if one did, the listener is closed and E removed, the socket
    // file left, as when the file cannot be looked up (hub SPEC "SocketFile").
    let failed = |r: route::Reached, error: io::Error| {
        if let Some(link) = &r.link {
            let _ = link.cleanup();
        }
        ExError {
            error,
            route: Some(r.route),
            skipped: r.skipped,
        }
    };
    let socket = match new_socket(r.socket.take()) {
        Ok(socket) => socket,
        Err(error) => return Err(failed(r, error)),
    };
    let file = r.file.take().map(|f| SocketFile::new(f, r.link.clone()));
    if remember && file.is_none() {
        drop(socket);
        return Err(failed(
            r,
            io::Error::other("the socket file was not looked up"),
        ));
    }
    Ok((socket, file, r))
}

/// The [`Bound`] of a bind made with `remember`.
pub(crate) fn bound<S: From<OwnedFd>>(
    (socket, file, r): (OwnedFd, Option<SocketFile>, route::Reached),
) -> Result<Bound<S>, ExError> {
    // `bind_with` with `remember` never hands back a bind without the file.
    let Some(file) = file else {
        drop(socket);
        if let Some(link) = &r.link {
            let _ = link.cleanup();
        }
        return Err(ExError {
            error: io::Error::other("the socket file was not looked up"),
            route: Some(r.route),
            skipped: r.skipped,
        });
    };
    Ok(Bound {
        socket: S::from(socket),
        file,
        spelling: r.spelling.unwrap_or(Spelling::Original),
        route: r.route,
        base: r.base,
        skipped: r.skipped,
    })
}

/// Create a socket bound to `path` and listening on it, like
/// [`UnixListener::bind`] but for a path of any length.
///
/// A path that std accepts goes to [`UnixListener::bind`] unchanged. A longer
/// path is bound by the routes of the platform (see the [crate
/// documentation](crate)); the socket file is created at `path` either way,
/// and is not removed when the listener is dropped (as with std). Its E stays
/// until the process ends ([`bind_ex`] returns a [`SocketFile`] that removes
/// the file and E).
///
/// # Errors
///
/// The errors of `bind(2)` / `listen(2)`, for example `EADDRINUSE` when `path`
/// exists and `ENOENT` when its directory does not; `ENAMETOOLONG` when no
/// route reaches the path; `InvalidInput` for a path longer than std accepts
/// that does not end with a file name, or for a path that contains a NUL
/// byte.
pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
    let (socket, _, _) = bind_with(SockType::Stream, path.as_ref(), &Options::default(), false)?;
    Ok(UnixListener::from(socket))
}

/// Connect to the socket at `path`, like [`UnixStream::connect`] but for a
/// path of any length. Nothing is left behind.
///
/// The stream blocks, like std's. In async code use [`connect_nonblocking`].
///
/// # Errors
///
/// The errors of `connect(2)`, for example `ENOENT` when nothing is at `path`
/// and `ECONNREFUSED` when nobody listens on it (on macOS also when its
/// backlog is full); `ENAMETOOLONG` when no route reaches the path;
/// `InvalidInput` as for [`bind`].
pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
    connect_with(path.as_ref(), false, &Options::default())
        .map(|c| c.socket)
        .map_err(io::Error::from)
}

/// [`connect`] on a nonblocking socket: the connect does not wait, and the
/// stream it returns is still nonblocking (ready for
/// `tokio::net::UnixStream::from_std` as it is).
///
/// A Unix-domain connect completes or fails within the call, and sockat's
/// clean-up after it is an `unlink`, so nothing waits. When the listener's
/// backlog is full the connect fails at once: `EAGAIN` on Linux (where a
/// blocking connect waits for room), `ECONNREFUSED` on macOS.
///
/// # Errors
///
/// Those of [`connect`], and `EAGAIN` for a full backlog on Linux.
pub fn connect_nonblocking<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
    connect_with(path.as_ref(), true, &Options::default())
        .map(|c| c.socket)
        .map_err(io::Error::from)
}

fn connect_with(
    path: &Path,
    nonblocking: bool,
    options: &Options,
) -> Result<Connected<UnixStream>, ExError> {
    let req = Request::Connect(Connect::Stream { nonblocking });
    let r = route::run(req, path, options, false, None)?;
    let socket = new_socket(r.socket).map_err(|error| ExError {
        error,
        route: Some(r.route),
        skipped: r.skipped.clone(),
    })?;
    Ok(Connected {
        socket: UnixStream::from(socket),
        route: r.route,
        skipped: r.skipped,
    })
}

/// [`bind`], with the [`SocketFile`] that removes the socket file and cleans
/// up E later, what the spelling is good for, and how it was bound
/// ([`Bound`]). `options` chooses B ([`Options`]).
///
/// The socket file's real path and `(st_dev, st_ino)` are taken right after
/// the bind, before this returns. For a relative `path`, the cwd's `(st_dev,
/// st_ino)` must be the same before the bind and after that look-up (another
/// thread changing it would make the file looked up another one); otherwise
/// the listener is closed and an error returned (the socket file stays, and
/// so does nothing else). A listener short of a [`SocketFile`] that could
/// remove the file is never handed out. Unlike std's and [`bind`]'s, nothing
/// is removed when the listener is closed even for a short path: remove by
/// [`SocketFile::remove`].
///
/// ```no_run
/// let path = "/a/very/long/directory/.../server.sock";
/// let bound = sockat::bind_ex(path, Default::default())?;
/// if let sockat::Spelling::Linked { removable: false, .. } = bound.spelling {
///     // removing by `bound.socket.local_addr()` would remove E, not the file
/// }
/// // ... serve on `bound.socket` ...
/// bound.file.remove()?;
/// drop(bound.socket);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// Those of [`bind`], those of [`Options::base`], and those of looking the
/// socket file up right after the bind, as an [`ExError`] with the routes
/// tried.
pub fn bind_ex<P: AsRef<Path>>(path: P, options: Options) -> Result<Bound<UnixListener>, ExError> {
    bound(bind_with(SockType::Stream, path.as_ref(), &options, true)?)
}

/// [`connect`], with how it connected ([`Connected`]). `options` chooses B
/// for the link-file route ([`Options::base`]); `usable_cross_uid` is not
/// looked at.
///
/// # Errors
///
/// Those of [`connect`] and of [`Options::base`], as an [`ExError`] with the
/// routes tried.
pub fn connect_ex<P: AsRef<Path>>(
    path: P,
    options: Options,
) -> Result<Connected<UnixStream>, ExError> {
    connect_with(path.as_ref(), false, &options)
}

/// [`connect_ex`] on a nonblocking socket, as [`connect_nonblocking`] is to
/// [`connect`].
///
/// # Errors
///
/// Those of [`connect_ex`], and `EAGAIN` for a full backlog on Linux.
pub fn connect_nonblocking_ex<P: AsRef<Path>>(
    path: P,
    options: Options,
) -> Result<Connected<UnixStream>, ExError> {
    connect_with(path.as_ref(), true, &options)
}

/// A spelling for a bind, from [`resolve_for_bind`]: hand [`ForBind::path`]
/// to std (or any library) to bind.
#[non_exhaustive]
#[derive(Debug)]
pub struct ForBind {
    /// The spelling: the path itself when std accepts it, else one through
    /// B, D and E that keeps naming the same place until
    /// [`ForBind::cleanup`].
    pub path: PathBuf,
    /// What the spelling is good for once bound.
    pub spelling: Spelling,
    /// How it was made (`standard`, `link-dir`, `link-file`). Diagnostics only.
    pub route: &'static str,
    /// The B it goes through. Diagnostics only.
    pub base: Option<&'static str>,
    /// The routes passed over. Diagnostics only.
    pub skipped: Vec<Skipped>,
    link: Option<Arc<base::Link>>,
    cwd: Option<(u64, u64)>,
}

impl ForBind {
    /// Remove E, the first time only (later calls do nothing): the spelling
    /// names nothing afterwards. Not the socket file: that is
    /// [`ForBind::socket_file`]'s [`SocketFile::remove`].
    ///
    /// # Errors
    ///
    /// Those of `unlink(2)` on E (other than `ENOENT`).
    pub fn cleanup(&self) -> io::Result<()> {
        match &self.link {
            Some(link) => link.cleanup(),
            None => Ok(()),
        }
    }

    /// The [`SocketFile`] of the socket bound by [`ForBind::path`]: call it
    /// after the bind (and before the socket is closed). The spelling is
    /// followed now, and the socket file's real path and `(st_dev, st_ino)`
    /// taken; its [`SocketFile::remove`] removes the file and cleans up the
    /// same E as [`ForBind::cleanup`] (once, whichever comes first). Without
    /// calling this there is nothing that removes the file.
    ///
    /// A replacement of the file between the bind and this call is not told
    /// apart.
    ///
    /// # Errors
    ///
    /// When the spelling cannot be followed (before the bind, after the
    /// clean-up), when what it reaches is not a socket (`ENOTSOCK`), and for
    /// a `CwdRelative` spelling when the cwd is not the one it was resolved
    /// against.
    pub fn socket_file(&self) -> io::Result<SocketFile> {
        let r = route::remember_resolved(&self.path, self.cwd)?;
        Ok(SocketFile::new(r, self.link.clone()))
    }
}

/// A spelling for a connect, from [`resolve_for_connect`]: hand
/// [`ForConnect::path`] to std's connect (or any library) as many times as
/// needed, then [`ForConnect::cleanup`].
#[non_exhaustive]
#[derive(Debug)]
pub struct ForConnect {
    /// The spelling: the path itself when std accepts it, else one through B,
    /// D and E (a symlink to the socket file) until [`ForConnect::cleanup`].
    pub path: PathBuf,
    /// How it was made (`standard`, `link-file`). Diagnostics only.
    pub route: &'static str,
    /// The routes passed over. Diagnostics only.
    pub skipped: Vec<Skipped>,
    link: Option<Arc<base::Link>>,
}

impl ForConnect {
    /// Remove E, the first time only (later calls do nothing).
    ///
    /// # Errors
    ///
    /// Those of `unlink(2)` on E (other than `ENOENT`).
    pub fn cleanup(&self) -> io::Result<()> {
        match &self.link {
            Some(link) => link.cleanup(),
            None => Ok(()),
        }
    }
}

/// A short spelling of `path` for a bind made by std or any other library
/// (hub SPEC "resolve"): `path` itself when std accepts it, else
/// `B/D/E/<file name>` through a symlink to the socket's directory
/// (link-dir), or on macOS and NetBSD `B/D/E` through a symlink to the socket
/// file (link-file). The spelling keeps naming the same place until
/// [`ForBind::cleanup`] (or, spelled through `/proc`, while this process
/// lives); the socket file is removed by [`ForBind::socket_file`].
///
/// ```no_run
/// let path = "/a/very/long/directory/.../server.sock";
/// let spelled = sockat::resolve_for_bind(path, Default::default())?;
/// let listener = std::os::unix::net::UnixListener::bind(&spelled.path)?;
/// let file = spelled.socket_file()?;
/// // ... serve ...
/// file.remove()?; // the socket file and E
/// drop(listener);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// `ENAMETOOLONG` where only thread-cwd / at would reach the path (use
/// [`bind`] / [`bind_ex`] for those), those of looking the socket's directory
/// up (`ENOENT`, `ENOTDIR`), `InvalidInput` as for [`bind`], and those of
/// [`Options::base`].
pub fn resolve_for_bind<P: AsRef<Path>>(path: P, options: Options) -> Result<ForBind, ExError> {
    let r = route::resolve(Use::Bind, path.as_ref(), &options, None)?;
    Ok(ForBind {
        path: r.path,
        spelling: r.spelling,
        route: r.route,
        base: r.base,
        skipped: r.skipped,
        link: r.link,
        cwd: r.cwd,
    })
}

/// A short spelling of `path` to connect to, as many times as needed (hub
/// SPEC "resolve"): `path` itself when std accepts it, else `B/D/E` through a
/// symlink to the socket file, whatever the file name's length. Made once
/// and reused, it saves making a spelling on each connect; clean it up by
/// [`ForConnect::cleanup`]. `usable_cross_uid` is not looked at.
///
/// # Errors
///
/// `ENAMETOOLONG` when no B can be had or the spelling does not fit (use
/// [`connect`], which also tries the routes that need no writing), those of
/// looking the socket's directory up, `InvalidInput` as for [`bind`], and
/// those of [`Options::base`].
pub fn resolve_for_connect<P: AsRef<Path>>(
    path: P,
    options: Options,
) -> Result<ForConnect, ExError> {
    let r = route::resolve(Use::Connect, path.as_ref(), &options, None)?;
    Ok(ForConnect {
        path: r.path,
        route: r.route,
        skipped: r.skipped,
        link: r.link,
    })
}

/// Remove the D whose process is gone (no lock held on it), with their E, in
/// this user's B of the automatic bases (`/tmp/sockat-<uid>`,
/// `$XDG_RUNTIME_DIR/sockat`, `$TMPDIR/sockat-<uid>`) that are there. Only
/// when called: nothing does it on its own. A live process's D, this one's
/// included, is left alone, and no B is made or removed.
///
/// # Errors
///
/// Those of listing a B.
pub fn sweep() -> io::Result<()> {
    base::sweep(&route::Config::from_env().places)
}

/// [`sweep`] of the B at `base` (an [`Options::base`] the application
/// manages).
///
/// # Errors
///
/// Those of [`Options::base`]'s checks (`base` is not made), and of listing
/// it.
pub fn sweep_base<P: AsRef<Path>>(base: P) -> io::Result<()> {
    base::sweep_base(base.as_ref())
}
