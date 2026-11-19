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
//! return std's own types. [`bind_ex`], [`connect_ex`] and
//! [`connect_nonblocking_ex`] do the same and also say how the path was
//! reached and what was given up for it, and [`bind_ex`] returns a
//! [`SocketFile`] that removes the socket file later. A path that std accepts is handed to std
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
//! A path longer than std accepts is bound through a symlink to the socket
//! file where a bind follows one (**symlink-file**; not Linux), else through
//! a directory of sockat's own, D (**the routes through D**), and only when
//! that cannot be used, through the socket's directory itself (**the form
//! without D**). A connect tries **the connect routes** that need neither
//! writing (making D, R or a symlink) nor a thread first, then the routes
//! through D and symlink-file. The routes carry the names of the
//! [specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md):
//!
//! | Platform | [`bind`] | [`connect`], [`connect_nonblocking`] |
//! |---|---|---|
//! | Linux, Android | thread-cwd, proc, fork-cwd | proc-direct, thread-cwd, proc, proc-held, symlink-file |
//! | Linux, Android without `/proc` | thread-cwd, fork-cwd | relative, thread-cwd, symlink-file |
//! | macOS, iOS (Apple) | symlink-file, thread-cwd, fork-cwd | relative, widened, thread-cwd, symlink-file |
//! | FreeBSD | symlink-file, at | at-direct, at |
//! | NetBSD, OpenBSD, illumos, other Unix | symlink-file, fork-cwd | relative, symlink-file |
//!
//! [`bind_ex`], [`connect_ex`] and [`connect_nonblocking_ex`] try the same,
//! and with [`LastResort::Allow`] a last resort after it: symlink-dir for
//! [`bind_ex`], fork-cwd-direct for the connects. The first route that can be
//! used makes the call; a real failure there (`ENOENT`, `EADDRINUSE`,
//! `ECONNREFUSED`, ...) is the result, not a reason to try the next.
//!
//! **symlink-file** (a bind, not Linux; a connect, not FreeBSD): a short
//! symlink `sockat.XXXXXXXXXX` to the socket file under `TMPDIR` or failing
//! that `/tmp`, removed right after the call, made only in a place where no
//! other user can replace it (a sticky directory owned by root or the calling
//! user, as `/tmp` is, or a directory of the calling user's that group and
//! others cannot write to). The spelling holds no file name, so it reaches any
//! file name length. A bind through it reaches the path only where the kernel
//! follows the symlink (macOS; Linux does not, and the route is not tried
//! there; the other targets are told at run time): when such a bind fails with
//! something at the path, that is the failure (`EADDRINUSE`, ...); with
//! nothing there, the kernel is taken not to have followed the symlink, and the
//! next route is tried.
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
//! * **fork-cwd** ([`bind`] only): a forked child that `fchdir`s into D binds
//!   and exits; the listen is made by the calling process, so a client sees
//!   the calling process as the server, as with any other route
//!
//! D and R are removed when the call returns, so the spelling the kernel keeps
//! for a bound socket names nothing afterwards, wherever it is resolved. D is
//! made at most once in a call and shared by every route that uses it; a
//! connect makes it only when no route before those reached the socket. D goes
//! in `TMPDIR` (or `/tmp` when it is unset), then `/tmp`, then the socket's
//! directory (named `.sockat.XXXXXXXXXX` there). None of them needs to be
//! safe or short: D is held by its fd. A D that cannot be opened right after
//! it is made (a umask took the owner's bits) is removed again by its name,
//! and the next place is tried.
//!
//! **The form without D.** When D can be made in no place, a bind makes R
//! (`.sockat.XXXXXXXXXX`) beside the socket instead and tries the same routes
//! (thread-cwd-sockdir or at-sockdir, proc-sockdir, fork-cwd-sockdir) with
//! `.sockat.XXXXXXXXXX/<file name>` relative to the socket's directory.
//!
//! `R/<file name>` takes 11 bytes, so the routes through D reach a file name
//! of up to 96 bytes (Linux, illumos) or 92 (Apple, the BSDs); 88 / 84 without
//! D (about 72 by proc-sockdir). On macOS a bind reaches any file name length
//! through symlink-file.
//!
//! **The connect routes** (connects only). A connected socket is not bound,
//! so its spelling is kept nowhere and needs no random part. The first four
//! need neither writing nor a thread and come first in a connect's order:
//!
//! * **proc-direct** (Linux with `/proc`):
//!   `/proc/self/fd/<dirfd>/<file name>`, for a file name of up to about 90
//!   bytes
//! * **relative**: the path spelled relative to the working directory, when
//!   std accepts that. Only the path string is rewritten; the cwd is never
//!   changed
//! * **widened** (Apple): a `sockaddr_un` carrying up to 253 bytes of path,
//!   which XNU accepts: the path itself, or failing that the relative
//!   spelling. A bind never uses it: the address of a socket bound so does not
//!   fit `sun_path` and breaks its readers (CPython, Go's `net.FileConn`)
//! * **at-direct** (FreeBSD): `connectat(2)` on the socket's directory fd with
//!   the file name (up to 103 bytes)
//! * **proc-held** (Linux with `/proc`): a symlink `s` to the socket file in
//!   D (the one a route before it made, or made as above), and the connect by
//!   `/proc/self/fd/<D's fd>/s`, whatever the directory depth and the file
//!   name length
//! * **symlink-file**, as above, last. Through `/tmp`, a connect is reached
//!   whatever the directory depth and the file name length
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
//! # The last resort: `LastResort::Allow`
//!
//! [`connect`] and [`connect_nonblocking`] never fork: a connect is not
//! expected to, and after a connect through a fork the peer pid the server
//! sees is that of a process that has already exited. Where only that would
//! reach the path, they fail with `ENAMETOOLONG`, as any connect to a path
//! too long for `sun_path` does. That happens only when the routes through D
//! cannot be used for a connect (no thread cwd and no `/proc`: NetBSD,
//! OpenBSD, illumos, Linux under a seccomp filter without `/proc`, an Apple
//! OS without `pthread_fchdir_np`) and none of the other connect routes
//! reaches the path (no safe place for the symlink, and the path fits neither
//! a relative spelling nor, on Apple, the widened sockaddr). [`bind`] fails with
//! `ENAMETOOLONG` where no route can be used.
//!
//! For that rare environment, when the path is wanted at the cost of
//! something, [`bind_ex`], [`connect_ex`] and [`connect_nonblocking_ex`] take
//! [`LastResort::Allow`]: [`bind_ex`] binds last through a symlink to the
//! socket's directory in a place (symlink-dir; in a shared place such as
//! `/tmp`, another user can make the reported address again), the connects
//! connect last from a forked child (fork-cwd-direct). What was given up is in
//! `losses` ([`Loss`]), with the name of the route taken and the routes passed
//! over ([`ExListener`], [`ExStream`]), and a failure is an [`ExError`] that
//! converts back into the `io::Error`. [`LastResort::Avoid`] tries only what
//! the plain function tries. [`bind`] (and [`bind_ex`] either way) forks as
//! fork-cwd when nothing before it can be used; the child only binds and the
//! listen is done by the calling process, so the listener is this process's
//! like any other, and nothing a client can see differs.
//!
//! # What differs from std for a long path
//!
//! * **The address the kernel reports is not the path.** `local_addr()` of the
//!   listener and of accepted streams, and `peer_addr()` of a connected client,
//!   report the spelling handed to the kernel when the listener was bound
//!   (whichever route the client took): `R/<file name>`,
//!   `/proc/self/fd/<n>/R/<file name>` or `.sockat.XXXXXXXXXX/<file name>`,
//!   `<place>/sockat.XXXXXXXXXX` after symlink-file
//!   (`<place>/sockat.XXXXXXXXXX/<file name>` after symlink-dir). R
//!   is gone by then, so removing, `stat`ing or `chmod`ing by the reported
//!   address finds nothing rather than another file; it never names the
//!   socket either. Keep the path you passed and use that (for example to
//!   remove the socket file with [`std::fs::remove_file`], which has no length
//!   limit, or with [`bind_ex`]). The same address shows in `Debug` output and
//!   in what frameworks log or hand out as the peer address.
//! * **A bind reaches a shorter file name than a connect** where it does not
//!   go through symlink-file: 96 bytes on Linux and illumos, 92 on Apple and
//!   the BSDs (`R/` takes 11 of `sun_path`), where a connect reaches any. On
//!   macOS a bind reaches any too.
//! * **A path that does not end with a file name is `InvalidInput`.** A path
//!   longer than std accepts ending in `/`, `/.` or `/..` (or being `..`)
//!   cannot be split into a directory and a file name and is refused before
//!   anything is created; a shorter one goes to std and gets the kernel's error.
//! * **The peer pid the server sees can be that of an exited process**
//!   ([`connect_ex`] and [`connect_nonblocking_ex`] with
//!   [`LastResort::Allow`], when they take fork-cwd-direct): the forked child
//!   that made the connect and exited. The socket itself belongs to the caller and works as usual. The uid and gid the
//!   server sees are the caller's. On macOS (`LOCAL_PEERPID`) the server sees
//!   the caller's pid once the caller sends on the socket; on Linux
//!   (`SO_PEERCRED`) it stays the exited child's. The number of an exited pid
//!   may be reused by another process later. Nothing can be looked up from
//!   that pid (the process, its parent): the process is gone.
//! * **The fork routes have side effects no peer sees** ([`bind`],
//!   [`bind_ex`], and the connects' last resort). As with any `fork(2)`, the
//!   child has copies of all file descriptors until it exits a moment later, so a socket or pipe
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
//!   proc-direct, relative, at-direct and fork-cwd-direct routes of a connect look
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
//!   call: D (`sockat.XXXXXXXXXX`) or a temporary symlink under
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
//! The `_ex` functions return them as `losses` ([`Loss`]); [`bind`],
//! [`connect`] and [`connect_nonblocking`] lose the same on the routes they
//! take, without returning it.
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
//!     let bound = sockat::bind_ex(path, sockat::LastResort::Avoid)?;
//!     // accept on bound.listener (it may go to tokio etc.)
//!     drop(bound.listener);
//!     bound.file.discard() // removes the file only while it is still the one bound
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
//!     let bound = sockat::bind_ex(&path, sockat::LastResort::Avoid)?;
//!     // accept on bound.listener
//!     drop(bound.listener);
//!     bound.file.discard()
//! }
//! # let dir = tempfile::tempdir()?;
//! # serve(&dir.path().join("s.sock"))?;
//! # assert_eq!(std::fs::read_dir(dir.path())?.count(), 0);
//! # Ok::<(), io::Error>(())
//! ```
//!
//! ## forked
//!
//! A forked child made the call (fork-cwd, fork-cwd-sockdir, fork-cwd-direct),
//! with the side effects of a fork (see [What differs from std for a long
//! path](#what-differs-from-std-for-a-long-path)). A `SIGCHLD` handler or a
//! `waitpid(-1)` may reap the child or mistake the result. Do not use
//! `waitpid(-1)` in other threads (wait by pid). A connect forks only as the
//! last resort: do not pass [`LastResort::Allow`] where `forked` cannot be
//! accepted. A bind's fork-cwd is one of its routes, so a bind cannot avoid it
//! where it is the route that reaches the path.
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
//!     sockat::connect(path) // a connect that does not fork (no last resort)
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
//! [`bind_ex`]'s last resort took symlink-dir in a shared place such as
//! `/tmp`: the spelling the kernel reports goes through it, and another user
//! can make it again once seen. Remove by [`SocketFile::discard`] or the
//! original path, not by that spelling. A place only the owner can write to
//! (`$XDG_RUNTIME_DIR`, or a `$TMPDIR` only the owner can write to) is used
//! first, and then this name is not given.
//!
//! ```
//! use std::io;
//! use std::path::Path;
//!
//! fn serve(path: &Path) -> io::Result<()> {
//!     let bound = sockat::bind_ex(path, sockat::LastResort::Allow)?;
//!     // accept on bound.listener
//!     drop(bound.listener);
//!     bound.file.discard() // not by local_addr()'s spelling
//! }
//! # let dir = tempfile::tempdir()?;
//! # let deep = dir.path().join("d".repeat(100));
//! # std::fs::create_dir(&deep)?;
//! # serve(&deep.join("s.sock"))?;
//! # assert_eq!(std::fs::read_dir(&deep)?.count(), 0);
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
//! * branching on an `_ex` function's `route` or `skipped`: branch on
//!   `losses`;
//! * reaping children with `waitpid(-1)` on another thread of a program whose
//!   calls may fork: wait by pid.
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
//!     let bound = sockat::bind_ex(path, sockat::LastResort::Avoid)?;
//!     // discard() does not close the listener
//!     bound.file.discard()?;
//!     let _still_open = bound.listener;
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
//! fn wrong_ex(path: &Path) -> io::Result<()> {
//!     let bound = sockat::bind_ex(path, sockat::LastResort::Allow)?;
//!     // branching on the route's string: it promises nothing (branch on losses)
//!     if bound.route == "symlink-dir" {
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
//! name too long for `R/<file name>` where symlink-file cannot be used; for
//! [`connect`] and [`connect_nonblocking`], also when only a forked child
//! would), the error is `ENAMETOOLONG`. The `_ex` functions return the same
//! errors as an [`ExError`], which also carries the routes tried and converts
//! into the `io::Error` by `?`. A path longer than std accepts that does not end with a
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

