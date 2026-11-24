//! Bind and connect Unix domain sockets whose path is longer than `sun_path`
//! allows (104 bytes on macOS and the BSDs, 108 on Linux and illumos, the
//! terminating NUL included).
//!
//! Use [`bind`] and [`connect`] where you would use std's
//! [`UnixListener::bind`] and [`UnixStream::connect`], and
//! [`connect_nonblocking`] in async code; [`datagram`] has the same for
//! `SOCK_DGRAM`. They take a path of any length and return std's own types. An
//! absolute path std accepts goes to std unchanged, so it behaves exactly as
//! with std:
//!
//! | Platform | `sun_path` (NUL included) | Handed to std |
//! |---|---|---|
//! | Linux, Android, illumos | 108 | up to 107 bytes |
//! | macOS, iOS, FreeBSD, NetBSD, OpenBSD | 104 | up to 103 bytes |
//!
//! A longer path is handed to the kernel by a short spelling that names the
//! same place. A bind (and a resolve) makes a relative path absolute first,
//! so the address the kernel keeps does not name another place once the cwd
//! changes; a connect hands a relative path on as it is.
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
//! The details, with examples, are in the
//! [manual](https://github.com/kawaz/sockat-rs/blob/main/docs/MANUAL.md); the
//! language-independent behaviour is the
//! [specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md),
//! with how a long path is reached in
//! [Routes for long paths and their order](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#routes-for-long-paths-and-their-order).
//!
//! # Things to know
//!
//! * A bind at a long path leaves a small directory and a symlink in
//!   `/tmp/sockat-<uid>` or the like. They go with the OS's cleaning of
//!   temporary directories, or by [`sweep`] ([`sweep_base`] for a B at
//!   [`Options::base`]). See
//!   [What stays in the base](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-stays-in-the-base)
//!   and [`sweep`](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#sweep).
//! * For a long path, the address the kernel reports (`local_addr`,
//!   `peer_addr`, `recv_from`) is the short spelling, not the original path;
//!   for a relative path bound, an absolute one.
//!   Remove the socket file by the original path or by
//!   [`SocketFile::remove`]. See
//!   [What differs from the standard library for a long path](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-differs-from-the-standard-library-for-a-long-path).
//! * To remove the socket file later, or to know what the spelling is good
//!   for ([`Spelling`]) and which route was taken, use the `_ex` functions
//!   ([`bind_ex`], [`connect_ex`], ...). To hand a spelling to std yourself,
//!   use [`resolve_for_bind`] / [`resolve_for_connect`].
//!
//! # Patterns not to use
//!
//! The patterns and why they are wrong are in the specification's
//! [Patterns not to use](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#patterns-not-to-use).
//! What to do instead in Rust, run as a test:
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
//! # let dir = tempfile::Builder::new().prefix("srt-").tempdir_in("/tmp")?;
//! # let deep = dir.path().join("d".repeat(150));
//! # std::fs::create_dir(&deep)?;
//! # serve(&deep.join("s.sock"))?;
//! # assert_eq!(std::fs::read_dir(&deep)?.count(), 0);
//! # Ok::<(), io::Error>(())
//! ```
//!
//! # Errors
//!
//! OS errors come as [`std::io::Error`]s with their errno; an argument error
//! is `InvalidInput`. The `_ex` functions and resolve return an [`ExError`],
//! which converts into the `io::Error` by `?`. Which error for which
//! situation is in the specification's
//! [Errors](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#errors).
//!
//! # Async runtimes
//!
//! The results are std types, so any runtime that adopts std sockets takes
//! them. [`connect_nonblocking`] does not wait and returns the stream
//! nonblocking:
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use route::{Connect, Request, Use};
use sys::SockType;

mod base;
pub mod datagram;
mod path;
mod route;
mod sys;

#[cfg(test)]
mod tests;

