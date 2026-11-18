# sockat manual

> English | [日本語](./MANUAL-ja.md)

How to use sockat, how it reaches a long path, and what differs from std for a long path. The decisions and their reasons are in the [DR index](./decisions/INDEX.md) (the routes and their order as a whole: [DR-0027](./decisions/DR-0027-own-directory-and-fallback-routes.md), in Japanese).

The language-independent behaviour is defined by [the sockat spec](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md); sockat-rs is its Rust implementation.

## Usage

```toml
[dependencies]
sockat = "0.4"
```

Use `sockat::bind` / `sockat::connect` where you would use std's `bind` / `connect`. In async code, use `sockat::connect_nonblocking`.

| Function | std counterpart | Returns |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listening) |
| `sockat::bind_ex(path)` | (none) | `io::Result<(UnixListener, SocketFile)>` ([below](#removing-the-socket-file-bind_ex)) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |
| `sockat::connect_nonblocking(path)` | (none) | `io::Result<UnixStream>` (nonblocking) |

For a rare environment there are also `connect_any` / `connect_any_nonblocking`, behind the feature `any` ([below](#the-exception-connect_any)).

```rust
use std::io::{Read, Write};

let path = "/a/very/deep/directory/.../server.sock";
let listener = sockat::bind(path)?;
let mut client = sockat::connect(path)?;
let (mut server, _) = listener.accept()?;
client.write_all(b"ping")?;
let mut buf = [0; 4];
server.read_exact(&mut buf)?;
```

- A path std accepts ([the length per OS](#how-a-long-path-is-reached)) goes to std's function unchanged (`connect_nonblocking`, which std does not have, connects sockat's own socket with the address std would use)
- A path containing a NUL is `InvalidInput` at any length, with nothing created ([Paths containing a NUL](#paths-containing-a-nul))
- The socket file is created at the path given. It stays when the listener is closed (as with std). To remove it, call `std::fs::remove_file(path)` with the original path, or use `bind_ex`
- sockat's own sockets are set up as std's are (close-on-exec; `SO_NOSIGPIPE` where std sets it: macOS / iOS, FreeBSD, NetBSD, DragonFly; the same listen backlog as std)

## connect and connect_nonblocking

- **`connect`** replaces std's `UnixStream::connect`. It blocks
- **`connect_nonblocking`** is for async runtimes. The socket is made nonblocking before the connect and returned as it is, ready for tokio's `UnixStream::from_std`. When the peer's backlog is full it fails at once (`EAGAIN` on Linux, `ECONNREFUSED` on macOS). A Unix-domain connect completes or fails within the call, so there is nothing to wait for afterwards (checked on Linux and macOS)

Neither takes the fork route ([How a long path is reached](#how-a-long-path-is-reached)). Where only the fork route would reach the path, they return `ENAMETOOLONG`, as an ordinary connect to a path too long for `sun_path` does. In an ordinary environment (a writable `/tmp`), they reach a path whatever the directory depth and the file name length ([Where reaching stops](#where-reaching-stops)).

## How a long path is reached

A path of up to the length std accepts goes to std unchanged. With today's std that is, on every OS, the length that fits the plain sockaddr (`sun_path` − 1, "the sockaddr length" below).

| OS | Handed to std up to |
|---|---|
| Linux / Android / illumos | 107 bytes |
| macOS / iOS and other Apple OSes / FreeBSD / NetBSD / OpenBSD | 103 bytes |

A longer path is reached first by [the central route](#the-central-route), through a directory of sockat's own, and when that cannot be used, for the connects only, by [the fallback routes](#the-fallback-routes-connects). The first route that can be used makes the call. None of the routes changes the working directory of the calling process or of the calling thread, so they can be used from multi-threaded programs.

### Order per OS

| OS | `bind` | `connect` / `connect_nonblocking` |
|---|---|---|
| Linux / Android | in D: thread cwd → /proc version → [fork](#fork) ([the central route](#the-central-route)) | in D: thread cwd → /proc version; then [/proc](#proc) → [a symlink to the file in D](#a-symlink-to-the-file-in-d) → [symlink](#symlink) |
| Linux / Android (no `/proc`) | in D: thread cwd → fork | in D: thread cwd; then [relative path](#relative-path) → symlink |
| macOS / iOS and other Apple OSes | in D: thread cwd → fork | in D: thread cwd; then relative path → [widened sockaddr](#widened-sockaddr) → symlink |
| FreeBSD | in D: bindat | in D: connectat; then [connectat](#connectat) |
| NetBSD / OpenBSD / illumos / other Unix | in D: fork | relative path → symlink |
| Windows | not supported (compile error outside Unix) | |

- `connect_any` and `connect_any_nonblocking` try what `connect` does, then connect through [fork](#fork) ([The exception: connect_any](#the-exception-connect_any))
- The socket's directory is looked up (`stat`) first. If it is missing, the error is `ENOENT` (whatever the file name length)
- When a route actually binds / connects and fails (`ENOENT`, `EADDRINUSE`, `ECONNREFUSED`, ...), that error is returned and nothing else is tried. The next one is tried only when a route cannot be used at all

### The central route

sockat makes a directory D of its own, holds it by an fd, makes in it a symlink R with a random name to the real path of the socket's directory, and binds / connects with `R/<file name>` relative to D's fd. R and D are removed afterwards.

1. Create a directory `sockat.XXXXXXXXXX` (10 random letters and digits) of mode 0700 in a place for D
2. Open it without following a symlink (`O_NOFOLLOW`) and check that it is a directory owned by the calling user with no permission for group and others. Otherwise (someone else replaced it between the two steps) it is neither used nor removed. Once checked, set it to 0700 (so a umask that took some of the owner's bits does not get in the way)
3. Create R in D
4. Bind / connect `R/<file name>` by the first of the means below that can be used
5. Whatever the outcome, remove R, and remove D only while its name still names the directory that was opened

| Means | OS | How |
|---|---|---|
| thread cwd | Linux / Android, Apple OSes | A thread started for this call alone moves its own working directory, and only its own, into D (`unshare(CLONE_FS)` then `fchdir` on Linux, `pthread_fchdir_np` on Apple). The thread ends with the call |
| /proc version | Linux / Android with `/proc` | `/proc/self/fd/<D's fd>/R/<file name>` is handed to std |
| bindat | FreeBSD | `bindat(2)` / `connectat(2)` with D's fd |
| fork | all (bind only) | A [forked](#fork) child `fchdir`s into D and binds |

- **The places for D** are `TMPDIR` (`/tmp` when it is unset; a relative one is fine), `/tmp`, then the socket's directory (named `.sockat.XXXXXXXXXX` there), in that order. The first where D can be made is used. A place needs to be neither safe (sticky etc.) nor short: D is held by its fd and the place never appears in a spelling
- When a place cannot be opened, D cannot be created, the check fails, D cannot be set to 0700 or R cannot be created, the next place is tried
- **When D can be made nowhere**, a bind creates R (`.sockat.XXXXXXXXXX`) in the socket's directory and binds `.sockat.XXXXXXXXXX/<file name>` relative to the socket's directory instead of D, by the same means in the same order (thread cwd (bindat on FreeBSD), the /proc version `/proc/self/fd/<fd>/.sockat.XXXXXXXXXX/<file name>`, [fork](#fork)). When the socket's directory cannot be opened or R cannot be made, it is `ENAMETOOLONG`. A connect goes on to the fallback routes
- The spelling is `R/<file name>`, so the file name has R's 11 bytes less than the sockaddr length (up to 96 bytes on Linux / illumos, 92 on Apple OSes / the BSDs; about 79 for the /proc version. Without D 88 / 84, about 72 for its /proc version)
- **Every function takes the thread-cwd means.** The calling process makes the call, so the peer pid the server sees is the caller's, and no fd is copied. `connect_any` reports `forked` as `false` on it
- The thread-cwd means cannot be used, and the next means is tried, when:
  - on Linux a seccomp filter is in effect (docker's default installs one). A filter may refuse `unshare` by killing the process (SIGSYS) rather than with an error (systemd's `SystemCallFilter=` does by default), so `unshare` is not called at all then
  - on Linux `unshare` is refused
  - on Apple `pthread_fchdir_np` is missing or fails (it is not in a public header, so it is looked up at run time)
  - no thread can be started
- D is opened `O_PATH` on Linux, `O_SEARCH` on Apple OSes, `O_RDONLY` elsewhere. It is set to 0700 by `chmod` of `/proc/self/fd/<fd>` on Linux with `/proc`, by `fchmod` of D opened again by name `O_RDONLY` and checked to be the same directory on Linux without `/proc`, by `fchmod` elsewhere
- The central route (the /proc version included) does not open the socket's directory (only D made in it and the form without D do); R reaches it with search permission alone. A directory without read permission is reached with the same permission as a plain bind / connect

### The fallback routes (connects)

Tried by the connects only, when the central route cannot be used (no means usable, D made nowhere, the file name too long once `R/` is added). A connected socket is not bound and its spelling is kept nowhere, so these spellings carry no random part.

#### /proc

Linux / Android. The socket's directory is opened `O_PATH` and `/proc/self/fd/<fd>/<file name>` is handed to std. The whole spelling must be at most 107 bytes, so the file name about 90 bytes at most (it depends on the number of digits of the fd). When the socket's directory cannot be opened, the next route is tried.

#### A symlink to the file in D

Linux / Android with `/proc`. D is made and checked as on the central route, a symlink `s` to the socket file is made in it, and the connect goes through `/proc/self/fd/<D's fd>/s`. The spelling is about 20 bytes whatever the file name length and the directory depth. The places for D are those of the central route (none needs to be safe). The socket's directory is not opened.

#### Relative path

When the path spelled relative to the cwd fits the sockaddr length, that is handed to std. Only when the original path is absolute. Only the string is rewritten; the cwd is never changed (no `chdir`).

- If the path is below the cwd (`getcwd`, or a `$PWD` that names the same directory), its front is cut off. When that does not fit the sockaddr length (including when the path is not below the cwd), a spelling that climbs with `..` between the real paths of both is made too. The shorter one is used
- When the relative spelling's directory is not the original directory by `(st_dev, st_ino)`, or the cwd has already changed, the route cannot be used and the next one is tried
- The cwd is checked again after the connect ([below](#the-relative-route-is-exposed-to-another-threads-chdir))

#### Widened sockaddr

Apple OSes. XNU accepts an address whose `sun_path` is over 104 bytes, up to a path of 253 bytes. The path itself is handed over when it is at most 253 bytes, otherwise the relative spelling when that is. When the kernel refuses the form (`EINVAL` / `ENAMETOOLONG`), the next route is tried.

`bind` never uses it. The address of a socket bound in the widened form does not fit a `sockaddr_un`; `getsockname` returns it cut short, and readers of it break (CPython's asyncio cleaning up, Go's `net.FileConn`, ...).

#### Symlink

A symlink `sockat.XXXXXXXXXX` to the socket file is made in a temporary directory, the connect goes through it, and it is removed right away.

- The spelling is `<place>/sockat.XXXXXXXXXX`: 22 bytes under `/tmp`, whatever the directory depth and the file name length
- The places tried are `TMPDIR` (when it is a non-empty absolute path), then `/tmp`. The first one where the spelling fits the sockaddr length, that is safe and where the symlink can be created is used. When `TMPDIR` is too deep, `/tmp` is used
- A **safe place** is a directory in which no other user can replace the symlink (anything but a directory is not safe), either of the following. Nothing is created in a place that is not; the next one is tried
  - sticky, owned by root or by the calling user (as `/tmp` is)
  - owned by the calling user, with no write permission for group and others (as macOS's `$TMPDIR` is)
- The place is judged through symlinks (macOS's `/tmp` is a symlink to `/private/tmp`)
- When the symlink cannot be created (the place is missing or not writable, a sandbox, no free name found, no random bytes), the next place is tried as well

#### connectat

FreeBSD. The socket's directory is opened and the file name handed to `connectat(2)`. File names of up to 103 bytes.

### fork

A child forked after an `fchdir` to a directory fd binds / connects by a spelling and exits at once. After the fork the parent and the child share the socket, so the child's bind / connect takes effect on the caller's socket. The listen after a bind is always made by the caller.

| Request | The child `fchdir`s into | Spelling |
|---|---|---|
| `bind` | D | `R/<file name>` |
| `bind` (D made nowhere) | the socket's directory | `.sockat.XXXXXXXXXX/<file name>` |
| `connect_any` and `connect_any_nonblocking` (after the fallback routes) | the socket's directory | the file name |

- **Only `bind` and the `connect_any` ones take it.** `connect` / `connect_nonblocking` return `ENAMETOOLONG` instead
- The file name of the `connect_any` ones fits the sockaddr length (on Apple, 253 bytes while the kernel takes the widened sockaddr)
- On the fork route, the `connect_any` ones and the fork without D open the socket's directory. macOS opens it `O_SEARCH` and Linux `O_PATH`, which need no more permission than a plain bind / connect; other Unix targets open it `O_RDONLY`, which needs read permission as well (when it cannot be opened, the route cannot be used)
- For a bind the child only binds and the caller listens, so the fork route has no side effect a peer sees. The server's peer a client sees (macOS's `LOCAL_PEERPID`, Linux's `SO_PEERCRED`) is the caller from before anything is sent ([DR-0025](./decisions/DR-0025-fork-bind-listen-by-parent.md)). There is no bind that never forks
- The child makes only async-signal-safe calls (`fchdir`, `bind` / `connect`, `_exit`), so it can be used from multi-threaded processes
- Side effects: [the peer pid of a connect](#side-effects-of-a-connect-through-the-fork-route) (seen by the peer, connects only), [side effects no peer sees](#side-effects-of-the-fork-route-that-no-peer-sees) (binds and connects)

### Where reaching stops

When a path too long for std is `ENAMETOOLONG` (assuming the directory exists and D can be made; a missing directory is `ENOENT`).

| OS | `bind` | `connect` / `connect_nonblocking` | `connect_any` ones |
|---|---|---|---|
| Linux / Android (`/proc`) | file name over 96 bytes | always reached (a symlink to the file in D) | always reached |
| Linux / Android (no `/proc`) | file name over 96 bytes | the central route unusable (file name over 96 bytes, or the thread-cwd means unusable), the relative path over 107 bytes, and no symlink | file name over 107 bytes, and no symlink |
| FreeBSD | file name over 92 bytes | file name over 103 bytes | file name over 103 bytes |
| macOS / iOS and other Apple OSes | file name over 92 bytes | the central route unusable (file name over 92 bytes, or the thread-cwd means unusable), the path and the relative path over 253 bytes, and no symlink | file name over 253 bytes, and no symlink |
| NetBSD / OpenBSD / other BSDs | file name over 92 bytes | the relative path over 103 bytes, and no symlink | file name over 103 bytes, and no symlink |
| illumos | file name over 96 bytes | the relative path over 107 bytes, and no symlink | file name over 107 bytes, and no symlink |

- On Linux without the thread-cwd means (a seccomp filter, ...), a bind takes the /proc version for a file name of up to about 79 bytes and the fork route up to 96
- When D can be made nowhere, a bind reaches a file name of up to 88 bytes (84 on Apple OSes / the BSDs) by the form without D (when the socket's directory can be opened and R made in it). The connects reach what the fallback routes reach (without the symlink to the file in D)
- The 253 bytes of the connects on Apple OSes hold while the kernel takes the widened sockaddr; after it refuses, 103
- A connect has no symlink route only when there is no safe place where the symlink can be made (a sandbox that allows neither `TMPDIR` nor `/tmp`, ...). With `/tmp` usable, the length is never the limit
- A path longer than PATH_MAX (1024 on macOS, 4096 on Linux) is `ENAMETOOLONG`: the directory is looked up by the full path. That limit is outside what sockat is for

### Paths containing a NUL

A path containing a NUL is `InvalidInput` at any length, and nothing is created: the kernel reads a path only up to its first NUL, so the socket could end up elsewhere. std refuses a path containing a NUL with `InvalidInput` too, so a short path gets the same kind of error as with std (the message is sockat's).

On Linux / Android a path starting with a NUL names a socket in the abstract namespace, not a file. It is outside sockat's business and goes to std as is at any length; std refuses any NUL in a `Path`, so the result is std's `InvalidInput`. Abstract-namespace sockets are handled with std's `SocketAddr::from_abstract_name` and `UnixListener::bind_addr` / `UnixStream::connect_addr`.

## Removing the socket file: bind_ex

`bind_ex` binds on the same routes as `bind` and returns the listener and `SocketFile`, which removes the socket file later.

```rust
let (listener, file) = sockat::bind_ex(path)?;
// use the listener (it may go to tokio etc.)
drop(listener);
file.discard()?;
```

- `bind_ex` takes the socket file's real path and `(st_dev, st_ino)` right after the bind, inside the route, before it returns the listener (on the central route while D and R are still there, by the spelling the bind took). When that cannot be done, the listener is closed and the error returned; the socket file stays
- `SocketFile::discard()` removes what is at that real path only when its `(st_dev, st_ino)` is the same. Anything else (another socket bound at the path since, a file, a symlink) is left alone, and with nothing there (a directory along the path removed or replaced by something that is not a directory included) nothing is done. Both are `Ok(())`. When what is there cannot be looked up for another reason (`EACCES`, say) or the removal fails, that is an error (not knowing whether the file was removed is never `Ok(())`)
- `discard()` does not close the listener. The listener is std's `UnixListener` and may go to tokio etc.; whoever holds it closes it, and `discard()` comes after that
- Dropping a `SocketFile` does nothing
- The same for a short path (handed to std unchanged)
- Limits: someone who can write to the socket's directory and replaces the file between the check and the removal, or between the bind and the look-up, is not caught (POSIX has no way to remove a file by its inode). An inode number freed by the file's removal and given to a new file on the same device makes that file look like the bind's

## The exception: connect_any

`connect_any` / `connect_any_nonblocking` are for the rare environment where `connect` is `ENAMETOOLONG` in the table above, when the connection is wanted at the cost of the fork's side effects. Normally they are not needed.

They are needed only when the central route cannot be used for a connect and no fallback route reaches the path either:

- the central route unusable for a connect: NetBSD / OpenBSD / illumos and the like (no thread-cwd means), Linux under a seccomp filter without `/proc`, an Apple OS without `pthread_fchdir_np`
- no fallback route: no safe place where a symlink can be made, and the path fits neither a relative spelling nor (on Apple OSes) the widened sockaddr

| Function | Returns |
|---|---|
| `sockat::connect_any(path)` | `io::Result<AnyConnection>` (blocking) |
| `sockat::connect_any_nonblocking(path)` | `io::Result<AnyConnection>` (nonblocking) |

They are off by default and available with the feature `any`: `sockat = { version = "0.4", features = ["any"] }`

`AnyConnection` is a struct with public fields `stream` (std's `UnixStream`) and `forked` (`bool`) ([DR-0026](./decisions/DR-0026-any-connection-and-feature-any.md)).

```rust
let sockat::AnyConnection { stream, forked } = sockat::connect_any(path)?;
```

The order tried is that of `connect`; when every other route is unusable, the connect is made through [fork](#fork) last. `forked` is `true` when the fork route was taken, `false` otherwise. When `true`, the side effects below apply, so it can be logged or warned about. On FreeBSD connectat reaches file names of up to 103 bytes, so the fork route is never reached there.

`connect_any_nonblocking` makes the socket nonblocking before the fork, so the child's connect does not wait either.

### Side effects of a connect through the fork route

**The peer pid the server sees is that of a process that has already exited (the forked child).** The socket itself belongs to the caller and works as usual.

- The uid / gid the server sees are the caller's
- On macOS (`LOCAL_PEERPID`) it changes to the caller's pid once the caller sends. On Linux (`SO_PEERCRED`) it stays the exited child's
- A server looking anything up from that pid (the executable, the parent through the ppid, ...) finds nothing: the process is gone
- The number of an exited pid may later be reused by another process. If it has been, the server gets an unrelated process's information
- So the pid is of no use to the caller or the server. `connect_any` does not return it, only whether the fork route was taken
- The peer credentials on NetBSD, OpenBSD and illumos have not been checked

## What differs from std for a long path

### The address the kernel reports is not the path

`local_addr()` (of the listener and of accepted streams) and `peer_addr()` (of a connected stream) report the spelling handed to the kernel when the listener was bound, whichever route the client took.

| Bind route | Reported |
|---|---|
| thread cwd, fork, bindat (in D) | `R/<file name>` |
| /proc version | `/proc/self/fd/<fd>/R/<file name>` |
| thread cwd, fork, bindat without D | `.sockat.XXXXXXXXXX/<file name>` |
| the /proc version without D | `/proc/self/fd/<fd>/.sockat.XXXXXXXXXX/<file name>` |

R (and `.sockat.XXXXXXXXXX`) is gone once the bind has returned, so removing, `stat`ing or `chmod`ing by the reported address hits nothing (however the fd number is reused, from whatever cwd it is resolved). It does not reach the socket either: keep the path you passed and use that (or `bind_ex` to remove the file). The same spelling shows in `Debug` output and in what frameworks log or hand out as the peer address.

On OpenBSD, std's `local_addr()` drops the last byte of the path even for a short path of up to 103 bytes (which sockat hands to std unchanged), as observed in CI. This is std's / the OS's behaviour, not sockat's: std's `UnixListener::bind` alone does the same.

### A bind reaches a shorter file name than a connect

A bind spelling carries R's 11 bytes, so a bind reaches file names of up to 96 bytes on Linux / illumos and 92 on Apple OSes / the BSDs. A connect normally reaches any file name length through the fallback routes.

### A path that does not end with a file name is InvalidInput

A path too long for std whose last component is not a file name (`dir/`, `dir/.`, `dir/..`, `..`) is `InvalidInput`, with nothing created: it cannot be split into a directory and a file name. A shorter one goes to std and gets the kernel's error (a bind of `dir/` is `EADDRINUSE` on macOS and Linux, for example).

### Side effects of the fork route that no peer sees

On the fork route (`bind` and the `connect_any` ones), the nature of fork brings the following. None of them is visible to the peer (server / client).

- The child holds copies of all file descriptors of the process until it exits. A socket or pipe another thread closes in that moment stays open until then (the peer sees the EOF slightly later; a closed listener's backlog can take one more connection)
- The calling process receives a `SIGCHLD`
- The handlers registered with `pthread_atfork` run
- Monitoring tools (process lists, audit logs, ...) see a short-lived child
- A `waitpid(-1)` on another thread may reap the child. The outcome is then read off the socket (a failure is `Other`)

### The directory is resolved slightly before the call

R points to the real path of the socket's directory as it was when R was made. The /proc, relative and connectat fallback routes and the fork of the `connect_any` ones look the directory up first (open it, or resolve it to a real path) and then connect by the file name. If a directory along the path is renamed or a symlink along it repointed in between, the socket is created in (or looked up in) the directory as it was looked up. The widened sockaddr and the symlink route let the kernel resolve the original path, and behave as std does in this respect.

### The relative route is exposed to another thread's chdir

sockat itself never changes the cwd. The relative spelling is checked to name the same directory when it is made, and the cwd is checked again after the connect. If another thread changed the cwd in between, the result is discarded (the connected socket closed) and an error (`Other`) returned. A change undone in between is not seen. A program that changes the cwd while connecting races every relative path in the process anyway; sockat does not make that safe, it only detects the visible case.

### Things can be left behind

If the process dies in the middle of a call, `sockat.XXXXXXXXXX` (D or a symlink) stays in `TMPDIR` or `/tmp`, and `.sockat.XXXXXXXXXX` (D or R) in the socket's directory. The names mark them as sockat's. On the targets that open D `O_RDONLY` (not Linux and Apple OSes), a umask that takes the owner's read bit makes a D just created impossible to open and check, and it is left as it is.

## Errors

| Situation | Error |
|---|---|
| The directory does not exist | `ENOENT` |
| Something other than a directory is where the directory should be | `ENOTDIR` |
| For a long path, the real path (realpath) of the socket's directory cannot be resolved | that error (for example `ENOENT` for `./<name>` in a removed cwd) |
| Nothing to connect to | `ENOENT` |
| A file already exists where the bind goes | `EADDRINUSE` |
| A socket file nobody listens on | `ECONNREFUSED` |
| Full backlog | Linux: `EAGAIN` for a nonblocking connect (a blocking one waits for room); macOS: `ECONNREFUSED` |
| No route fits ([Where reaching stops](#where-reaching-stops)); for `connect` / `connect_nonblocking` also when only the fork route would reach the path | `ENAMETOOLONG` |
| A path too long for std that does not end with a file name (`dir/`, `dir/.`, `..`, ...) | `InvalidInput` |
| A path containing a NUL (at any length; on Linux / Android a path starting with a NUL goes to std as is) | `InvalidInput` |
| The cwd changed during the relative route | `Other` |
| The forked child did not exit normally, or its exit status was reaped elsewhere (`SIGCHLD` set to `SIG_IGN`, a `waitpid(-1)` on another thread) and the socket was not bound / connected | `Other` |

Errors are `std::io::Error`s; for OS errors `raw_os_error()` gives the errno.

On macOS a connect answers `ECONNREFUSED` (blocking or not) when the peer's backlog is full too, so `ECONNREFUSED` alone does not tell a stale socket file nobody listens on. Code that removes a socket file on `ECONNREFUSED` can remove a live listener's socket file on macOS. This is the kernel's behaviour, the same with std's connect (on Linux, a full backlog makes a nonblocking connect `EAGAIN` and a blocking one wait for room).

## Async runtimes

The results are std types, so any runtime that adopts std sockets can take them. With tokio:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` does not wait for the peer, so it can be called on the runtime's worker threads directly. `bind` does not wait for a peer either (the bind and the listen return at once; the thread-cwd means waits for its thread and the fork route for its child, both of which bind / connect and end at once). `bind_ex`'s listener goes to the runtime the same way, and its `SocketFile` stays with the caller.

## Not checked

- Android has not even been compiled. It uses the same routes as Linux. Android apps run under a seccomp filter, so the thread-cwd means is not expected to be used there (not checked)
- NetBSD, OpenBSD, illumos and FreeBSD run the tests in CI VMs. The peer credentials (pid) of a connect through the fork route and the server's peer a client sees after a fork-route bind have not been checked there
- The error of a nonblocking connect on a full backlog, and that a connect completes within the call, have been checked on Linux and macOS only
- Apple OSes other than macOS (iOS, ...) use the same routes as macOS, but only iOS's clippy has been run. Whether they have `pthread_fchdir_np` has not been checked either (without it, the thread-cwd means counts as unusable and the next is tried)
