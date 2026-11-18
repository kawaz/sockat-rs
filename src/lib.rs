//! Bind and connect Unix domain sockets whose path is longer than `sun_path`
//! allows.
//!
//! `struct sockaddr_un` keeps the socket path in a fixed array: 104 bytes on
//! macOS and the BSDs, 108 on Linux and illumos, the terminating NUL included.
//! std's [`UnixListener::bind`] and [`UnixStream::connect`] refuse anything
//! longer, although the file system has no such limit. Deep temporary
//! directories (macOS's `$TMPDIR` alone is about 50 bytes; test harnesses and
//! sandboxes nest further) run into it easily. The limit is on the argument
//! passed to `bind(2)` / `connect(2)`, so sockat hands the kernel a short
//! spelling that names the same place.
//!
//! Use [`bind`] and [`connect`] where you would use std's, and
//! [`connect_nonblocking`] in async code. They take a path of any length and
//! return std's own types. A path of at most 100 bytes is handed to std
//! unchanged, so short paths behave exactly as with std. For a longer path the
//! socket file is still created at (or looked up at) the path given.
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
//! # How a long path is reached
//!
//! A path longer than 100 bytes is tried on these routes, in this order per
//! platform, and bound / connected on the first route that can be used:
//!
//! | Platform | Order tried |
//! |---|---|
//! | Linux, Android (with `/proc`) | /proc |
//! | Linux, Android (without `/proc`) | relative path, symlink, fork |
//! | FreeBSD | bindat |
//! | macOS (Apple) | relative path, widened sockaddr, symlink, fork |
//! | NetBSD, OpenBSD, illumos, other Unix | relative path, symlink, fork |
//!
//! * **/proc**: `/proc/self/fd/<dirfd>/<name>`, with the directory opened
//!   `O_PATH`. The directory may be of any length; the whole spelling must fit
//!   `sun_path` (a file name of about 90 bytes)
//! * **bindat**: `bindat(2)` / `connectat(2)` with the directory fd and the file
//!   name (up to 103 bytes)
//! * **relative path**: the path spelled relative to the working directory,
//!   when that is at most 100 bytes
//! * **widened sockaddr**: a `sockaddr_un` carrying up to 253 bytes of path,
//!   which XNU accepts: the path itself, or failing that the relative spelling
//! * **symlink**: a short symlink to the socket's directory, under `TMPDIR` or
//!   failing that `/tmp`, removed right after the call. It is created only in a
//!   place where no other user can replace it before the bind / connect through
//!   it: a sticky directory owned by root or the calling user (as `/tmp` is), or
//!   a directory of the calling user's that group and others cannot write to.
//!   Through `/tmp`, a file name of up to 77 bytes (230 on macOS) is reached
//!   at any directory depth
//! * **fork**: a forked child that `fchdir`s into the socket's directory makes
//!   the call by the file name alone and exits. Only [`bind`],
//!   [`connect_any`] and [`connect_any_nonblocking`] take it
//!
//! None of these changes the working directory of the calling process, and all
//! of them can be used from multi-threaded programs. Windows is not supported
//! (the crate is Unix-only). The details, and where each function stops
//! reaching a path, are in the
//! [manual](https://github.com/kawaz/sockat/blob/main/docs/MANUAL.md); the
//! design and the reasons for the order in
//! [DR-0001](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0001-long-socket-path-routes.md)
//! (in Japanese).
//!
//! # The exception: `connect_any`
//!
//! [`connect`] and [`connect_nonblocking`] never fork: a connect is not
//! expected to, and after a connect through the fork route the peer pid the
//! server sees is that of a process that has already exited. Where only the
//! fork route would reach the path, they fail with `ENAMETOOLONG`, as any
//! connect to a path too long for `sun_path` does. That happens only on a
//! platform without `/proc` or `bindat`, when the path fits neither as a
//! relative path (nor, on macOS, in the widened sockaddr) nor through the
//! symlink (the file name is too long for it, or there is no safe place for it).
//!
//! [`connect_any`] and [`connect_any_nonblocking`] are for that rare
//! environment, when the connection is wanted at the cost of the fork's side
//! effects. They take the fork route as the last resort and also return whether
//! they did, so that the caller can, for example, log or warn about it.
//! [`bind`] takes the fork route as a last resort too; the child only binds and
//! the listen is done by the calling process, so the listener is this
//! process's like any other.
//!
//! # What differs from std for a long path
//!
//! * **The address the kernel reports is not the path.** `local_addr()` of the
//!   listener and of accepted streams, and `peer_addr()` of a connected client,
//!   report the spelling handed to the kernel when the listener was bound
//!   (whichever route the client took): a relative path, a
//!   `/proc/self/fd/...` path, a path through a temporary symlink that no
//!   longer exists, or a bare file name, and on macOS possibly truncated. Keep
//!   the path you passed and use that (for example to remove the socket file
//!   with [`std::fs::remove_file`], which has no length limit).
//! * **A path that does not end with a file name is `InvalidInput`.** A path
//!   longer than 100 bytes ending in `/`, `/.` or `/..` (or being `..`) cannot
//!   be split into a directory and a file name and is refused before anything
//!   is created; a short one goes to std and gets the kernel's error.
//! * **The peer pid the server sees can be that of an exited process**
//!   ([`connect_any`] and [`connect_any_nonblocking`], when they take the fork
//!   route): the forked child that made the connect and exited. The socket
//!   itself belongs to the caller and works as usual. The uid and gid the
//!   server sees are the caller's. On macOS (`LOCAL_PEERPID`) the server sees
//!   the caller's pid once the caller sends on the socket; on Linux
//!   (`SO_PEERCRED`) it stays the exited child's. The number of an exited pid
//!   may be reused by another process later. Nothing can be looked up from
//!   that pid (the process, its parent): the process is gone.
//! * **The forked child briefly holds every fd of the process** (fork route).
//!   As with any `fork(2)`, the child has copies of all file descriptors until
//!   it exits a moment later, so a socket or pipe another thread closes during
//!   that moment stays open until then (a peer sees the EOF slightly later; a
//!   listener closed meanwhile can still accept a connection into its backlog).
//!   On macOS the fork route opens the socket's directory `O_SEARCH` and on
//!   Linux `O_PATH`, so it needs no more permission than a plain bind /
//!   connect; on other Unix targets it (and FreeBSD's `bindat`) opens it
//!   `O_RDONLY` and needs read permission on the directory as well.
//! * **The directory is resolved slightly earlier than the call.** The /proc,
//!   relative, `bindat` and fork routes look the directory up first (open it, or
//!   resolve it to a real path) and then bind / connect by the file name. If a
//!   directory along the path is renamed or a symlink along it is repointed in
//!   between, the socket goes to (or is looked up in) the directory as it was
//!   when looked up, not as it is at the moment of the call.
//! * **The relative route depends on the working directory.** The path is
//!   spelled relative to the cwd only after checking that the spelling names
//!   the same directory, and the cwd is checked again afterwards; if another
//!   thread changed it in between, the result is discarded and an error is
//!   returned. Programs that change the cwd while other threads bind / connect
//!   are racing every relative path in the process anyway; sockat does not
//!   make that safe, it only detects the visible case.
//! * **A temporary symlink can be left behind** if the process dies between
//!   creating and removing it. It is named `sockat.XXXXXXXXXX` under `TMPDIR`
//!   or `/tmp` and points to the socket's directory.
//!
//! # Errors
//!
//! The errors are those of `bind(2)`, `listen(2)` and `connect(2)` (and of
//! opening the directory) as [`std::io::Error`]s, for example `ENOENT` for a
//! missing directory or socket, `EADDRINUSE` for an existing file,
//! `ECONNREFUSED` for a socket nobody listens on, `EAGAIN` for a full backlog
//! on a nonblocking connect (Linux; macOS answers `ECONNREFUSED` to any
//! connect on a full backlog, so there `ECONNREFUSED` alone does not mean
//! nobody listens). When no route can reach the path, which happens when the
//! file name alone is too long (or, for [`connect`] and
//! [`connect_nonblocking`], when only the fork route would), the error is
//! `ENAMETOOLONG`. A path longer than 100 bytes that does not end with a file
//! name, or any path that contains a NUL byte, is an `InvalidInput` error.
//!
//! # Async runtimes
//!
//! The results are std types, so any runtime that adopts std sockets can take
//! them. With tokio:
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
//!
//! [`connect_nonblocking`] does not wait for the peer (a Unix-domain connect
//! completes or fails within the call) and returns the stream still
//! nonblocking, so it can be called on the runtime's worker threads directly.
//! [`bind`] does not wait for a peer either.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(not(unix))]
compile_error!("sockat supports Unix targets only (Windows is out of scope, see DR-0001)");

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use route::ConnectOptions;

