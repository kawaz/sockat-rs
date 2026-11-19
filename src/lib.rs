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
//! own, D (**the routes through D**), and only when that cannot be used,
//! for a bind through the socket's directory itself (**the form without
//! D**), for a connect through **the fallback routes**. The routes carry the
//! names of the [specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md):
//!
//! | Platform | [`bind`] | [`connect`], [`connect_nonblocking`] |
//! |---|---|---|
//! | Linux, Android | thread-cwd, proc, fork | thread-cwd, proc, then proc-direct, proc-held, symlink |
//! | Linux, Android without `/proc` | thread-cwd, fork | thread-cwd, then relative, symlink |
//! | macOS, iOS (Apple) | thread-cwd, fork | thread-cwd, then relative, widened, symlink |
//! | FreeBSD | at | at, then at-direct |
//! | NetBSD, OpenBSD, illumos, other Unix | fork | relative, symlink |
//!
//! `bind_any` tries what [`bind`] does, then a symlink in a place;
//! `connect_any` and `connect_any_nonblocking` try what [`connect`] does,
//! then a forked child (fork-direct). The first route that can be used makes
//! the call; a real failure there (`ENOENT`, `EADDRINUSE`, `ECONNREFUSED`,
//! ...) is the result, not a reason to try the next.
//!
//! **The routes through D.** sockat makes a directory D of mode 0700 (named
//! `sockat.XXXXXXXXXX`, 10 random letters and digits) and holds it by an fd,
//! checked to be the directory it made (owned by the calling user, no
//! permission for group and others). In D it makes a symlink R with a random
//! name to the real path of the socket's directory, and makes the call with
//! `R/<file name>` relative to D's fd, by the first of these routes that can
//! be used:
//!
//! * **thread-cwd** (Linux, Apple): a thread started for the call alone moves
//!   its own working directory, and no other thread's, into D
//!   (`unshare(CLONE_FS)` then `fchdir` on Linux, `pthread_fchdir_np` on
//!   Apple). Not used on Linux under a seccomp filter (docker's default
//!   installs one; a filter may kill the process for `unshare`, so it is not
//!   called then) or where `pthread_fchdir_np` is missing
//! * **proc** (Linux): `/proc/self/fd/<D's fd>/R/<file name>`, for a file
//!   name of up to about 79 bytes
//! * **at** (FreeBSD): `bindat(2)` / `connectat(2)` on D's fd
//! * **fork** ([`bind`] only): a forked child that `fchdir`s into D binds and
//!   exits; the listen is made by the calling process, so a client sees the
//!   calling process as the server, as with any other route
//!
//! D and R are removed when the call returns, so the spelling the kernel keeps
//! for a bound socket names nothing afterwards, wherever it is resolved. D goes
//! in `TMPDIR` (or `/tmp` when it is unset), then `/tmp`, then the socket's
//! directory (named `.sockat.XXXXXXXXXX` there). None of them needs to be
//! safe or short: D is held by its fd. A D that cannot be opened right after
//! it is made (a umask took the owner's bits) is removed again by its name,
//! and the next place is tried.
//!
//! **The form without D.** When D can be made in no place, a bind makes R
//! (`.sockat.XXXXXXXXXX`) beside the socket instead and tries the same routes
//! (thread-cwd-sockdir or at-sockdir, proc-sockdir, fork-sockdir) with
//! `.sockat.XXXXXXXXXX/<file name>` relative to the socket's directory.
//!
//! `R/<file name>` takes 11 bytes, so a bind reaches a file name of up to 96
//! bytes (Linux, illumos) or 92 (Apple, the BSDs); 88 / 84 without D (about
//! 72 by proc-sockdir).
//!
//! **The fallback routes** (connects only). A connected socket is not bound,
//! so its spelling is kept nowhere and needs no random part:
//!
//! * **proc-direct** (Linux): `/proc/self/fd/<dirfd>/<file name>`, for a file
//!   name of up to about 90 bytes
//! * **proc-held** (Linux with `/proc`): D made as above, a symlink `s` in it
//!   to the socket file, and the connect by `/proc/self/fd/<D's fd>/s`,
//!   whatever the directory depth and the file name length
//! * **relative**: the path spelled relative to the working directory, when
//!   std accepts that. Only the path string is rewritten; the cwd is never
//!   changed
//! * **widened** (Apple): a `sockaddr_un` carrying up to 253 bytes of path,
//!   which XNU accepts: the path itself, or failing that the relative
//!   spelling. A bind never uses it: the address of a socket bound so does not
//!   fit `sun_path` and breaks its readers (CPython, Go's `net.FileConn`)
//! * **symlink**: a short symlink to the socket file under `TMPDIR` or failing
//!   that `/tmp`, removed right after the call, made only in a place where no
//!   other user can replace it (a sticky directory owned by root or the calling
//!   user, as `/tmp` is, or a directory of the calling user's that group and
//!   others cannot write to). Through `/tmp`, a connect is reached whatever the
//!   directory depth and the file name length
//! * **at-direct** (FreeBSD): `connectat(2)` on the socket's directory fd with
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
//! # The exception: the `any` functions
//!
//! [`connect`] and [`connect_nonblocking`] never fork: a connect is not
//! expected to, and after a connect through a fork the peer pid the server
//! sees is that of a process that has already exited. Where only that would
//! reach the path, they fail with `ENAMETOOLONG`, as any connect to a path
//! too long for `sun_path` does. That happens only when the routes through D
//! cannot be used for a connect (no thread cwd and no `/proc`: NetBSD,
//! OpenBSD, illumos, Linux under a seccomp filter without `/proc`, an Apple
//! OS without `pthread_fchdir_np`) and none of the fallback routes reaches the
//! path (no safe place for the symlink, and the path fits neither a relative
//! spelling nor, on Apple, the widened sockaddr). [`bind`] fails with
//! `ENAMETOOLONG` where no route through D or beside the socket can be used.
//!
//! `bind_any`, `connect_any` and `connect_any_nonblocking`, behind the crate
//! feature `any` (see [Features](#features)), are for that rare environment,
//! when the path is wanted at the cost of something: `bind_any` binds last
//! through a symlink to the socket's directory in a place (in a shared place
//! such as `/tmp`, another user can make the reported address again), the
//! `connect_any` ones connect last from a forked child. They return the
//! listener or stream with what was given up (`Loss`), the name of the route
//! taken and the routes passed over (`AnyListener`, `AnyConnection`), and on
//! failure an `AnyError` that converts back into the `io::Error`. [`bind`] forks as a
//! last resort too; the child only binds and the listen is done by the
//! calling process, so the listener is this process's like any other, and
//! nothing a client can see differs.
//!
//! # What differs from std for a long path
//!
//! * **The address the kernel reports is not the path.** `local_addr()` of the
//!   listener and of accepted streams, and `peer_addr()` of a connected client,
//!   report the spelling handed to the kernel when the listener was bound
//!   (whichever route the client took): `R/<file name>`,
//!   `/proc/self/fd/<n>/R/<file name>` or `.sockat.XXXXXXXXXX/<file name>`
//!   (`<place>/sockat.XXXXXXXXXX/<file name>` after `bind_any`'s symlink). R
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
//!   (`connect_any` and `connect_any_nonblocking`, when they take
//!   fork-direct): the forked child that made the connect and exited. The socket
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
//!   proc-direct, relative, at-direct and fork-direct routes of a connect look
//!   the directory up first. If a directory along the path is renamed or a symlink
//!   along it is repointed in between, the socket goes to (or is looked up in)
//!   the directory as it was when looked up, not as it is at the moment of the
//!   call.
//! * **relative is exposed to another thread's `chdir`.** sockat
//!   itself never changes the cwd. The path is spelled relative to the cwd only
//!   after checking that the spelling names the same directory, and the cwd is
//!   checked again afterwards; if another thread changed it in between, the
//!   result is discarded and an error is returned. Programs that change the
//!   cwd while other threads connect are racing every relative path in the
//!   process anyway; sockat does not make that safe, it only detects the
//!   visible case.
//! * **Something can be left behind** if the process dies in the middle of a
//!   call: D (`sockat.XXXXXXXXXX`) or the temporary symlink under
//!   `XDG_RUNTIME_DIR`, `TMPDIR` or `/tmp`, and D or R (`.sockat.XXXXXXXXXX`)
//!   in the socket's directory.
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
//! # What is lost
//!
//! What taking a route loses compared to std, by the names of the
//! [specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-is-lost).
//! The `any` functions return them as `losses` (`Loss`); [`bind`] and
//! [`bind_ex`] lose the same on the routes they take, without returning it.
//! The [manual](https://github.com/kawaz/sockat-rs/blob/main/docs/MANUAL.md#what-is-lost)
//! has what happens, what to watch out for and how to avoid each, with the
//! code below.
//!
//! ## respelled
//!
//! A bind took a route other than standard: the address the kernel reports
//! is not the path. Others may see that spelling and make it again, so do not
//! remove or `stat` the file by it; keep the original path and use it, or
//! use [`bind_ex`]'s [`SocketFile::discard`].
//!
//! ```
//! use std::io;
//! use std::path::Path;
//!
//! fn serve(path: &Path) -> io::Result<()> {
//!     let (listener, file) = sockat::bind_ex(path)?;
//!     // accept on listener (it may go to tokio etc.)
//!     drop(listener);
//!     file.discard() // removes the file only while it is still the one bound
//! }
//!
//! fn serve_and_remove(path: &Path) -> io::Result<()> {
//!     let listener = sockat::bind(path)?;
//!     // accept on listener
//!     drop(listener);
//!     std::fs::remove_file(path) // remove by the original path
//! }
//! # let dir = tempfile::tempdir()?;
//! # let deep = dir.path().join("d".repeat(100));
//! # std::fs::create_dir(&deep)?;
//! # serve(&deep.join("a.sock"))?;
//! # serve_and_remove(&deep.join("b.sock"))?;
//! # assert_eq!(std::fs::read_dir(&deep)?.count(), 0);
//! # Ok::<(), io::Error>(())
//! ```
//!
//! ## cwd-relative
//!
//! A bind of a relative path went to std as it is: the spelling the kernel
//! keeps is relative to the cwd, and removing or `stat`ing by it (or by the
//! relative path) after the cwd changes hits another place. A hazard std has
//! on its own, reported all the same. Make the path absolute before the bind,
//! and remove by the absolute path or `discard()`.
//!
//! ```
//! use std::io;
//! use std::path::Path;
//!
//! fn serve(path: &Path) -> io::Result<()> {
//!     let path = std::path::absolute(path)?; // make it absolute before the bind
//!     let (listener, file) = sockat::bind_ex(&path)?;
//!     // accept on listener
//!     drop(listener);
//!     file.discard()
//! }
//! # let dir = tempfile::tempdir()?;
//! # serve(&dir.path().join("s.sock"))?;
//! # assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
//! # Ok::<(), io::Error>(())
//! ```
//!
//! ## forked
//!
//! A forked child made the call (fork, fork-sockdir, fork-direct), with the
//! side effects of a fork (see [What differs from std for a long
//! path](#what-differs-from-std-for-a-long-path)). A `SIGCHLD` handler or a
//! `waitpid(-1)` may reap the child or mistake the result. Do not use
//! `waitpid(-1)` in other threads (wait by pid), and where `forked` cannot be
//! accepted, do not use the `any` functions.
//!
//! ```
//! use std::io;
//! use std::os::unix::net::UnixStream;
//! use std::path::Path;
//! use std::process::{Command, ExitStatus};
//!
//! fn run_tool() -> io::Result<ExitStatus> {
//!     let mut child = Command::new("true").spawn()?;
//!     child.wait() // waits for this child's pid (no waitpid(-1))
//! }
//!
//! fn dial(path: &Path) -> io::Result<UnixStream> {
//!     sockat::connect(path) // a connect that does not fork, not connect_any
//! }
//! # assert!(run_tool()?.success());
//! # let dir = tempfile::tempdir()?;
//! # let path = dir.path().join("d".repeat(100)).join("s.sock");
//! # std::fs::create_dir(path.parent().unwrap())?;
//! # let _listener = sockat::bind(&path)?;
//! # dial(&path)?;
//! # Ok::<(), io::Error>(())
//! ```
//!
//! ## recreatable-spelling
//!
//! `bind_any` took the symlink in a shared place such as `/tmp`: the spelling
//! the kernel reports goes through it, and another user can make it again
//! once seen. Remove by the original path, not by that spelling. A place only
//! the owner can write to (`$XDG_RUNTIME_DIR`, or a `$TMPDIR` only the owner
//! can write to) is used first, and then this name is not given.
//!
//! ```
//! # #[cfg(feature = "any")] {
//! use std::io;
//! use std::path::Path;
//!
//! fn serve(path: &Path) -> io::Result<()> {
//!     let any = sockat::bind_any(path)?; // feature `any`
//!     // accept on any.listener
//!     drop(any.listener);
//!     std::fs::remove_file(path) // by the original path, not local_addr()'s spelling
//! }
//! # let dir = tempfile::tempdir()?;
//! # let deep = dir.path().join("d".repeat(100));
//! # std::fs::create_dir(&deep)?;
//! # serve(&deep.join("s.sock"))?;
//! # assert_eq!(std::fs::read_dir(&deep)?.count(), 0);
//! # }
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! # Patterns not to use
//!
//! Ways of using sockat that remove an unrelated file or fail to remove the
//! file (the
//! [specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#patterns-not-to-use)
//! and the
//! [manual](https://github.com/kawaz/sockat-rs/blob/main/docs/MANUAL.md#patterns-not-to-use)
//! have why, and what to do instead):
//!
//! * removing or `stat`ing the socket file by the address the kernel reports
//!   (`listener.local_addr()?.as_pathname()`): keep the original path, or use
//!   [`SocketFile::discard`];
//! * binding a relative path and removing by it after the cwd changes: make
//!   the path absolute before the bind;
//! * relying on closing the listener to remove the socket file, or on
//!   `discard()` to close the listener;
//! * branching on an `any` function's `route` or `skipped`: branch on
//!   `losses`;
//! * reaping children with `waitpid(-1)` on another thread of a program that
//!   uses the `any` functions' fork: wait by pid.
//!
//! ```no_run
//! use std::io;
//! use std::path::Path;
//!
//! fn wrong(path: &Path) -> io::Result<()> {
//!     let listener = sockat::bind(path)?;
//!     // removing by the spelling the kernel reports: for a long path it is not the path, and may hit another file
//!     std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())?;
//!     // closing the listener does not remove the socket file
//!     drop(listener);
//!
//!     let (listener, file) = sockat::bind_ex(path)?;
//!     // discard() does not close the listener
//!     file.discard()?;
//!     let _still_open = listener;
//!     Ok(())
//! }
//!
//! fn wrong_relative() -> io::Result<()> {
//!     let _listener = sockat::bind("app.sock")?;
//!     std::env::set_current_dir("/")?;
//!     // removing by the relative path after the cwd changed: hits a file in another place
//!     std::fs::remove_file("app.sock")
//! }
//!
//! # #[cfg(feature = "any")]
//! fn wrong_any(path: &Path) -> io::Result<()> {
//!     let any = sockat::bind_any(path)?; // feature `any`
//!     // branching on the route's string: it promises nothing (branch on losses)
//!     if any.route == "symlink" {
//!         eprintln!("bound through a symlink");
//!     }
//!     Ok(())
//! }
//! ```
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
//! * `any` (off by default): `bind_any`, `connect_any` and
//!   `connect_any_nonblocking`, which reach the path at the cost of something
//!   (a symlink others may make again, a fork), and `AnyListener`,
//!   `AnyConnection`, `AnyError`, `Loss` and `Skipped`, what they return. Enabling it is the
//!   choice to accept those costs; the other functions do not need it.
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