use route::{BindOptions, ConnectOptions, Request, Route};

mod path;
mod route;
mod sys;

#[cfg(test)]
mod tests;

pub use route::Skipped;

/// Create a socket bound to `path` and listening on it, like
/// [`UnixListener::bind`] but for a path of any length.
///
/// A path that std accepts goes to [`UnixListener::bind`] unchanged. A longer
/// path is reached by the routes the platform has, fork-cwd last (see the
/// [crate documentation](crate)): the forked child only binds, and the listen
/// is done by this process, so the listener is this process's like any other.
/// The socket file is created at `path` either way, and is not removed when
/// the listener is dropped (as with std; [`bind_ex`] returns a
/// [`SocketFile`] that removes it).
///
/// # Errors
///
/// The errors of `bind(2)` / `listen(2)`, for example `EADDRINUSE` when `path`
/// exists and `ENOENT` when its directory does not; `ENAMETOOLONG` when no
/// route reaches the path (a file name too long for `R/<file name>` where no
/// symlink-file bind can be used); `InvalidInput` for a path longer than std
/// accepts that does not end with a file name, or for a path that contains a
/// NUL byte.
pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<UnixListener> {
    let options = BindOptions { last_resort: false };
    route::run(Request::Bind(options), path.as_ref()).map(|(fd, _)| UnixListener::from(fd))
}