mod path;
mod route;
mod sys;

#[cfg(test)]
mod tests;

/// Create a socket bound to `path` and listening on it, like
/// [`UnixListener::bind`] but for a path of any length.
///
/// A path of at most 100 bytes goes to [`UnixListener::bind`] unchanged. A
/// longer path is reached by the routes the platform has, the fork route as
/// the last resort (see the [crate documentation](crate)): the forked child
/// only binds, and the listen is done by this process, so the listener is this
/// process's like any other. The socket file is created at `path` either way,
/// and is not removed when the listener is dropped (as with std).
///
/// # Errors
///
/// The errors of `bind(2)` / `listen(2)`, for example `EADDRINUSE` when `path`
/// exists and `ENOENT` when its directory does not; `ENAMETOOLONG` when the file
/// name alone is too long for any route; `InvalidInput` for a path longer than
/// 100 bytes that does not end with a file name, or for a path that contains a
/// NUL byte.
pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
    route::run(route::Request::Bind, path.as_ref()).map(|(fd, _)| UnixListener::from(fd))
}

/// Connect to the socket at `path`, like [`UnixStream::connect`] but for a path
/// of any length, without forking. Use it wherever std's would be used.
///
/// A path of at most 100 bytes goes to [`UnixStream::connect`] unchanged. A
/// longer path is reached by every route the platform has but the fork route
/// (see the [crate documentation](crate)), so in an ordinary environment it is
/// reached at any directory depth, and the connection is always made by this
/// process. Only in a rare environment where nothing but the fork route would
/// reach the path is the error `ENAMETOOLONG`, as for any connect to a path
/// too long for `sun_path` ([`connect_any`] is for that case).
///
/// The stream blocks, like std's. In async code use [`connect_nonblocking`].
///
/// # Errors
///
/// The errors of `connect(2)`, for example `ENOENT` when nothing is at `path`
/// and `ECONNREFUSED` when nobody listens on it (on macOS also when its backlog
/// is full); `ENAMETOOLONG` when no route but the fork route reaches the path
/// (or none at all does); `InvalidInput` for a path longer than 100 bytes that
/// does not end with a file name, or for a path that contains a NUL byte.
pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
    connect_with(
        path.as_ref(),
        ConnectOptions {
            nonblocking: false,
            fork: false,
        },
    )
    .map(|(stream, _)| stream)
}

