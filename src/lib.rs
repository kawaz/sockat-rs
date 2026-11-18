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
//! return std's own types. A path that std accepts is handed to std
//! unchanged, so it behaves exactly as with std:
//!
//! | Platform | `sun_path` (NUL included) | Handed to std |
//! |---|---|---|
//! | Linux, Android, illumos | 108 | up to 107 bytes |
//! | macOS, iOS, FreeBSD, NetBSD, OpenBSD | 104 | up to 103 bytes |
//!
//! For a longer path the socket file is still created at (or looked up at) the
//! path given.
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
//! A path longer than std accepts goes first through a directory of sockat's
//! own (**the central route**), and only when that cannot be used, for a
//! connect, through **the fallback routes**:
//!
//! | Platform | [`bind`] | [`connect`], [`connect_nonblocking`] |
//! |---|---|---|
//! | Linux, Android | thread cwd, /proc, fork (all in D) | thread cwd, /proc (in D), then /proc, symlink in D, symlink |
//! | Linux, Android without `/proc` | thread cwd, fork (in D) | thread cwd (in D), then relative path, symlink |
//! | macOS, iOS (Apple) | thread cwd, fork (in D) | thread cwd (in D), then relative path, widened sockaddr, symlink |
//! | FreeBSD | bindat (in D) | connectat (in D), then connectat |
//! | NetBSD, OpenBSD, illumos, other Unix | fork (in D) | relative path, symlink |
//!
//! `connect_any` and `connect_any_nonblocking` try what [`connect`] does, then
//! a forked child. The first route that can be used makes the call; a real
//! failure there (`ENOENT`, `EADDRINUSE`, `ECONNREFUSED`, ...) is the result,
//! not a reason to try the next.
//!
//! **The central route.** sockat makes a directory D of mode 0700 (named
//! `sockat.XXXXXXXXXX`, 10 random letters and digits) and holds it by an fd,
//! checked to be the directory it made (owned by the calling user, no
//! permission for group and others). In D it makes a symlink R with a random
//! name to the real path of the socket's directory, and makes the call with
//! `R/<file name>` relative to D's fd, by the first of these means that can be
//! used:
//!
//! * **thread cwd** (Linux, Apple): a thread started for the call alone moves
//!   its own working directory, and no other thread's, into D
//!   (`unshare(CLONE_FS)` then `fchdir` on Linux, `pthread_fchdir_np` on
//!   Apple). Not used on Linux under a seccomp filter (docker's default
//!   installs one; a filter may kill the process for `unshare`, so it is not
//!   called then) or where `pthread_fchdir_np` is missing
//! * **/proc** (Linux): `/proc/self/fd/<D's fd>/R/<file name>`, for a file
//!   name of up to about 79 bytes
//! * **bindat** (FreeBSD): `bindat(2)` / `connectat(2)` on D's fd
//! * **fork** ([`bind`] only): a forked child that `fchdir`s into D binds and
//!   exits; the listen is made by the calling process, so a client sees the
//!   calling process as the server, as with any other route
//!
//! D and R are removed when the call returns, so the spelling the kernel keeps
//! for a bound socket names nothing afterwards, wherever it is resolved. D goes
//! in `TMPDIR` (or `/tmp` when it is unset), then `/tmp`, then the socket's
//! directory (named `.sockat.XXXXXXXXXX` there). None of them needs to be
//! safe or short: D is held by its fd. When D can be made in none of them, a
//! bind forks with R (`.sockat.XXXXXXXXXX`) made beside the socket, and a
//! connect goes on to the fallback routes.
//!
//! `R/<file name>` takes 11 bytes, so a bind reaches a file name of up to 96
//! bytes (Linux, illumos) or 92 (Apple, the BSDs); 88 / 84 without D.
//!
//! **The fallback routes** (connects only). A connected socket is not bound,
//! so its spelling is kept nowhere and needs no random part:
//!
//! * **/proc** (Linux): `/proc/self/fd/<dirfd>/<file name>`, for a file name of
//!   up to about 90 bytes
//! * **symlink in D** (Linux with `/proc`): D made as above, a symlink `s` in
//!   it to the socket file, and the connect by `/proc/self/fd/<D's fd>/s`,
//!   whatever the directory depth and the file name length
//! * **relative path**: the path spelled relative to the working directory,
//!   when std accepts that. Only the path string is rewritten; the cwd is
//!   never changed
//! * **widened sockaddr** (Apple): a `sockaddr_un` carrying up to 253 bytes of
//!   path, which XNU accepts: the path itself, or failing that the relative
//!   spelling. A bind never uses it: the address of a socket bound so does not
//!   fit `sun_path` and breaks its readers (CPython, Go's `net.FileConn`)
//! * **symlink**: a short symlink to the socket file under `TMPDIR` or failing
//!   that `/tmp`, removed right after the call, made only in a place where no
//!   other user can replace it (a sticky directory owned by root or the calling
//!   user, as `/tmp` is, or a directory of the calling user's that group and
//!   others cannot write to). Through `/tmp`, a connect is reached whatever the
//!   directory depth and the file name length
//! * **connectat** (FreeBSD): `connectat(2)` on the socket's directory fd with
//!   the file name (up to 103 bytes)
//!
//! None of these changes the working directory of the calling process or of
//! the calling thread, and all of them can be used from multi-threaded
//! programs. Windows is not supported (the crate is Unix-only). The details,
//! and where each function stops reaching a path, are in the
//! [manual](https://github.com/kawaz/sockat-rs/blob/main/docs/MANUAL.md); the
//! design and the reasons in
//! [DR-0027](https://github.com/kawaz/sockat-rs/blob/main/docs/decisions/DR-0027-own-directory-and-fallback-routes.md)
//! (in Japanese).
//!
//! # The exception: `connect_any`
//!
//! [`connect`] and [`connect_nonblocking`] never fork: a connect is not
//! expected to, and after a connect through the fork route the peer pid the
//! server sees is that of a process that has already exited. Where only the
//! fork route would reach the path, they fail with `ENAMETOOLONG`, as any
//! connect to a path too long for `sun_path` does. That happens only when the
//! central route cannot be used for a connect (no thread cwd means and no
//! `/proc`: NetBSD, OpenBSD, illumos, Linux under a seccomp filter without
//! `/proc`, an Apple OS without `pthread_fchdir_np`) and none of the fallback
//! routes reaches the path (no safe place for the symlink, and the path fits
//! neither a relative spelling nor, on Apple, the widened sockaddr).
//!
//! `connect_any` and `connect_any_nonblocking` are for that rare environment,
//! when the connection is wanted at the cost of the fork's side effects. They
//! take the fork route as the last resort and return the stream with whether
//! they took it (`AnyConnection`), so that the caller can, for example, log or
//! warn about it. They are behind the crate feature `any` (see
//! [Features](#features)). [`bind`] takes the fork route as a last resort too;
//! the child only binds and the listen is done by the calling process, so the
//! listener is this process's like any other, and nothing a client can see
//! differs.
//!
//! # What differs from std for a long path
//!
//! * **The address the kernel reports is not the path.** `local_addr()` of the
//!   listener and of accepted streams, and `peer_addr()` of a connected client,
//!   report the spelling handed to the kernel when the listener was bound
//!   (whichever route the client took): `R/<file name>`,
//!   `/proc/self/fd/<n>/R/<file name>` or `.sockat.XXXXXXXXXX/<file name>`. R
//!   is gone by then, so removing, `stat`ing or `chmod`ing by the reported
//!   address finds nothing rather than another file; it never names the
//!   socket either. Keep the path you passed and use that (for example to
//!   remove the socket file with [`std::fs::remove_file`], which has no length
//!   limit, or with [`bind_ex`]). The same address shows in `Debug` output and
//!   in what frameworks log or hand out as the peer address.
//! * **A bind reaches a shorter file name than a connect**: 96 bytes on Linux
//!   and illumos, 92 on Apple and the BSDs (`R/` takes 11 of `sun_path`),
//!   where a connect reaches any.
//! * **A path that does not end with a file name is `InvalidInput`.** A path
//!   longer than std accepts ending in `/`, `/.` or `/..` (or being `..`)
//!   cannot be split into a directory and a file name and is refused before
//!   anything is created; a shorter one goes to std and gets the kernel's error.
//! * **The peer pid the server sees can be that of an exited process**
//!   (`connect_any` and `connect_any_nonblocking`, when they take the fork
//!   route): the forked child that made the connect and exited. The socket
//!   itself belongs to the caller and works as usual. The uid and gid the
//!   server sees are the caller's. On macOS (`LOCAL_PEERPID`) the server sees
//!   the caller's pid once the caller sends on the socket; on Linux
//!   (`SO_PEERCRED`) it stays the exited child's. The number of an exited pid
//!   may be reused by another process later. Nothing can be looked up from
//!   that pid (the process, its parent): the process is gone.
//! * **The fork route has side effects no peer sees** ([`bind`] and the
//!   `connect_any` ones). As with any `fork(2)`, the child has copies of all
//!   file descriptors until it exits a moment later, so a socket or pipe
//!   another thread closes during that moment stays open until then (a peer
//!   sees the EOF slightly later; a listener closed meanwhile can still accept
//!   a connection into its backlog). The calling process also gets a
//!   `SIGCHLD`, the `pthread_atfork` handlers run, monitoring tools see a
//!   short-lived child, and a `waitpid(-1)` on another thread may reap the
//!   child (sockat then reads the outcome off the socket).
//! * **Permissions.** On macOS the socket's directory and D are opened
//!   `O_SEARCH` and on Linux `O_PATH`, so a long path needs no more permission
//!   than a plain bind / connect; on other Unix targets they are opened
//!   `O_RDONLY` and need read permission as well.
//! * **The directory is resolved slightly earlier than the call.** R points to
//!   the real path of the socket's directory, taken when R is made, and the
//!   /proc, relative, connectat and fork routes of a connect look the
//!   directory up first. If a directory along the path is renamed or a symlink
//!   along it is repointed in between, the socket goes to (or is looked up in)
//!   the directory as it was when looked up, not as it is at the moment of the
//!   call.
//! * **The relative route is exposed to another thread's `chdir`.** sockat
//!   itself never changes the cwd. The path is spelled relative to the cwd only
//!   after checking that the spelling names the same directory, and the cwd is
//!   checked again afterwards; if another thread changed it in between, the
//!   result is discarded and an error is returned. Programs that change the
//!   cwd while other threads connect are racing every relative path in the
//!   process anyway; sockat does not make that safe, it only detects the
//!   visible case.
//! * **Something can be left behind** if the process dies in the middle of a
//!   call: D (`sockat.XXXXXXXXXX`) or the temporary symlink under `TMPDIR` or
//!   `/tmp`, and D or R (`.sockat.XXXXXXXXXX`) in the socket's directory. On
//!   the targets that open D `O_RDONLY` (not Linux and Apple), a umask that
//!   takes the owner's read bit makes D impossible to open and check, and it
//!   is left as it is.
//!
//! # Removing the socket file
//!
//! As with std, the socket file stays when the listener is closed. [`bind_ex`]
//! binds as [`bind`] does and also returns a [`SocketFile`], which removes the
//! socket file later ([`SocketFile::discard`]), only while it is still the one
//! the bind made. Closing the listener stays with whoever has it (tokio, a
//! server framework, ...). A replacement of the file between the check and
//! the removal, by someone who can write to the directory, cannot be caught:
//! POSIX has no way to remove a file by its inode.
//!
//! # Errors
//!
//! The errors are those of `bind(2)`, `listen(2)` and `connect(2)` (and of
//! opening the directory) as [`std::io::Error`]s, for example `ENOENT` for a
//! missing directory or socket, `EADDRINUSE` for an existing file,
//! `ECONNREFUSED` for a socket nobody listens on, `EAGAIN` for a full backlog
//! on a nonblocking connect (Linux; macOS answers `ECONNREFUSED` to any
//! connect on a full backlog, so there `ECONNREFUSED` alone does not mean
//! nobody listens). When no route can reach the path (for [`bind`], a file
//! name too long for `R/<file name>`; for [`connect`] and
//! [`connect_nonblocking`], also when only the fork route would), the error is
//! `ENAMETOOLONG`. A path longer than std accepts that does not end with a
//! file name is an `InvalidInput` error. So is a path of any length that
//! contains a NUL byte, refused before anything is created: the kernel would
//! read the path only up to the NUL. On Linux and Android a path starting with
//! a NUL names a socket in the abstract namespace rather than a file; it is
//! handed to std as is, which refuses it as well (std takes abstract names
//! through `SocketAddr::from_abstract_name` and `UnixListener::bind_addr` /
//! `UnixStream::connect_addr`).
//!
//! A path longer than `PATH_MAX` (1024 on macOS, 4096 on Linux) is
//! `ENAMETOOLONG`: the directory is opened by the full path, and that limit is
//! outside what sockat is for.
//!
//! # Features
//!
//! * `any` (off by default): `connect_any` and `connect_any_nonblocking`, the
//!   connects that fork as the last resort, and `AnyConnection`, what they
//!   return. Enabling it is the choice to accept the fork's side effects; the
//!   other functions do not need it.
//!
//! ```toml
//! [dependencies]
//! sockat = { version = "0.4", features = ["any"] }
//! ```
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
//! [`bind`] does not wait for a peer either. [`bind_ex`]'s listener goes to the
//! runtime the same way; its [`SocketFile`] stays with the caller.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