/// Connect to the socket at `path`, like [`UnixStream::connect`] but for a path
/// of any length, without forking. Use it wherever std's would be used.
///
/// A path that std accepts goes to [`UnixStream::connect`] unchanged. A longer
/// path is reached by the connect order of the platform (see the
/// [crate documentation](crate)), so in an ordinary environment it is reached
/// whatever the directory depth and the file name length, and the connection
/// is always made by this process. Only in a rare environment where nothing but
/// a forked child would reach the path is the error `ENAMETOOLONG`, as for any
/// connect to a path too long for `sun_path` ([`connect_ex`] with
/// [`LastResort::Allow`] is for that case).
///
/// The stream blocks, like std's. In async code use [`connect_nonblocking`].
///
/// # Errors
///
/// The errors of `connect(2)`, for example `ENOENT` when nothing is at `path`
/// and `ECONNREFUSED` when nobody listens on it (on macOS also when its backlog
/// is full); `ENAMETOOLONG` when no route of the connect order reaches the
/// path; `InvalidInput` for a path longer than std accepts that does not end
/// with a file name, or for a path that contains a NUL byte.
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

/// The two plain connects: block or not, never the last resort.
fn connect_with(path: &Path, nonblocking: bool) -> io::Result<UnixStream> {
    let options = ConnectOptions {
        nonblocking,
        last_resort: false,
    };
    route::run(Request::Connect(options), path).map(|(fd, _)| UnixStream::from(fd))
}