/// [`connect`] on a nonblocking socket: the connect does not wait, and the
/// stream it returns is still nonblocking (ready for
/// `tokio::net::UnixStream::from_std` as it is).
///
/// A Unix-domain connect completes or fails within the call, so there is
/// nothing to wait for afterwards. When the listener's backlog is full the
/// connect fails at once: `EAGAIN` on Linux (where a blocking connect waits for
/// room), `ECONNREFUSED` on macOS (as a blocking connect does).
///
/// # Errors
///
/// Those of [`connect`], and `EAGAIN` for a full backlog on Linux.
pub fn connect_nonblocking<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
    connect_with(
        path.as_ref(),
        ConnectOptions {
            nonblocking: true,
            fork: false,
        },
    )
    .map(|(stream, _)| stream)
}

/// [`connect`], forking when nothing else reaches the path, and whether it
/// did.
///
/// Use [`connect`] (or [`connect_nonblocking`]) unless it fails with
/// `ENAMETOOLONG` in an environment you have to support: it reaches a long
/// path in any ordinary environment. This is for the rare one where only the
/// fork route does (on a platform without `/proc` or `bindat`, a path that fits
/// neither as a relative path, nor on macOS in the widened sockaddr, nor
/// through the symlink, because the file name is too long for it or there is no
/// safe place for it), when the connection is wanted at the cost of the fork's
/// side effects. The order tried is that of [`connect`], with the fork route
/// last. Linux with `/proc` and FreeBSD never fork.
///
/// The fork route connects from a forked child that `fchdir`s into the
/// socket's directory and exits at once. The second value is `true` when the
/// fork route was taken, and `false` on every other route (the connection was
/// made by this process). Use it to note the fork's side effects, for example
/// by logging or warning about it.
///
/// After the fork route, the peer pid the server sees is that of the child, a
/// process that has already exited. The stream itself belongs to this process
/// and works as usual. The uid and gid the server sees are this process's. On
/// macOS (`LOCAL_PEERPID`) the server sees this process's pid once it sends on
/// the stream; on Linux (`SO_PEERCRED`) it stays the exited child's. Nothing
/// can be looked up from that pid (the process, its parent): the process is
/// gone, and the number may be reused by another process later. So the pid
/// itself is of no use to this process or to the server, and is not returned.
///
/// # Errors
///
/// Those of [`connect`], except that a path only the fork route reaches is
/// connected rather than `ENAMETOOLONG`.
pub fn connect_any<P: AsRef<Path>>(path: P) -> io::Result<(UnixStream, bool)> {
    connect_with(
        path.as_ref(),
        ConnectOptions {
            nonblocking: false,
            fork: true,
        },
    )
}

/// [`connect_any`] on a nonblocking socket, as [`connect_nonblocking`] is to
/// [`connect`]. Like [`connect_any`], only for the rare environment where
/// [`connect_nonblocking`] cannot reach the path. On the fork route the socket
/// is made nonblocking before the fork, so the child's connect does not wait
/// either.
///
/// # Errors
///
/// Those of [`connect_any`], and `EAGAIN` for a full backlog on Linux.
pub fn connect_any_nonblocking<P: AsRef<Path>>(path: P) -> io::Result<(UnixStream, bool)> {
    connect_with(
        path.as_ref(),
        ConnectOptions {
            nonblocking: true,
            fork: true,
        },
    )
}

/// The four connects: block or not, fork or not ([`ConnectOptions`]). Returns
/// the stream and whether the fork route was taken (always `false` without
/// `fork`).
pub(crate) fn connect_with(path: &Path, options: ConnectOptions) -> io::Result<(UnixStream, bool)> {
    route::run(route::Request::Connect(options), path)
        .map(|(fd, route)| (UnixStream::from(fd), route.forked()))
}