#[cfg(feature = "any")]
use route::Route;
use route::{BindOptions, ConnectOptions, Request};

mod path;
mod route;
mod sys;

#[cfg(all(sockat_conformance, feature = "any"))]
#[doc(hidden)]
pub mod conformance;

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
    route::run(Request::Bind(BindOptions { symlink: false }), path.as_ref())
        .map(|(fd, _)| UnixListener::from(fd))
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
    /// or a symlink put in its place) is left alone, and so is nothing at all
    /// (the path or a directory along it gone, or a directory along it
    /// replaced by something that is not one): all are `Ok(())`, the file of
    /// the bind being gone either way. When what is at the path cannot be
    /// looked up for another reason (`EACCES`, say), whether the file is
    /// still there is unknown, and that is an error rather than `Ok(())`. The
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
    /// Those of `lstat(2)` (other than `ENOENT` and `ENOTDIR`, which mean
    /// the file is not there) and `unlink(2)` on the path.
    pub fn discard(self) -> io::Result<()> {
        let gone = |e: &io::Error| matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR));
        match file_id(&self.path) {
            Ok(id) if id == self.id => match std::fs::remove_file(&self.path) {
                Err(e) if gone(&e) => Ok(()),
                removed => removed,
            },
            Ok(_) => Ok(()),
            Err(e) if gone(&e) => Ok(()),
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
    connect_with(path.as_ref(), false)
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
    connect_with(path.as_ref(), true)
}