/// Whether an `_ex` function may take its last resort: the route that reaches
/// the path at the cost of something, tried once every other route is
/// unusable.
///
/// | function | last resort | what it gives up |
/// |---|---|---|
/// | [`bind_ex`] | symlink-dir: a symlink in a place to the socket's directory | [`Loss::Respelled`], and in a shared place such as `/tmp` [`Loss::RecreatableSpelling`] |
/// | [`connect_ex`], [`connect_nonblocking_ex`] | fork-cwd-direct: a forked child connects | [`Loss::Forked`] (the peer pid the server sees is an exited child's) |
///
/// [`LastResort::Avoid`] (the [`Default`]) tries what the plain function
/// tries, in the same order. Passing [`LastResort::Allow`] is the choice to
/// accept what the last resort gives up; [`ExListener::losses`] /
/// [`ExStream::losses`] say what was given up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum LastResort {
    /// Do not take the last resort: where only it would reach the path, the
    /// call fails with `ENAMETOOLONG`, as the plain function does.
    #[default]
    Avoid,
    /// Take the last resort when no other route reaches the path.
    Allow,
}

impl LastResort {
    fn allowed(self) -> bool {
        self == LastResort::Allow
    }
}

/// Something given up, compared with std, to reach the path (the SPEC's
/// "失うもの"). An `_ex` function returns those of the route it took, as a
/// list ([`ExListener::losses`], [`ExStream::losses`]); none of them implies
/// another, and they have no order.
///
/// The names are promised and only ever added to (the enum is
/// `#[non_exhaustive]`): read one you do not know as "something was given
/// up". [`Loss::as_str`] and `Display` give the SPEC's name.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Loss {
    /// `respelled`: the address the kernel reports for the bound socket is
    /// not the path (see the [crate documentation](crate)). It names nothing
    /// once the call has returned, but it can be seen by other users (Linux's
    /// `/proc/net/unix`), and where the process's cwd is writable by others
    /// they can make it again; remove the socket file by the path you passed
    /// or [`SocketFile::discard`], not by the reported address. Only a bind
    /// gives this up.
    Respelled,
    /// `forked`: a forked child made the call, with the side effects of a
    /// fork (see the [crate documentation](crate)). After a connect, the peer
    /// pid the server sees is the exited child's.
    Forked,
    /// `recreatable-spelling`: the address the kernel reports goes through a
    /// shared place such as `/tmp` ([`bind_ex`]'s last resort, symlink-dir),
    /// where another user can make it again once sockat removed it: whatever
    /// later removes the socket file by that address could remove a file of
    /// that user's choosing. Remove it by the path you passed or
    /// [`SocketFile::discard`].
    RecreatableSpelling,
    /// `cwd-relative`: a relative path went to std as it is, so the address
    /// the kernel keeps for the bound socket is relative to the cwd: after the
    /// cwd changes, removing or looking up the socket by it reaches another
    /// place. std has this risk on its own; it is reported all the same. Not
    /// for a name in Linux's abstract namespace.
    CwdRelative,
}

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