#[cfg(not(unix))]
compile_error!("sockat supports Unix targets only (Windows is out of scope, see DR-0009)");

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use route::ConnectOptions;

mod path;
mod route;
mod sys;

#[cfg(test)]
mod tests;

/// Create a socket bound to `path` and listening on it, like
/// [`UnixListener::bind`] but for a path of any length.
///
/// A path that std accepts goes to [`UnixListener::bind`] unchanged. A longer
/// path is reached by the routes the platform has, the fork route as the last
/// resort (see the [crate documentation](crate) for both): the forked child
/// only binds, and the listen is done by this process, so the listener is this
/// process's like any other. The socket file is created at `path` either way,
/// and is not removed when the listener is dropped (as with std).
///
/// # Errors
///
/// The errors of `bind(2)` / `listen(2)`, for example `EADDRINUSE` when `path`
/// exists and `ENOENT` when its directory does not; `ENAMETOOLONG` when the file
/// name alone is too long for any route; `InvalidInput` for a path longer than
/// std accepts that does not end with a file name, or for a path that contains
/// a NUL byte.
pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
    route::run(route::Request::Bind, path.as_ref()).map(|(fd, _)| UnixListener::from(fd))
}

/// [`bind`], and a [`SocketFile`] that removes the socket file later, only
/// while it is still the one this bind made.
///
/// The listener is std's own type, free to go wherever [`bind`]'s would (into
/// `tokio::net::UnixListener::from_std`, another thread, a server
/// framework); closing it stays with its owner. The [`SocketFile`] only
/// removes the file, by [`SocketFile::discard`], typically once the listener
/// is closed.
///
/// The socket file's real path and its `(st_dev, st_ino)` are taken right
/// after the bind, before this returns: for a path handed to std, as soon as
/// std's bind returns (`realpath(3)` of `path`, then `lstat(2)`); for a long
/// path, inside the route, while sockat's own directory and the symlink in it
/// are still there, by the same spelling the bind took. Someone who can write
/// to the socket's directory and replaces the file in the moment between the
/// bind and that look-up is not caught: POSIX has no way to name a file by
/// its inode.
///
/// ```no_run
/// let path = "/a/very/long/directory/.../server.sock";
/// let (listener, file) = sockat::bind_ex(path)?;
/// // ... serve on `listener` ...
/// drop(listener);
/// file.discard()?;
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// # Errors
///
/// Those of [`bind`], and those of looking the socket file up right after the
/// bind; then the listener is closed and the socket file stays (as after
/// [`bind`]), so that no [`SocketFile`] is handed out that could not remove
/// it.
pub fn bind_ex<P: AsRef<Path>>(path: P) -> io::Result<(UnixListener, SocketFile)> {
    let (fd, _, bound) = route::bind_remembering(path.as_ref())?;
    let file = SocketFile {
        path: bound.path,
        id: bound.id,
    };
    Ok((UnixListener::from(fd), file))
}