#[cfg(feature = "any")]
pub use route::Skipped;

/// Available on crate feature `any` only.
///
/// Something an `any` function gave up, compared with std, to reach the path
/// (the SPEC's "失うもの"). A call returns those of the route it took, as a
/// list ([`AnyListener::losses`]); none of them implies another, and they
/// have no order.
///
/// The names are promised and only ever added to (the enum is
/// `#[non_exhaustive]`): read one you do not know as "something was given
/// up". [`Loss::as_str`] and `Display` give the SPEC's name.
#[cfg(feature = "any")]
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Loss {
    /// `respelled`: the address the kernel reports for the bound socket is
    /// not the path (see the [crate documentation](crate)). It names nothing
    /// once the call has returned, but it can be seen by other users (Linux's
    /// `/proc/net/unix`), and where the process's cwd is writable by others
    /// they can make it again; remove the socket file by the path you passed,
    /// not by the reported address. Only a bind gives this up.
    Respelled,
    /// `forked`: a forked child made the call, with the side effects of a
    /// fork (see the [crate documentation](crate)). After a connect, the peer
    /// pid the server sees is the exited child's.
    Forked,
    /// `recreatable-spelling`: the address the kernel reports goes through a
    /// shared place such as `/tmp` (`bind_any`'s symlink), where another user
    /// can make it again once sockat removed it: whatever later removes the
    /// socket file by that address could remove a file of that user's
    /// choosing. Remove it by the path you passed.
    RecreatableSpelling,
    /// `cwd-relative`: a relative path went to std as it is, so the address
    /// the kernel keeps for the bound socket is relative to the cwd: after the
    /// cwd changes, removing or looking up the socket by it reaches another
    /// place. std has this risk on its own; it is reported all the same. Not
    /// for a name in Linux's abstract namespace.
    CwdRelative,
}