impl std::fmt::Display for Loss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the route `route` gave up for a bind (`bind`) or a connect to `path`
/// (SPEC "経路の名前", the column "失うもの").
fn losses(bind: bool, route: Route, path: &Path) -> Vec<Loss> {
    match (bind, route) {
        (true, Route::Standard) if !path.is_absolute() && !path::is_abstract(path) => {
            vec![Loss::CwdRelative]
        }
        (_, Route::Standard) => Vec::new(),
        (true, Route::SymlinkDir { shared: true }) => {
            vec![Loss::Respelled, Loss::RecreatableSpelling]
        }
        (true, r) if r.forked() => vec![Loss::Respelled, Loss::Forked],
        (true, _) => vec![Loss::Respelled],
        (false, r) if r.forked() => vec![Loss::Forked],
        (false, _) => Vec::new(),
    }
}

/// What [`bind_ex`] returns: the listener, the [`SocketFile`] that removes
/// the socket file later, what was given up to bind it, and how it was bound.
///
/// The fields are public so the value can be taken apart as it comes. Later
/// versions may add fields (the struct is `#[non_exhaustive]`), so a pattern
/// needs `..`:
///
/// ```no_run
/// use sockat::{ExListener, LastResort, Loss};
///
/// let path = "/a/very/long/directory/.../server.sock";
/// let ExListener { listener, file, losses, .. } = sockat::bind_ex(path, LastResort::Avoid)?;
/// if losses.contains(&Loss::Respelled) {
///     // the address the kernel reports is not `path`: remove by `file`
/// }
/// // ... serve on `listener` ...
/// drop(listener);
/// file.discard()?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[non_exhaustive]
#[derive(Debug)]
pub struct ExListener {
    /// The listening socket, std's own type, as [`bind`] returns it.
    pub listener: UnixListener,
    /// Removes the socket file later, only while it is still the one this
    /// bind made ([`SocketFile::discard`]). Closing the listener stays with
    /// whoever has it.
    pub file: SocketFile,
    /// What was given up to bind it ([`Loss`]), empty when nothing was.
    /// Branch on this.
    pub losses: Vec<Loss>,
    /// The name of the route that bound it (the SPEC's "経路の名前", such as
    /// `"thread-cwd"` or `"symlink-file"`). For diagnostics only: routes may be
    /// added, renamed or changed, so do not branch on it.
    pub route: &'static str,
    /// The routes tried before [`ExListener::route`] and passed over, in the
    /// order tried, each with why ([`Skipped`]): those of the order this call
    /// tries on this platform in its state (no /proc route on a Linux without
    /// /proc; the last resort only when asked for), `standard` first for a
    /// long path. Empty for a path std took. For diagnostics only, like
    /// [`ExListener::route`].
    pub skipped: Vec<Skipped>,
}