/// The socket file a [`bind_ex`] made: its real path and `(st_dev, st_ino)`.
///
/// Dropping it does nothing: the file is removed only by [`SocketFile::discard`].
#[derive(Debug)]
pub struct SocketFile {
    /// The real path of the socket file, taken right after the bind.
    path: PathBuf,
    /// `(st_dev, st_ino)` of the socket file at that path then.
    id: (u64, u64),
}

impl SocketFile {
    /// Remove the socket file, if what is at its real path now is still the
    /// file the bind made (the same `(st_dev, st_ino)`, looked up without
    /// following a symlink).
    ///
    /// Anything else there (another socket bound at the path since, a file
    /// or a symlink put in its place) is left alone, and so is nothing at all:
    /// both are `Ok(())`, the file of the bind being gone either way. The
    /// listener is not touched; close it first, as a socket file removed under
    /// a listener that is still open leaves that listener unreachable by path.
    ///
    /// The look-up and the removal are two steps: a file put at the path in
    /// the moment between them, by someone who can write to the directory, is
    /// removed instead. POSIX has no way to remove a file by its inode, so
    /// this cannot be closed. An inode number freed by the file's removal and
    /// given to a new file on the same device makes that file look like the
    /// bind's.
    ///
    /// # Errors
    ///
    /// Those of `lstat(2)` (other than `ENOENT`) and `unlink(2)` on the path.
    pub fn discard(self) -> io::Result<()> {
        match file_id(&self.path) {
            Ok(id) if id == self.id => match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                removed => removed,
            },
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

/// `(st_dev, st_ino)` of `path`, without following a symlink at it.
fn file_id(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(path)?;
    Ok((m.dev(), m.ino()))
}

/// Connect to the socket at `path`, like [`UnixStream::connect`] but for a path
/// of any length, without forking. Use it wherever std's would be used.
///
/// A path that std accepts goes to [`UnixStream::connect`] unchanged. A longer
/// path is reached by every route the platform has but the fork route
/// (see the [crate documentation](crate)), so in an ordinary environment it is
/// reached whatever the directory depth and the file name length, and the connection is always made by this
/// process. Only in a rare environment where nothing but the fork route would
/// reach the path is the error `ENAMETOOLONG`, as for any connect to a path
/// too long for `sun_path` (`connect_any`, behind the crate feature
/// `any`, is for that case).
///
/// The stream blocks, like std's. In async code use [`connect_nonblocking`].
///
/// # Errors
///
/// The errors of `connect(2)`, for example `ENOENT` when nothing is at `path`
/// and `ECONNREFUSED` when nobody listens on it (on macOS also when its backlog
/// is full); `ENAMETOOLONG` when no route but the fork route reaches the path
/// (or none at all does); `InvalidInput` for a path longer than std accepts
/// that does not end with a file name, or for a path that contains a NUL byte.
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

/// Available on crate feature `any` only.
///
/// What [`connect_any`] and [`connect_any_nonblocking`] return: the stream,
/// and whether the fork route was taken to connect it.
///
/// The fields are public so the value can be taken apart as it comes:
///
/// ```no_run
/// let path = "/a/very/long/directory/.../server.sock";
/// let sockat::AnyConnection { stream, forked } = sockat::connect_any(path)?;
/// if forked {
///     eprintln!("connected to {path} through a forked child");
/// }
/// # drop(stream);
/// # Ok::<(), std::io::Error>(())
/// ```
#[cfg(feature = "any")]
#[derive(Debug)]
pub struct AnyConnection {
    /// The connected stream, std's own type. It belongs to this process and
    /// works as usual whichever route made the connection.
    pub stream: UnixStream,
    /// `true` when the connection was made through the fork route, by a
    /// forked child that has exited since; `false` on every other route (the
    /// connection was made by this process). After the fork route, the peer
    /// pid the server sees is that of the exited child (see
    /// [`connect_any`]); use this to note it, for example by logging or
    /// warning about it.
    pub forked: bool,
}

/// Available on crate feature `any` only.
///
/// [`connect`], forking when nothing else reaches the path, and whether it
/// did.
///
/// Use [`connect`] (or [`connect_nonblocking`]) unless it fails with
/// `ENAMETOOLONG` in an environment you have to support: it reaches a long
/// path in any ordinary environment. This is for the rare one where only the
/// fork route does (sockat's own directory cannot be used for a connect, and
/// none of the fallback routes reaches the path; see the
/// [crate documentation](crate)), when the connection is wanted at the cost of
/// the fork's side effects. The order tried is that of [`connect`], with the
/// fork route last.
///
/// The fork route connects from a forked child that `fchdir`s into the
/// socket's directory and exits at once. [`AnyConnection::forked`] is `true`
/// when the fork route was taken, and `false` on every other route (the
/// connection was made by this process).
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
#[cfg(feature = "any")]
pub fn connect_any<P: AsRef<Path>>(path: P) -> io::Result<AnyConnection> {
    connect_with(
        path.as_ref(),
        ConnectOptions {
            nonblocking: false,
            fork: true,
        },
    )
    .map(|(stream, forked)| AnyConnection { stream, forked })
}

/// Available on crate feature `any` only.
///
/// [`connect_any`] on a nonblocking socket, as [`connect_nonblocking`] is to
/// [`connect`]. Like [`connect_any`], only for the rare environment where
/// [`connect_nonblocking`] cannot reach the path. On the fork route the socket
/// is made nonblocking before the fork, so the child's connect does not wait
/// either.
///
/// # Errors
///
/// Those of [`connect_any`], and `EAGAIN` for a full backlog on Linux.
#[cfg(feature = "any")]
pub fn connect_any_nonblocking<P: AsRef<Path>>(path: P) -> io::Result<AnyConnection> {
    connect_with(
        path.as_ref(),
        ConnectOptions {
            nonblocking: true,
            fork: true,
        },
    )
    .map(|(stream, forked)| AnyConnection { stream, forked })
}

/// The four connects: block or not, fork or not ([`ConnectOptions`]). Returns
/// the stream and whether the fork route was taken (always `false` without
/// `fork`).
pub(crate) fn connect_with(path: &Path, options: ConnectOptions) -> io::Result<(UnixStream, bool)> {
    route::run(route::Request::Connect(options), path)
        .map(|(fd, route)| (UnixStream::from(fd), route.forked()))
}