#[cfg(feature = "any")]
impl Loss {
    /// The SPEC's name: `"respelled"`, `"forked"`, `"recreatable-spelling"`
    /// or `"cwd-relative"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Loss::Respelled => "respelled",
            Loss::Forked => "forked",
            Loss::RecreatableSpelling => "recreatable-spelling",
            Loss::CwdRelative => "cwd-relative",
        }
    }
}

#[cfg(feature = "any")]
impl std::fmt::Display for Loss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the route `route` gave up for a bind (`bind`) or a connect to `path`
/// (SPEC "経路の名前", the column "失うもの").
#[cfg(feature = "any")]
fn losses(bind: bool, route: Route, path: &Path) -> Vec<Loss> {
    match (bind, route) {
        (true, Route::Standard) if !path.is_absolute() && !path::is_abstract(path) => {
            vec![Loss::CwdRelative]
        }
        (_, Route::Standard) => Vec::new(),
        (true, Route::SymlinkBind { shared: true }) => {
            vec![Loss::Respelled, Loss::RecreatableSpelling]
        }
        (true, r) if r.forked() => vec![Loss::Respelled, Loss::Forked],
        (true, _) => vec![Loss::Respelled],
        (false, r) if r.forked() => vec![Loss::Forked],
        (false, _) => Vec::new(),
    }
}