/// What [`connect_ex`] and [`connect_nonblocking_ex`] return: the stream,
/// what was given up to connect it, and how it was connected.
///
/// The fields are public so the value can be taken apart as it comes. Later
/// versions may add fields (the struct is `#[non_exhaustive]`), so a pattern
/// needs `..`:
///
/// ```no_run
/// use sockat::{ExStream, LastResort, Loss};
///
/// let path = "/a/very/long/directory/.../server.sock";
/// let ExStream { stream, losses, .. } = sockat::connect_ex(path, LastResort::Allow)?;
/// if losses.contains(&Loss::Forked) {
///     eprintln!("connected to {path} through a forked child");
/// }
/// # drop(stream);
/// # Ok::<(), sockat::ExError>(())
/// ```
#[non_exhaustive]
#[derive(Debug)]
pub struct ExStream {
    /// The connected stream, std's own type. It belongs to this process and
    /// works as usual whichever route made the connection.
    pub stream: UnixStream,
    /// What was given up to connect it ([`Loss`]): [`Loss::Forked`] when the
    /// last resort, a forked child that has exited since, made the
    /// connection, after which the peer pid the server sees is that child's
    /// (see [`connect_ex`]); empty when this process made it (a connect's
    /// spelling is kept nowhere). Branch on this.
    pub losses: Vec<Loss>,
    /// The name of the route that connected it, as [`ExListener::route`].
    pub route: &'static str,
    /// The routes passed over before it, as [`ExListener::skipped`].
    pub skipped: Vec<Skipped>,
}

