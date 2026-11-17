//! Bind and connect Unix domain sockets whose path is longer than `sun_path`
//! allows.
//!
//! `struct sockaddr_un` keeps the socket path in a fixed array: 104 bytes on
//! macOS and the BSDs, 108 on Linux, the terminating NUL included. std's
//! [`UnixListener::bind`] and [`UnixStream::connect`] refuse anything longer,
//! although the file system has no such limit. Deep temporary directories
//! (macOS's `$TMPDIR` alone is about 50 bytes; test harnesses and sandboxes nest
//! further) run into it easily.
//!
//! [`bind`] and [`connect`] take a path of any length. A path of at most 100
//! bytes is handed to std unchanged, so short paths behave exactly as with std.
//! A longer path is reached another way, chosen per platform; the socket file is
//! always created at (or looked up at) the path given.
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
//! | Platform | Way |
//! |---|---|
//! | Linux, Android | `/proc/self/fd/<dirfd>/<name>`, with the directory opened `O_PATH` |
//! | macOS (Apple) | in order: the path relative to the cwd; a widened `sockaddr_un` carrying up to 253 bytes; a short symlink to the directory under `${TMPDIR:-/tmp}`; a forked child that `fchdir`s into the directory |
//! | FreeBSD | `bindat(2)` / `connectat(2)` with the directory fd |
//! | other Unix | a forked child that `fchdir`s into the directory |
//!
//! None of these changes the working directory of the calling process, and all
//! of them can be used from multi-threaded programs. Windows is not supported
//! (the crate is Unix-only). The design and the reasons
//! for the order are in `docs/decisions/DR-0001` of the repository.
//!
//! # What differs from std for a long path
//!
//! * **The address the kernel reports is not the path.** `local_addr()` of the
//!   listener and of accepted streams, and `peer_addr()` of a connected client,
//!   report what was actually handed to the kernel: a relative path, a
//!   `/proc/self/fd/...` path, a temporary symlink that no longer exists, or a
//!   bare file name, and on macOS possibly truncated. Keep the path you passed
//!   to [`bind`] / [`connect`] and use that (for example to remove the socket
//!   file with [`std::fs::remove_file`], which has no length limit).
//! * **The peer process may be a short-lived child** (macOS and other Unix,
//!   when the fork route is taken). When a client connects through the fork
//!   route, the server's peer-credential queries can report the child's pid: on
//!   macOS `LOCAL_PEERPID` reports the child until the client process itself
//!   sends on the socket, then the client's pid. The uid and gid are the
//!   client's either way. Linux never forks (its `SO_PEERCRED` would keep the
//!   child's pid forever).
//! * **The directory is resolved slightly earlier than the call.** The /proc,
//!   relative, `bindat` and fork routes look the directory up first (open it, or
//!   resolve it to a real path) and then bind / connect by the file name. If a
//!   directory along the path is renamed or a symlink along it is repointed in
//!   between, the socket goes to (or is looked up in) the directory as it was
//!   when looked up, not as it is at the moment of the call.
//! * **The relative route depends on the working directory** (macOS). The path
//!   is spelled relative to the cwd only after checking that the spelling names
//!   the same directory, and the cwd is checked again afterwards; if another
//!   thread changed it in between, the result is discarded and an error is
//!   returned. Programs that change the cwd while other threads bind / connect
//!   are racing every relative path in the process anyway; sockat does not
//!   make that safe, it only detects the visible case.
//! * **The forked child briefly holds every fd of the process** (fork route).
//!   As with any `fork(2)`, the child has copies of all file descriptors until
//!   it exits a moment later, so a socket or pipe another thread closes during
//!   that moment stays open until then (a peer sees the EOF slightly later; a
//!   listener closed meanwhile can still accept a connection into its backlog).
//!   On macOS the fork route opens the socket's directory `O_SEARCH`, so it
//!   needs no more permission than a plain bind / connect; on other Unix
//!   targets it (and FreeBSD's `bindat`) opens it `O_RDONLY` and needs read
//!   permission on the directory as well.
//! * **A temporary symlink can be left behind** (macOS) if the process dies
//!   between creating and removing it. It is named `sockat.XXXXXXXXXX` under
//!   `${TMPDIR:-/tmp}` and points to the socket's directory.
//!
//! # Errors
//!
//! The errors are those of `bind(2)`, `listen(2)` and `connect(2)` (and of
//! opening the directory) as [`std::io::Error`]s, for example `ENOENT` for a
//! missing directory or socket, `EADDRINUSE` for an existing file,
//! `ECONNREFUSED` for a socket nobody listens on. When no way can reach the
//! path, which happens when the file name alone is too long, the error is
//! `ENAMETOOLONG`. A path that does not end with a file name (`dir/`, `dir/.`,
//! `..`) or that contains a NUL byte is an `InvalidInput` error.
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
//! let stream = sockat::connect(&path)?;
//! stream.set_nonblocking(true)?;
//! let stream = tokio::net::UnixStream::from_std(stream)?;
//! # Ok(())
//! # }
//! ```
//!
//! [`bind`] and [`connect`] block like std's. A local `connect(2)` returns at
//! once unless the listener's backlog is full; to keep even that off the
//! runtime's worker threads, call [`connect`] inside `spawn_blocking`.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(not(unix))]
compile_error!("sockat supports Unix targets only (Windows is out of scope, see DR-0001)");

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

mod path;
mod route;
mod sys;

#[cfg(test)]
mod tests;

/// Create a socket bound to `path` and listening on it, like
/// [`UnixListener::bind`] but for a path of any length.
///
/// A path of at most 100 bytes goes to [`UnixListener::bind`] unchanged. The
/// socket file is created at `path` either way, and is not removed when the
/// listener is dropped (as with std). See the [crate documentation](crate) for
/// what differs for a long path.
///
/// # Errors
///
/// The errors of `bind(2)` / `listen(2)`, for example `EADDRINUSE` when `path`
/// exists and `ENOENT` when its directory does not; `ENAMETOOLONG` when the file
/// name alone is too long to reach; `InvalidInput` for a path that does not end
/// with a file name or contains a NUL byte.
pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
    route::run(sys::Op::Bind, path.as_ref()).map(|(fd, _)| UnixListener::from(fd))
}

/// Connect to the socket at `path`, like [`UnixStream::connect`] but for a path
/// of any length.
///
/// A path of at most 100 bytes goes to [`UnixStream::connect`] unchanged. See
/// the [crate documentation](crate) for what differs for a long path.
///
/// # Errors
///
/// The errors of `connect(2)`, for example `ENOENT` when nothing is at `path`
/// and `ECONNREFUSED` when nobody listens on it; `ENAMETOOLONG` when the file
/// name alone is too long to reach; `InvalidInput` for a path that does not end
/// with a file name or contains a NUL byte.
pub fn connect<P: AsRef<Path>>(path: P) -> io::Result<UnixStream> {
    route::run(sys::Op::Connect, path.as_ref()).map(|(fd, _)| UnixStream::from(fd))
}