/// Available on crate feature `any` only.
///
/// What [`bind_any`] returns: the listener, what was given up to bind it,
/// and how it was bound.
///
/// The fields are public so the value can be taken apart as it comes. Later
/// versions may add fields (the struct is `#[non_exhaustive]`), so a pattern
/// needs `..`:
///
/// ```no_run
/// let path = "/a/very/long/directory/.../server.sock";
/// let sockat::AnyListener { listener, losses, route, .. } = sockat::bind_any(path)?;
/// if losses.contains(&sockat::Loss::RecreatableSpelling) {
///     eprintln!("bound {path} by {route}: others can make its address again");
/// }
/// # drop(listener);
/// # Ok::<(), sockat::AnyError>(())
/// ```
#[cfg(feature = "any")]
#[non_exhaustive]
#[derive(Debug)]
pub struct AnyListener {
    /// The listening socket, std's own type, as [`bind`] returns it.
    pub listener: UnixListener,
    /// What was given up to bind it ([`Loss`]), empty when nothing was.
    /// Branch on this.
    pub losses: Vec<Loss>,
    /// The name of the route that bound it (the SPEC's "経路の名前", such as
    /// `"thread-cwd"` or `"symlink"`). For diagnostics only: routes may be
    /// added, renamed or changed, so do not branch on it.
    pub route: &'static str,
    /// The routes tried before [`AnyListener::route`] and passed over, in the
    /// order tried, each with why ([`Skipped`]): those of the order this call
    /// tries on this platform in its state (no /proc route on a Linux without
    /// /proc), `standard` first for a long path. Empty for a path std took.
    /// For diagnostics only, like [`AnyListener::route`].
    pub skipped: Vec<Skipped>,
}