// Design rationale: the `sockat` command (another package, `sockat-cli`) makes
// and checks B as this library does, and a package cannot see `pub(crate)`.
// Rather than copy B's making and checking into the command, or add it to
// the API, this entry is public only behind a feature off by default and
// hidden from the documentation: it is not part of the API and promises
// nothing (DR-0033).
#[cfg(feature = "__cli-internal")]
#[doc(hidden)]
#[path = "cli_internal.rs"]
pub mod __cli;

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
    /// the check fails; it must be absolute, hold no NUL and end with a name (else
    /// `InvalidInput`, whatever the length of the path). The check is
    /// made before any route is tried, for a long path only; B is made only
    /// when a route that uses it is reached. Used for the binds and the
    /// connects. B is sockat's own: put nothing else in it ([`sweep_base`]
    /// looks only at directories named in the form of a D).
    pub base: Option<PathBuf>,
}

/// What the spelling of a bind (the address the kernel keeps and reports by
/// `getsockname` / `getpeername`) or of a resolve is good for, later.
///
/// `#[non_exhaustive]`: read a kind you do not know as "do not rely on the
/// spelling".
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Spelling {
    /// The absolute path itself, or an absolute path naming the same place
    /// (a relative path made absolute, a long path's real path) that std
    /// takes.
    Original,
    /// The relative path itself: after the cwd changes it names another
    /// place. Only when no absolute spelling could be had (a cwd so deep
    /// that a relative path made absolute fits nowhere, and no route through
    /// B can be used).
    CwdRelative,
    /// Through B, D and E, naming the same place as the path while it can be
    /// followed.
    ///
    /// `#[non_exhaustive]`: more attributes may come, so match it with `..`
    /// (`Spelling::Linked { removable, .. }`).
    #[non_exhaustive]
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
/// When the last component of the path is a symlink, which a bind follows on
/// macOS and NetBSD (making the socket where it points), the file is the
/// socket the symlink led to: removing it leaves the symlink.
///
/// Dropping it does nothing: the file is removed only by
/// [`SocketFile::remove`], E only by it or [`SocketFile::cleanup`].
#[derive(Debug)]
pub struct SocketFile {
    /// The socket file's real path and `(st_dev, st_ino)`; none for a name
    /// in the abstract namespace, which is no file.
    file: Option<route::Remembered>,
    clean: Arc<Clean>,
    /// Whether this file has been removed (read and set under `clean`'s
    /// lock).
    removed: AtomicBool,
}

/// What the holders of one spelling share for its clean-up: the E it goes
/// through (none for a spelling through no E), and the lock that every
/// removal of the socket file and of E takes, so that they are one at a time
/// across the [`ForBind`] (or [`ForConnect`]) and every [`SocketFile`] of the
/// spelling (hub SPEC "remove と cleanup"). Two removals of the file at once
/// could otherwise both find it still the one bound, and the second remove a
/// socket bound at the path in between.
#[derive(Debug)]
struct Clean {
    link: Option<Arc<base::Link>>,
    lock: Mutex<()>,
    /// Test-only: how many times a [`SocketFile::remove`] has unlinked the
    /// socket file.
    #[cfg(test)]
    file_unlinks: std::sync::atomic::AtomicU64,
}

impl Clean {
    fn new(link: Option<Arc<base::Link>>) -> Arc<Self> {
        Arc::new(Self {
            link,
            lock: Mutex::new(()),
            #[cfg(test)]
            file_unlinks: Default::default(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Remove E (once it is removed, nothing), the lock held by the caller.
    fn cleanup_locked(&self) -> io::Result<()> {
        match &self.link {
            Some(link) => link.cleanup(),
            None => Ok(()),
        }
    }

    fn cleanup(&self) -> io::Result<()> {
        let _held = self.lock();
        self.cleanup_locked()
    }
}

impl SocketFile {
    fn new(kept: route::Kept, clean: Arc<Clean>) -> Self {
        Self {
            file: match kept {
                route::Kept::File(r) => Some(r),
                route::Kept::Abstract => None,
            },
            clean,
            removed: AtomicBool::new(false),
        }
    }

    /// The socket file's real path (test-only).
    #[cfg(test)]
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.file.as_ref().expect("a socket file").path
    }

    /// Remove the socket file if what is at its real path now is still the
    /// file the bind made (the same `(st_dev, st_ino)`, looked up without
    /// following a symlink), and clean up E ([`SocketFile::cleanup`]). Once
    /// that has succeeded, later calls do nothing; calls at once remove one
    /// at a time.
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
    /// For a name in the abstract namespace (Linux / Android, a path whose
    /// first byte is NUL) there is no file: this removes none, and only
    /// cleans up as [`SocketFile::cleanup`] does.
    ///
    /// # Errors
    ///
    /// Those of `lstat(2)` (other than `ENOENT` and `ENOTDIR`) and `unlink(2)`
    /// on the path; when the file was removed, those of removing E
    /// ([`SocketFile::cleanup`], which is done either way). A call after an
    /// error tries again.
    pub fn remove(&self) -> io::Result<()> {
        let _held = self.clean.lock();
        if self.removed.load(Ordering::Relaxed) {
            return Ok(());
        }
        let gone = |e: &io::Error| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
        let removed = match &self.file {
            None => Ok(()),
            Some(file) => match file_id(&file.path) {
                Ok(id) if id == file.id => {
                    #[cfg(test)]
                    self.clean.file_unlinks.fetch_add(1, Ordering::Relaxed);
                    match std::fs::remove_file(&file.path) {
                        Err(e) if gone(&e) => Ok(()),
                        removed => removed,
                    }
                }
                Ok(_) => Ok(()),
                Err(e) if gone(&e) => Ok(()),
                Err(e) => Err(e),
            },
        };
        // E is cleaned up even when the file could not be removed; the
        // file's error is the one returned (hub SPEC "remove").
        let cleaned = self.clean.cleanup_locked();
        removed?;
        cleaned?;
        self.removed.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Remove the E the spelling goes through; once it is removed, later
    /// calls do nothing. The spelling names nothing afterwards; the socket
    /// file and the socket stay. Nothing to do for a spelling through no E.
    ///
    /// # Errors
    ///
    /// Those of `unlink(2)` on E (other than `ENOENT`); a call after an error
    /// tries again.
    pub fn cleanup(&self) -> io::Result<()> {
        self.clean.cleanup()
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
    bind_by(ty, path, options, remember, None, None)
}

/// [`bind_with`] with the route order's inputs `cfg` (from the environment
/// when `None`) and the routes `names` to take in that order (the order of
/// the target when `None`; [`route::run_by`]).
pub(crate) fn bind_by(
    ty: SockType,
    path: &Path,
    options: &Options,
    remember: bool,
    cfg: Option<&route::Config>,
    names: Option<&[&'static str]>,
) -> Result<(OwnedFd, Option<SocketFile>, route::Reached), ExError> {
    let mut r = route::run_by(Request::Bind(ty), path, options, remember, cfg, names)?;
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
    let file = r
        .file
        .take()
        .map(|f| SocketFile::new(f, Clean::new(r.link.clone())));
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
/// An absolute path that std accepts goes to [`UnixListener::bind`]
/// unchanged. A relative path is made absolute first: by the real path of its
/// directory or by `$PWD`, whichever fits std (checked to name the same
/// directory), else through the routes as a long path; only when none of
/// those can be used does it go to std as it is. A longer path is bound by
/// the routes of the platform (see the [crate documentation](crate)), or by
/// its real path when that fits std; the socket file is created at `path`
/// either way,
/// and is not removed when the listener is dropped (as with std). The E its
/// spelling goes through stays until the process ends; through link-dir it is
/// one per directory, shared by every `bind` and [`datagram::bind`] there
/// ([`bind_ex`] returns a [`SocketFile`] that removes the file and an E of
/// its own).
///
/// # Errors
///
/// The errors of `bind(2)` / `listen(2)`, for example `EADDRINUSE` when `path`
/// exists and `ENOENT` when its directory does not; `ENAMETOOLONG` when no
/// route reaches the path; `InvalidInput` for an empty path (on Linux std's
/// bind would autobind it to an unnamed abstract address), for a path longer
/// than std accepts that does not end with a file name, or for a path that
/// contains a NUL byte.
pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
    let (socket, _, _) = bind_with(SockType::Stream, path.as_ref(), &Options::default(), false)?;
    Ok(UnixListener::from(socket))
}

/// Connect to the socket at `path`, like [`UnixStream::connect`] but for a
/// path of any length. Nothing is left behind. A path std accepts, relative
/// or not, goes to std unchanged.
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
    connect_by(path, nonblocking, options, None, None)
}

/// [`connect_with`] with `cfg` and `names` as in [`bind_by`].
pub(crate) fn connect_by(
    path: &Path,
    nonblocking: bool,
    options: &Options,
    cfg: Option<&route::Config>,
    names: Option<&[&'static str]>,
) -> Result<Connected<UnixStream>, ExError> {
    let req = Request::Connect(Connect::Stream { nonblocking });
    let r = route::run_by(req, path, options, false, cfg, names)?;
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
/// the bind, before this returns: when the last component of `path` is a
/// symlink (followed by the bind on macOS and NetBSD), those of the socket it
/// led to, so [`SocketFile::remove`] leaves the symlink. What is found there
/// must be a socket (`ENOTSOCK` otherwise: it was replaced, and is not taken).
/// Where it is looked up is hub SPEC "remove"'s: a route that binds through
/// the real path of the directory taken before the bind looks there, not up
/// `path` again, so a symlink on its way switched meanwhile does not move it;
/// the others look up `path` after the bind.
/// A relative `path` is made absolute as by [`bind`], and the file looked up
/// by that. Only when it goes to std as it is ([`Spelling::CwdRelative`])
/// must the cwd's `(st_dev, st_ino)` be the same before anything is looked
/// up and after the file is (another thread changing it would make the file
/// looked up another one); otherwise the listener is closed and an error
/// returned (the socket file stays, and so does nothing else).
/// A listener short of a [`SocketFile`] that could remove the file is never
/// handed out. Unlike std's and [`bind`]'s, nothing is removed when the
/// listener is closed even for a short path: remove by
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
    /// The spelling: an absolute path std accepts (the path itself, or one
    /// naming the same place), else one through B, D and E that keeps naming
    /// the same place until [`ForBind::cleanup`]; a relative path std
    /// accepts, as it is, only when none of those can be had.
    pub path: PathBuf,
    /// What the spelling is good for once bound.
    pub spelling: Spelling,
    /// How it was made (`standard`, `link-dir`, `link-file`). Diagnostics only.
    pub route: &'static str,
    /// The B it goes through. Diagnostics only.
    pub base: Option<&'static str>,
    /// The routes passed over. Diagnostics only.
    pub skipped: Vec<Skipped>,
    clean: Arc<Clean>,
    cwd: Option<(u64, u64)>,
}

impl ForBind {
    /// Remove E; once it is removed, later calls do nothing: the spelling
    /// names nothing afterwards. Not the socket file: that is
    /// [`ForBind::socket_file`]'s [`SocketFile::remove`].
    ///
    /// # Errors
    ///
    /// Those of `unlink(2)` on E (other than `ENOENT`); a call after an error
    /// tries again.
    pub fn cleanup(&self) -> io::Result<()> {
        self.clean.cleanup()
    }

    /// The [`SocketFile`] of the socket bound by [`ForBind::path`]: call it
    /// after the bind (and before the socket is closed). The spelling is
    /// followed now, and the socket file's real path and `(st_dev, st_ino)`
    /// taken; its [`SocketFile::remove`] removes the file and cleans up the
    /// same E as [`ForBind::cleanup`] (once, whichever comes first). Without
    /// calling this there is nothing that removes the file. Called more than
    /// once, it gives [`SocketFile`]s that remove one at a time with each
    /// other and with the [`ForBind`].
    ///
    /// When the last component of the path is a symlink (which the bind
    /// follows on macOS and NetBSD), the file taken is the socket it led to,
    /// so removing it leaves the symlink. A replacement of the file between
    /// the bind and this call is not told apart.
    ///
    /// # Errors
    ///
    /// When the spelling cannot be followed (before the bind, after the
    /// clean-up), when what it reaches is not a socket (`ENOTSOCK`), and for
    /// a `CwdRelative` spelling when the cwd is not the one it was resolved
    /// against.
    pub fn socket_file(&self) -> io::Result<SocketFile> {
        let r = route::remember_resolved(&self.path, self.spelling, self.cwd)?;
        Ok(SocketFile::new(r, Arc::clone(&self.clean)))
    }
}

/// A spelling for a connect, from [`resolve_for_connect`]: hand
/// [`ForConnect::path`] to std's connect (or any library) as many times as
/// needed, then [`ForConnect::cleanup`].
#[non_exhaustive]
#[derive(Debug)]
pub struct ForConnect {
    /// The spelling: an absolute path std accepts (the path itself, or one
    /// naming the same place), else one through B, D and E (a symlink to the
    /// socket file) until [`ForConnect::cleanup`]; a relative path std
    /// accepts, as it is, only when none of those can be had.
    pub path: PathBuf,
    /// What the spelling is good for, kept and used later: `Original`,
    /// `CwdRelative` (it names another place once the cwd changes), or
    /// `Linked` (`removable` is `false`; spelled through `/proc`, `until` is
    /// [`Until::Process`]).
    pub spelling: Spelling,
    /// How it was made (`standard`, `link-file`). Diagnostics only.
    pub route: &'static str,
    /// The routes passed over. Diagnostics only.
    pub skipped: Vec<Skipped>,
    clean: Arc<Clean>,
}

impl ForConnect {
    /// Remove E; once it is removed, later calls do nothing.
    ///
    /// # Errors
    ///
    /// Those of `unlink(2)` on E (other than `ENOENT`); a call after an error
    /// tries again.
    pub fn cleanup(&self) -> io::Result<()> {
        self.clean.cleanup()
    }
}

/// A short spelling of `path` for a bind made by std or any other library
/// (hub SPEC "resolve"): `path` itself when it is absolute and std accepts
/// it, an absolute path naming the same place when std accepts that (a
/// relative path made absolute as by [`bind`], a long path's real path),
/// else `B/D/E/<file name>` through a symlink to the socket's directory
/// (link-dir), or on macOS and NetBSD `B/D/E` through a symlink to the socket
/// file (link-file). The spelling keeps naming the same place until
/// [`ForBind::cleanup`] (or, spelled through `/proc`, while this process
/// lives); the socket file is removed by [`ForBind::socket_file`]. A
/// relative path std accepts comes back as it is
/// ([`Spelling::CwdRelative`]) only when none of those can be had.
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
        clean: Clean::new(r.link),
        cwd: r.cwd,
    })
}

/// A short spelling of `path` to connect to, as many times as needed (hub
/// SPEC "resolve"): `path` itself when it is absolute and std accepts it, an
/// absolute path naming the same place when std accepts that (as for
/// [`resolve_for_bind`]: the spelling may be used after the cwd changes),
/// else `B/D/E` through a symlink to the socket file, whatever the file
/// name's length; a relative path std accepts comes back as it is only when
/// none of those can be had. [`ForConnect::spelling`] tells which. Made once
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
        spelling: r.spelling,
        route: r.route,
        skipped: r.skipped,
        clean: Clean::new(r.link),
    })
}

/// Remove the D whose process is gone (no lock held on it), with their E, in
/// this user's B of the automatic bases (`/tmp/sockat-<uid>`,
/// `$XDG_RUNTIME_DIR/sockat`, `$TMPDIR/sockat-<uid>`) that are there. Only
/// when called: nothing does it on its own. A live process's D, this one's
/// included, is left alone, and no B is made or removed. Only the
/// directories right under B named in the form of a D (10 ASCII letters and
/// digits) are looked at; anything else in B is left alone.
///
/// # Errors
///
/// Those of listing a B.
pub fn sweep() -> io::Result<()> {
    base::sweep(&route::Config::from_env().places)
}

/// [`sweep`] of the B at `base` (an [`Options::base`] the application
/// manages). When `base` or its parent is not there, there is nothing to
/// sweep, and this succeeds without making anything.
///
/// # Errors
///
/// Those of [`Options::base`]'s checks other than its not being there (`base`
/// is not made), and of listing it.
pub fn sweep_base<P: AsRef<Path>>(base: P) -> io::Result<()> {
    base::sweep_base(base.as_ref())
}