/// Why an `_ex` function did not reach the path: the [`io::Error`], and the
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
/// match sockat::connect_ex(path, sockat::LastResort::Avoid) {
///     Ok(c) => drop(c.stream),
///     Err(e) => eprintln!("{e} (errno {:?}) after {:?}", e.error.raw_os_error(), e.skipped),
/// }
/// ```
#[non_exhaustive]
#[derive(Debug)]
pub struct ExError {
    /// The error, as the plain function returns it (for example `ENOENT`, or
    /// `ENAMETOOLONG` when no route could be used).
    pub error: io::Error,
    /// The name of the route that made the call and failed for real (an
    /// existing file: `EADDRINUSE`, nobody listening: `ECONNREFUSED`, ...;
    /// `"standard"` when std failed on a path it took); `None` when no route
    /// could be used, or the path was refused before any was tried (a NUL
    /// byte, no file name, the socket's directory missing or not one). For
    /// diagnostics only, as [`ExListener::route`].
    pub route: Option<&'static str>,
    /// The routes tried and passed over, in the order tried, as
    /// [`ExListener::skipped`]; empty when the path was refused before any
    /// route was tried (the length is judged only once the socket's directory
    /// is looked up).
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

/// The [`ExError`] of a call that did not reach the path.
fn ex_error(f: route::Failed) -> ExError {
    ExError {
        error: f.error,
        route: f.route,
        skipped: f.skipped,
    }
}

/// [`bind`], with a [`SocketFile`] that removes the socket file later, what
/// was given up and how the path was bound ([`ExListener`]); with
/// [`LastResort::Allow`], also the last resort, symlink-dir, when no route of
/// [`bind`] can be used.
///
/// With [`LastResort::Avoid`] the routes and their order are [`bind`]'s. With
/// [`LastResort::Allow`], where [`bind`] would be `ENAMETOOLONG` (sockat's own
/// directory can be made in no place, and the socket's directory cannot be
/// opened or R made in it), this makes a symlink `sockat.XXXXXXXXXX` (10
/// random letters and digits) to the real path of the socket's directory in a
/// place, binds by `<place>/sockat.XXXXXXXXXX/<file name>`, and removes the
/// symlink. The places are `XDG_RUNTIME_DIR` and `TMPDIR` (each when set to an
/// absolute path) and `/tmp`: first those only this user can write to (owned
/// by this user, without write permission for group and others), then the
/// shared ones (sticky and owned by root or this user, as `/tmp` is). A place
/// that is neither, or too long for the spelling, is passed over. Through a
/// shared place another user can make the reported address again once the
/// symlink is gone ([`Loss::RecreatableSpelling`]); remove the socket file by
/// [`SocketFile::discard`] or the path you passed.
///
/// The listener is std's own type, free to go wherever [`bind`]'s would (into
/// `tokio::net::UnixListener::from_std`, another thread, a server
/// framework); closing it stays with its owner. The [`SocketFile`] only
/// removes the file, typically once the listener is closed. The socket file's
/// real path and its `(st_dev, st_ino)` are taken right after the bind, before
/// this returns: for a path handed to std, as soon as std's bind returns; for
/// a long path, inside the route (through D while sockat's own directory and
/// the symlink in it are still there, by the same spelling the bind took).
/// Someone who can write to the socket's directory and replaces the file in
/// the moment between the bind and that look-up is not caught: POSIX has no
/// way to name a file by its inode.
///
/// ```no_run
/// let path = "/a/very/long/directory/.../server.sock";
/// let bound = sockat::bind_ex(path, sockat::LastResort::Avoid)?;
/// // ... serve on `bound.listener` ...
/// drop(bound.listener);
/// bound.file.discard()?;
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// # Errors
///
/// Those of [`bind`] (with [`LastResort::Allow`], a path symlink-dir reaches is
/// bound rather than `ENAMETOOLONG`), and those of looking the socket file up
/// right after the bind; then the listener is closed and the socket file stays
/// (as after [`bind`]), so that no [`SocketFile`] is handed out that could not
/// remove it. The same for a relative `path` when the cwd's `(st_dev,
/// st_ino)` differs before the bind and the realpath and after both (another
/// thread changed it, so the file looked up need not be the one bound), as an
/// `Other` error. As an [`ExError`] with the routes tried (it converts into
/// the [`io::Error`] by `?`).
pub fn bind_ex<P: AsRef<Path>>(path: P, last_resort: LastResort) -> Result<ExListener, ExError> {
    let path = path.as_ref();
    let options = BindOptions {
        last_resort: last_resort.allowed(),
    };
    let r = route::run_reporting(Request::Bind(options), path).map_err(ex_error)?;
    let route = r.route;
    let Some(bound) = r.bound else {
        return Err(ExError {
            error: io::Error::other("the socket file was not looked up"),
            route: Some(route.name()),
            skipped: r.skipped,
        });
    };
    Ok(ExListener {
        listener: UnixListener::from(r.fd),
        file: SocketFile {
            path: bound.path,
            id: bound.id,
        },
        losses: losses(true, route, path),
        route: route.name(),
        skipped: r.skipped,
    })
}

/// [`connect`], with what was given up and how it connected
/// ([`ExStream`]); with [`LastResort::Allow`], also the last resort,
/// fork-cwd-direct, when no route of [`connect`] reaches the path.
///
/// With [`LastResort::Avoid`] the routes and their order are [`connect`]'s.
/// [`LastResort::Allow`] is for the rare environment where only a forked
/// child reaches the path (sockat's own directory cannot be used for a
/// connect, and none of the other connect routes reaches the path; see the
/// [crate documentation](crate)), when the connection is wanted at the cost of
/// the fork's side effects. The forked child `fchdir`s into the socket's
/// directory, connects by the file name and exits at once.
/// [`ExStream::losses`] holds [`Loss::Forked`] when it was taken; on every
/// other route the connection was made by this process, and nothing is given
/// up.
///
/// After the fork, the peer pid the server sees is that of the child, a
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
/// Those of [`connect`] (with [`LastResort::Allow`], a path only the fork
/// reaches is connected rather than `ENAMETOOLONG`), as an [`ExError`] with the
/// routes tried (it converts into the [`io::Error`] by `?`).
pub fn connect_ex<P: AsRef<Path>>(path: P, last_resort: LastResort) -> Result<ExStream, ExError> {
    connect_ex_with(path.as_ref(), false, last_resort)
}

/// [`connect_ex`] on a nonblocking socket, as [`connect_nonblocking`] is to
/// [`connect`]. With [`LastResort::Allow`] the socket is made nonblocking
/// before the fork, so the child's connect does not wait either.
///
/// # Errors
///
/// Those of [`connect_ex`], and `EAGAIN` for a full backlog on Linux.
pub fn connect_nonblocking_ex<P: AsRef<Path>>(
    path: P,
    last_resort: LastResort,
) -> Result<ExStream, ExError> {
    connect_ex_with(path.as_ref(), true, last_resort)
}

/// The two `_ex` connects: block or not.
fn connect_ex_with(
    path: &Path,
    nonblocking: bool,
    last_resort: LastResort,
) -> Result<ExStream, ExError> {
    let options = ConnectOptions {
        nonblocking,
        last_resort: last_resort.allowed(),
    };
    let r = route::run_reporting(Request::Connect(options), path).map_err(ex_error)?;
    Ok(ExStream {
        stream: UnixStream::from(r.fd),
        losses: losses(false, r.route, path),
        route: r.route.name(),
        skipped: r.skipped,
    })
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