/// Available on crate feature `any` only.
///
/// What [`connect_any`] and [`connect_any_nonblocking`] return: the stream,
/// what was given up to connect it, and how it was connected.
///
/// The fields are public so the value can be taken apart as it comes. Later
/// versions may add fields (the struct is `#[non_exhaustive]`), so a pattern
/// needs `..`:
///
/// ```no_run
/// let path = "/a/very/long/directory/.../server.sock";
/// let sockat::AnyConnection { stream, losses, .. } = sockat::connect_any(path)?;
/// if losses.contains(&sockat::Loss::Forked) {
///     eprintln!("connected to {path} through a forked child");
/// }
/// # drop(stream);
/// # Ok::<(), sockat::AnyError>(())
/// ```
#[cfg(feature = "any")]
#[non_exhaustive]
#[derive(Debug)]
pub struct AnyConnection {
    /// The connected stream, std's own type. It belongs to this process and
    /// works as usual whichever route made the connection.
    pub stream: UnixStream,
    /// What was given up to connect it ([`Loss`]): [`Loss::Forked`] when a
    /// forked child that has exited since made the connection, after which
    /// the peer pid the server sees is that child's (see [`connect_any`]);
    /// empty when this process made it (a connect's spelling is kept
    /// nowhere). Branch on this.
    pub losses: Vec<Loss>,
    /// The name of the route that connected it, as [`AnyListener::route`].
    pub route: &'static str,
    /// The routes passed over before it, as [`AnyListener::skipped`].
    pub skipped: Vec<Skipped>,
}

/// Available on crate feature `any` only.
///
/// Why an `any` function did not reach the path: the [`io::Error`], and the
/// routes tried, for diagnostics.
///
/// It converts into the [`io::Error`] as it is (`From`), so `?` in a function
/// returning [`io::Result`] works and [`io::Error::raw_os_error`] still gives
/// the errno; the `error` field gives it without the conversion. `Display`
/// and `source` are those of the [`io::Error`].
///
/// The fields are public; later versions may add fields (the struct is
/// `#[non_exhaustive]`).
///
/// ```no_run
/// let path = "/a/very/long/directory/.../server.sock";
/// match sockat::connect_any(path) {
///     Ok(c) => drop(c.stream),
///     Err(e) => eprintln!("{e} (errno {:?}) after {:?}", e.error.raw_os_error(), e.skipped),
/// }
/// ```
#[cfg(feature = "any")]
#[non_exhaustive]
#[derive(Debug)]
pub struct AnyError {
    /// The error, as the plain function returns it (for example `ENOENT`, or
    /// `ENAMETOOLONG` when no route could be used).
    pub error: io::Error,
    /// The name of the route that made the call and failed for real (an
    /// existing file: `EADDRINUSE`, nobody listening: `ECONNREFUSED`, ...;
    /// `"standard"` when std failed on a path it took); `None` when no route
    /// could be used, or the path was refused before any was tried (a NUL
    /// byte, no file name, the socket's directory missing or not one). For
    /// diagnostics only, as [`AnyListener::route`].
    pub route: Option<&'static str>,
    /// The routes tried and passed over, in the order tried, as
    /// [`AnyListener::skipped`]; empty when the path was refused before any
    /// route was tried (the length is judged only once the socket's directory
    /// is looked up).
    pub skipped: Vec<Skipped>,
}

#[cfg(feature = "any")]
impl std::fmt::Display for AnyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

#[cfg(feature = "any")]
impl std::error::Error for AnyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

#[cfg(feature = "any")]
impl From<AnyError> for io::Error {
    fn from(e: AnyError) -> Self {
        e.error
    }
}

/// The [`AnyError`] of a call that did not reach the path.
#[cfg(feature = "any")]
fn any_error(f: route::Failed) -> AnyError {
    AnyError {
        error: f.error,
        route: f.route,
        skipped: f.skipped,
    }
}

/// Available on crate feature `any` only.
///
/// [`bind`], and when no route of [`bind`] can be used, a symlink in a place
/// to the socket's directory as the last resort, accepting what that gives
/// up; returns what was given up and how the path was bound
/// ([`AnyListener`]).
///
/// Use [`bind`] unless it fails with `ENAMETOOLONG` in an environment you have
/// to support. The order tried is that of [`bind`], so where [`bind`] reaches
/// the path this binds the same way. Where [`bind`] would be `ENAMETOOLONG`
/// (sockat's own directory can be made in no place, and the socket's
/// directory cannot be opened or R made in it), this makes a symlink
/// `sockat.XXXXXXXXXX` (10 random letters and digits) to the real path of the
/// socket's directory in a place, binds by
/// `<place>/sockat.XXXXXXXXXX/<file name>`, and removes the symlink. The
/// places are `XDG_RUNTIME_DIR` and `TMPDIR` (each when set to an absolute
/// path) and `/tmp`: first those only this user can write to (owned by this
/// user, without write permission for group and others), then the shared ones
/// (sticky and owned by root or this user, as `/tmp` is). A place that is
/// neither, or too long for the spelling, is passed over. The spelling is
/// longer than that of [`bind`]'s routes, so it does not reach a longer file
/// name than [`bind`] does.
///
/// The socket file is created at `path`, as with [`bind`], and the address
/// the kernel reports is `<place>/sockat.XXXXXXXXXX/<file name>`. Through a
/// shared place another user can make that name again once the symlink is
/// gone, so whatever later removes the socket file by the reported address
/// could remove a file of that user's choosing; remove it by the path you
/// passed.
///
/// # Errors
///
/// Those of [`bind`], except that a path the symlink reaches is bound rather
/// than `ENAMETOOLONG`, as an [`AnyError`] with the routes tried (it
/// converts into the [`io::Error`] by `?`).
#[cfg(feature = "any")]
pub fn bind_any<P: AsRef<Path>>(path: P) -> Result<AnyListener, AnyError> {
    bind_reporting(path.as_ref(), BindOptions { symlink: true })
}

/// A bind as `options` say, with what was given up and how ([`AnyListener`]).
#[cfg(feature = "any")]
fn bind_reporting(path: &Path, options: BindOptions) -> Result<AnyListener, AnyError> {
    let r = route::run_reporting(Request::Bind(options), path).map_err(any_error)?;
    Ok(AnyListener {
        listener: UnixListener::from(r.fd),
        losses: losses(true, r.route, path),
        route: r.route.name(),
        skipped: r.skipped,
    })
}

/// Available on crate feature `any` only.
///
/// [`connect`], forking when nothing else reaches the path, and how it
/// connected.
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
/// socket's directory and exits at once. [`AnyConnection::losses`] holds
/// [`Loss::Forked`] when the fork route was taken; on every other route the
/// connection was made by this process, and nothing is given up.
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
/// connected rather than `ENAMETOOLONG`, as an [`AnyError`] with the routes
/// tried (it converts into the [`io::Error`] by `?`).
#[cfg(feature = "any")]
pub fn connect_any<P: AsRef<Path>>(path: P) -> Result<AnyConnection, AnyError> {
    connect_any_with(path.as_ref(), false)
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
pub fn connect_any_nonblocking<P: AsRef<Path>>(path: P) -> Result<AnyConnection, AnyError> {
    connect_any_with(path.as_ref(), true)
}

/// The two connects that do not fork: block or not.
fn connect_with(path: &Path, nonblocking: bool) -> io::Result<UnixStream> {
    let options = ConnectOptions {
        nonblocking,
        fork: false,
    };
    route::run(Request::Connect(options), path).map(|(fd, _)| UnixStream::from(fd))
}

/// The two connects that may fork: block or not.
#[cfg(feature = "any")]
fn connect_any_with(path: &Path, nonblocking: bool) -> Result<AnyConnection, AnyError> {
    let options = ConnectOptions {
        nonblocking,
        fork: true,
    };
    connect_reporting(path, options)
}

/// A connect as `options` say, with what was given up and how
/// ([`AnyConnection`]).
#[cfg(feature = "any")]
fn connect_reporting(path: &Path, options: ConnectOptions) -> Result<AnyConnection, AnyError> {
    let r = route::run_reporting(Request::Connect(options), path).map_err(any_error)?;
    Ok(AnyConnection {
        stream: UnixStream::from(r.fd),
        losses: losses(false, r.route, path),
        route: r.route.name(),
        skipped: r.skipped,
    })
}
