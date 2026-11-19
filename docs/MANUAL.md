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

There are also `bind_any` / `connect_any` / `connect_any_nonblocking`, which reach the path at the cost of something, behind the feature `any` ([below](#the-exception-the-any-functions)).

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

Neither takes [fork-direct](#fork-direct). Where only fork-direct would reach the path, they return `ENAMETOOLONG`, as an ordinary connect to a path too long for `sun_path` does. In an ordinary environment (a writable `/tmp`), they reach a path whatever the directory depth and the file name length ([Where reaching stops](#where-reaching-stops)).

## How a long path is reached

A path of up to the length std accepts goes to std unchanged ([standard](#standard)). With today's std that is, on every OS, the length that fits the plain sockaddr (`sun_path` − 1, "the sockaddr length" below).

| OS | Handed to std up to |
|---|---|
| Linux / Android / illumos | 107 bytes |
| macOS / iOS and other Apple OSes / FreeBSD / NetBSD / OpenBSD | 103 bytes |

A longer path is bound through D, a directory of sockat's own, and when that cannot be used, by [the form without D](#the-form-without-d). A connect tries [the connect routes](#the-connect-routes) that need neither writing (making D, R or a symlink) nor a thread first, and the routes through D and the symlink after them. The first route that can be used makes the call. None of the routes changes the working directory of the calling process or of the calling thread, so they can be used from multi-threaded programs. The routes' names are those of the [sockat specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md) ("route names").

### Order per OS

| OS | `bind` | `connect` / `connect_nonblocking` |
|---|---|---|
| Linux / Android | [thread-cwd](#thread-cwd) → [proc](#proc) → [fork](#fork) | [proc-direct](#proc-direct) → thread-cwd → proc → [proc-held](#proc-held) → [symlink](#symlink) |
| Linux / Android (no `/proc`) | thread-cwd → fork | [relative](#relative) → thread-cwd → symlink |
| macOS / iOS and other Apple OSes | thread-cwd → fork | relative → [widened](#widened) → thread-cwd → symlink |
| FreeBSD | [at](#at) | [at-direct](#at-direct) → at |
| NetBSD / OpenBSD / illumos / other Unix | fork | relative → symlink |
| Windows | not supported (compile error outside Unix) | |

- [standard](#standard) is tried before these and cannot be used for the length
- A connect tries the routes that need neither writing nor a thread (proc-direct, relative, widened, at-direct) first, and after them the routes that make D (thread-cwd, proc, proc-held, at), the one that starts a thread (thread-cwd) and the one that makes a symlink. When an earlier route reaches the socket, neither D nor a symlink is made
- A bind for which D can be made nowhere tries the same order in [the form without D](#the-form-without-d) (the `-sockdir` routes)
- `bind_any` tries the order of `bind`, then binds through [symlink](#symlink) last. `connect_any` and `connect_any_nonblocking` try the order of `connect`, then connect through [fork-direct](#fork-direct) last ([The exception: the any functions](#the-exception-the-any-functions))
- The socket's directory is looked up (`stat`) first. If it is missing, the error is `ENOENT` (whatever the file name length)
- When a route actually binds / connects and fails (`ENOENT`, `EADDRINUSE`, `ECONNREFUSED`, ...), that error is returned and nothing else is tried. The next one is tried only when a route cannot be used at all

### standard

The path goes to std as it is. Used only for a path of up to the length std accepts.

### The routes through D

sockat makes a directory D of its own, holds it by an fd, makes in it a symlink R with a random name to the real path of the socket's directory, and binds / connects with `R/<file name>` relative to D's fd. R and D are removed afterwards. [thread-cwd](#thread-cwd), [at](#at), [proc](#proc) and [fork](#fork) take this form. The connect route [proc-held](#proc-held) uses the same D, with `s`, a symlink to the socket file, in place of R.

1. In a place for D, a directory `sockat.XXXXXXXXXX` (10 random letters and digits) of mode 0700 is made
2. It is opened without following its name (`O_NOFOLLOW`) and checked: a directory, owned by the caller, no permission for group and others. Anything else (someone replaced it between the two steps) is neither used nor removed. Once checked, its mode is set to 0700 (so a umask that took the owner's bits does not get in the way)
3. R is made in D
4. `R/<file name>` is bound / connected by the first route that can be used
5. Success or failure, R is removed, and D is removed only while its name still names the directory held

- D is made at most once in a call and shared by the routes that use it (proc after thread-cwd was refused, proc-held after proc's spelling did not fit, use the same D). When it can be made nowhere, every later route that uses D is passed over, and D is not tried again
- **The places for D** are `TMPDIR` (`/tmp` when unset; a relative one is fine), `/tmp`, then the socket's directory (named `.sockat.XXXXXXXXXX` there), the first where D can be made. A place needs to be neither safe (sticky, ...) nor short: D is held by its fd and no spelling names the place
- When a place cannot be opened, D cannot be created, the check fails, D cannot be set to 0700 or R (`s` when proc-held asks for D first) cannot be created, the next place is tried
- When D just made cannot be opened (a umask took the owner's bits: search on Apple OSes, read on targets other than Linux and Apple OSes), it is looked up by its name and removed if it passes the checks of 2, and the next place is tried. Should it be replaced between that look-up and the removal, only an empty directory can be removed
- The spelling is `R/<file name>`, so the file name has R's 11 bytes less than the sockaddr length (up to 96 bytes on Linux / illumos, 92 on Apple OSes / the BSDs; about 79 for proc)
- D is opened `O_PATH` on Linux, `O_SEARCH` on Apple OSes, `O_RDONLY` elsewhere. It is set to 0700 by `chmod` of `/proc/self/fd/<fd>` on Linux with `/proc`, by `fchmod` of D opened again by name `O_RDONLY` and checked to be the same directory on Linux without `/proc`, by `fchmod` elsewhere
- The routes through D do not open the socket's directory (only D made in it does); R reaches it with search permission alone. A directory without read permission is reached with the same permission as a plain bind / connect

#### thread-cwd

Linux / Android, Apple OSes. A thread started for this call alone moves its own working directory, and no other thread's, into D (`unshare(CLONE_FS)` then `fchdir` on Linux, `pthread_fchdir_np` on Apple OSes) and binds / connects `R/<file name>`. The thread ends with the call.

- **Every function tries it.** The calling process makes the bind / connect, so the peer pid the server sees is the caller's, and no fd is duplicated
- It cannot be used in these cases, and the next route is tried:
  - on Linux a seccomp filter is in effect (docker's default installs one). A filter may refuse `unshare` by killing the process (SIGSYS) rather than with an error (systemd's `SystemCallFilter=` does by default), so `unshare` is not called at all then
  - on Linux `unshare` is refused
  - on Apple `pthread_fchdir_np` is missing or fails (it is not in a public header, so it is looked up at run time)
  - no thread can be started

#### at

FreeBSD. `bindat(2)` / `connectat(2)` take D's fd and `R/<file name>`. No thread.

#### proc

Linux / Android with `/proc`. `/proc/self/fd/<D's fd>/R/<file name>` is handed to std, with no thread. The whole spelling must be at most 107 bytes, so the file name about 79 bytes at most (it depends on the number of digits of the fd). It is estimated before D is made and measured again with D's fd once D is open. Whether `/proc` can be used is checked with an fd of `/` (the socket's directory is not opened).

#### fork

A bind only. [The child](#the-forked-child) `fchdir`s into D and binds `R/<file name>`. Tried when thread-cwd and proc cannot be used (proc's spelling not fitting included).

### The form without D

Used by a bind when D can be made nowhere. R (`.sockat.XXXXXXXXXX`) is made in the socket's directory, and the bind is made with `.sockat.XXXXXXXXXX/<file name>` relative to the socket's directory's fd instead of D's, by the same routes in the same order (thread-cwd-sockdir (at-sockdir on FreeBSD) → proc-sockdir `/proc/self/fd/<fd>/.sockat.XXXXXXXXXX/<file name>` → fork-sockdir). R is removed afterwards.

- The spelling is 19 bytes plus the file name (file names of up to 88 bytes on Linux / illumos, 84 on Apple OSes / the BSDs; about 72 for proc-sockdir)
- When the socket's directory cannot be opened or R cannot be made either, it is `ENAMETOOLONG` (`bind_any` then tries [symlink](#symlink))
- The connects do not use this form; they go on to the next route of the [order per OS](#order-per-os)

### The connect routes

Routes of the connects only. A connected socket is not bound and its spelling is kept nowhere, so these spellings carry no random part. proc-direct, relative, widened and at-direct need neither writing nor a thread, so they come first in the connect order.

#### proc-direct

Linux / Android with `/proc`. First in the connect order. The socket's directory is opened `O_PATH` and `/proc/self/fd/<fd>/<file name>` is handed to std. The whole spelling must be at most 107 bytes, so the file name about 90 bytes at most (it depends on the number of digits of the fd). When the socket's directory cannot be opened or `/proc/self/fd/<fd>` cannot be looked up, the next route is tried.

#### proc-held

Linux / Android with `/proc`. After [proc](#proc). In D (the one an earlier route made, or one made and checked now), a symlink `s` to the socket file is made, and the connect goes through `/proc/self/fd/<D's fd>/s`. The spelling is about 20 bytes whatever the file name length and the directory depth. The places for D are those of the routes through D (none needs to be safe). The socket's directory is not opened.

#### relative

First in the connect order of Linux / Android without `/proc`, Apple OSes, NetBSD / OpenBSD / illumos / other Unix. When the path spelled relative to the cwd fits the sockaddr length, that goes to std. Only for an absolute path. Only the path string is rewritten; the cwd is never changed (no `chdir`).

- When the path is below the cwd (`getcwd`, or a `$PWD` that names the cwd), that prefix is cut off. When that does not fit (the path not being below the cwd included), a spelling climbing with `..` between the two resolved to real paths is made too. The shorter is used
- When the relative spelling's directory does not match the original by `(st_dev, st_ino)`, or the cwd has already changed, the route counts as unusable and the next is tried
- The cwd is checked again after the connect ([below](#relative-is-exposed-to-another-threads-chdir))

#### widened

Apple OSes. After relative. XNU takes an address whose path is longer than `sun_path`'s 104 bytes, up to 253 bytes. The path itself is handed over when it is at most 253 bytes, failing that the relative spelling when that is. When the kernel refuses it (`EINVAL` / `ENAMETOOLONG`) the next route is tried, and [fork-direct](#fork-direct) does not use the widened sockaddr either.

A `bind` never uses it: the address of a socket bound so does not fit `sockaddr_un`, `getsockname` returns a truncated path, and its readers break (CPython's asyncio clean-up, Go's `net.FileConn`, ...).

#### symlink

A symlink `sockat.XXXXXXXXXX` is made in a place, the bind / connect goes through it, and it is removed at once. A connect tries it last in the connect order (not on FreeBSD); a bind only in `bind_any`, last.

| | connect | `bind_any` |
|---|---|---|
| What the symlink points to | the socket file | the real path of the socket's directory |
| The spelling | `<place>/sockat.XXXXXXXXXX` (22 bytes through `/tmp`, whatever the directory depth and the file name length) | `<place>/sockat.XXXXXXXXXX/<file name>` |
| Candidate places | `TMPDIR` (when it is a non-empty absolute path), then `/tmp` | `XDG_RUNTIME_DIR` and `TMPDIR` (each when a non-empty absolute path), then `/tmp`; a spelling once |
| How they are tried | the safe places in the candidates' order | the places only the caller can write to in the candidates' order, then the shared places in the candidates' order |

- **A place only the caller can write to**: owned by the caller, without write permission for group and others (macOS's `$TMPDIR`, Linux's `$XDG_RUNTIME_DIR`)
- **A shared place**: sticky, owned by root or the caller (as `/tmp` is). Anyone can create entries there, so the spelling `bind_any` bound by can be made again by another user once the symlink is gone
- **A safe place**: either. Nobody else can replace the symlink in it (a non-directory is neither)
- A place is judged through symlinks (macOS's `/tmp` is a symlink to `/private/tmp`)
- A candidate where the spelling would not fit the sockaddr length, that does not qualify, or where the symlink cannot be made (the place missing or not writable, a sandbox, no free name found, no random bytes) gets nothing made in it, and the next is tried. A `TMPDIR` too deep for the spelling gives way to `/tmp`

#### at-direct

FreeBSD. First in the connect order. The socket's directory is opened and `connectat(2)` takes the file name, of up to 103 bytes. When the socket's directory cannot be opened, [at](#at) is tried.

#### fork-direct

The `connect_any` ones only. [The child](#the-forked-child) `fchdir`s into the socket's directory and connects by the file name. On Apple OSes, unless the kernel has refused the widened sockaddr, a file name longer than 103 bytes goes in the widened sockaddr.

- `connect` / `connect_nonblocking` return `ENAMETOOLONG` once they get here
- The file name fits the sockaddr length (on Apple, 253 bytes while the kernel takes the widened sockaddr)

### The forked child

Used by [fork](#fork), fork-sockdir and [fork-direct](#fork-direct). A child that `fchdir`s into a directory fd is forked, binds / connects by the spelling, and exits at once. After the fork, parent and child share the socket, so the child's bind / connect takes effect on the caller's socket. The listen after a bind is always made by the caller.

| Route | The child `fchdir`s into | Spelling |
|---|---|---|
| fork | D | `R/<file name>` |
| fork-sockdir | the socket's directory | `.sockat.XXXXXXXXXX/<file name>` |
| fork-direct | the socket's directory | the file name |

- macOS opens the socket's directory `O_SEARCH` and Linux `O_PATH`, which need no more permission than a plain bind / connect; other Unix targets open it `O_RDONLY`, which needs read permission as well (when it cannot be opened, the route cannot be used)
- For a bind the child only binds and the caller listens, so the fork route has no side effect a peer sees. The server's peer a client sees (macOS's `LOCAL_PEERPID`, Linux's `SO_PEERCRED`) is the caller from before anything is sent ([DR-0025](./decisions/DR-0025-fork-bind-listen-by-parent.md)). There is no bind that never forks
- The child makes only async-signal-safe calls (`fchdir`, `bind` / `connect`, `_exit`), so it can be used from multi-threaded processes
- Side effects: [the peer pid of a connect](#side-effects-of-a-connect-through-fork-direct) (seen by the peer, fork-direct only), [side effects no peer sees](#side-effects-of-the-fork-routes-that-no-peer-sees) (every fork route)

### Where reaching stops

When a path too long for std is `ENAMETOOLONG` (assuming the directory exists and D can be made; a missing directory is `ENOENT`).

| OS | `bind` | `connect` / `connect_nonblocking` | `connect_any` ones |
|---|---|---|---|
| Linux / Android (`/proc`) | file name over 96 bytes | always reached (proc-held) | always reached |
| Linux / Android (no `/proc`) | file name over 96 bytes | relative over 107 bytes, thread-cwd unusable (file name over 96 bytes, or the thread cwd unusable), and no symlink | file name over 107 bytes, and no symlink |
| FreeBSD | file name over 92 bytes | file name over 103 bytes | file name over 103 bytes |
| macOS / iOS and other Apple OSes | file name over 92 bytes | the path and the relative path over 253 bytes, thread-cwd unusable (file name over 92 bytes, or the thread cwd unusable), and no symlink | file name over 253 bytes, and no symlink |
| NetBSD / OpenBSD / other BSDs | file name over 92 bytes | relative over 103 bytes, and no symlink | file name over 103 bytes, and no symlink |
| illumos | file name over 96 bytes | relative over 107 bytes, and no symlink | file name over 107 bytes, and no symlink |

- On Linux without the thread cwd (a seccomp filter, ...), a bind takes proc for a file name of up to about 79 bytes and fork up to 96
- When D can be made nowhere, a bind reaches a file name of up to 88 bytes (84 on Apple OSes / the BSDs) by the form without D (when the socket's directory can be opened and R made in it). The connects reach what the routes of the connect order that do not use D reach (proc-direct, relative, widened, symlink, at-direct; without proc-held either)
- `bind_any`'s symlink spelling is longer than those through D, so it does not reach a longer file name than `bind`. It reaches the path when neither D nor the form without D can be used
- `bind_any` is `ENAMETOOLONG` as well when no place makes its symlink spelling fit 103 / 107 bytes
- The 253 bytes of the connects on Apple OSes hold while the kernel takes the widened sockaddr; after it refuses, 103
- A connect has no symlink only when there is no safe place where the symlink can be made (a sandbox that allows neither `TMPDIR` nor `/tmp`, ...). With `/tmp` usable, the length is never the limit
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

- `bind_ex` takes the socket file's real path and `(st_dev, st_ino)` right after the bind, inside the route, before it returns the listener (on the routes through D and the form without D while R is still there, by the spelling the bind took). When that cannot be done, the listener is closed and the error returned; the socket file stays
- `SocketFile::discard()` removes what is at that real path only when its `(st_dev, st_ino)` is the same. Anything else (another socket bound at the path since, a file, a symlink) is left alone, and with nothing there (a directory along the path removed or replaced by something that is not a directory included) nothing is done. Both are `Ok(())`. When what is there cannot be looked up for another reason (`EACCES`, say) or the removal fails, that is an error (not knowing whether the file was removed is never `Ok(())`)
- `discard()` does not close the listener. The listener is std's `UnixListener` and may go to tokio etc.; whoever holds it closes it, and `discard()` comes after that
- Dropping a `SocketFile` does nothing
- The same for a short path (handed to std unchanged)
- Limits: someone who can write to the socket's directory and replaces the file between the check and the removal, or between the bind and the look-up, is not caught (POSIX has no way to remove a file by its inode). An inode number freed by the file's removal and given to a new file on the same device makes that file look like the bind's

## The exception: the any functions

`bind_any` / `connect_any` / `connect_any_nonblocking` reach the path at the cost of something when every default route is unusable. They are only for the rare environment where `bind` / `connect` are `ENAMETOOLONG` and the path must be reached anyway. Normally they are not needed. They are off by default and available with the feature `any`: `sockat = { version = "0.4", features = ["any"] }`

| Function | Order tried | Returns |
|---|---|---|
| `sockat::bind_any(path)` | that of `bind`, then [symlink](#symlink) last | `Result<AnyListener, AnyError>` |
| `sockat::connect_any(path)` | that of `connect`, then [fork-direct](#fork-direct) last | `Result<AnyConnection, AnyError>` (blocking) |
| `sockat::connect_any_nonblocking(path)` | the same | `Result<AnyConnection, AnyError>` (nonblocking) |

`bind_any`'s symlink gives up `respelled`, and in a shared place (`/tmp`, ...) `recreatable-spelling` too. fork-direct of the `connect_any` ones gives up `forked` (the peer pid the server sees is that of a child that has exited; [below](#side-effects-of-a-connect-through-fork-direct)). What each means and how to avoid it is in [What is lost](#what-is-lost).

The results are structs with public fields, `#[non_exhaustive]` (fields may be added, so a pattern needs `..`):

| Field | Content |
|---|---|
| `listener` / `stream` | std's `UnixListener` / `UnixStream` |
| `losses` | what was given up (`Vec<Loss>`; empty when nothing was). Branch on this. `Loss` is a `#[non_exhaustive]` enum, its values in the table of [What is lost](#what-is-lost) |
| `route` | the name of the route taken (`&'static str`, such as `"thread-cwd"`, `"symlink"`, `"fork-direct"`) |
| `skipped` | the routes tried and passed over, in the order tried (`Vec<Skipped>`). A `Skipped` has `name` (the route's name) and `reason` (why it could not be used) |

- `route` and `skipped` are for diagnostics and promise nothing about their content (routes may be added or renamed)
- Every route tried before `route` was passed over. For a long path the first is `standard`
- `skipped` holds only routes of the order the call tries (the row of [Order per OS](#order-per-os) for that OS and state). On a Linux without `/proc`, for example, proc, proc-sockdir, proc-direct and proc-held are not in it

A failure returns an `AnyError`, with `error` (the `io::Error`), `route` and `skipped`.

- `route` is the name of the route that actually bound / connected and failed; `"standard"` when std failed on a short path. `None` when no route could be used (`ENAMETOOLONG`) and when the path was refused before any route (a NUL byte, no file name, the socket's directory missing, not a directory, or its real path not resolvable)
- When the path was refused before any route, `skipped` is empty too: the length is judged only after the socket's directory is looked up, so not even `standard` has been tried
- `From<AnyError> for io::Error` gives the inner `io::Error` as it is, so `?` works in a function returning `io::Result` and `raw_os_error()` still gives the errno. Its `Display` is the inner `io::Error`'s

```rust
let sockat::AnyConnection { stream, losses, route, .. } = sockat::connect_any(path)?;
if losses.contains(&sockat::Loss::Forked) {
    eprintln!("connected by {route} (forked)");
}
```

`bind_any`'s symlink is needed only when D can be made nowhere and the socket's directory cannot be opened either (no R). Its spelling is longer than that of the routes through D, so it does not help when the file name is too long for `bind`.

fork-direct of the `connect_any` ones is needed only when no route of the connect order reaches the path:

- the routes through D unusable for a connect: NetBSD / OpenBSD / illumos and the like (no thread cwd), Linux under a seccomp filter without `/proc`, an Apple OS without `pthread_fchdir_np`
- no route without D either: no safe place where a symlink can be made, and the path fits neither a relative spelling nor (on Apple OSes) the widened sockaddr

On FreeBSD at-direct reaches file names of up to 103 bytes, so fork-direct is never reached there. `connect_any_nonblocking` makes the socket nonblocking before the fork, so the child's connect does not wait either.

### Side effects of a connect through fork-direct

**The peer pid the server sees is that of a process that has already exited (the forked child).** The socket itself belongs to the caller and works as usual.

- The uid / gid the server sees are the caller's
- On macOS (`LOCAL_PEERPID`) it changes to the caller's pid once the caller sends. On Linux (`SO_PEERCRED`) it stays the exited child's
- A server looking anything up from that pid (the executable, the parent through the ppid, ...) finds nothing: the process is gone
- The number of an exited pid may later be reused by another process. If it has been, the server gets an unrelated process's information
- So the pid is of no use to the caller or the server. `connect_any` does not return it
- The peer credentials on NetBSD, OpenBSD and illumos have not been checked

## What differs from std for a long path

### The address the kernel reports is not the path

`local_addr()` (of the listener and of accepted streams) and `peer_addr()` (of a connected stream) report the spelling handed to the kernel when the listener was bound, whichever route the client took.

| Bind route | Reported |
|---|---|
| thread-cwd, fork, at | `R/<file name>` |
| proc | `/proc/self/fd/<fd>/R/<file name>` |
| thread-cwd-sockdir, fork-sockdir, at-sockdir | `.sockat.XXXXXXXXXX/<file name>` |
| proc-sockdir | `/proc/self/fd/<fd>/.sockat.XXXXXXXXXX/<file name>` |
| symlink (`bind_any`) | `<place>/sockat.XXXXXXXXXX/<file name>` |

R (and `.sockat.XXXXXXXXXX`, the place's symlink) is gone once the bind has returned, so removing, `stat`ing or `chmod`ing by the reported address normally hits nothing (however the fd number is reused). Others can see the spelling, though, and once they make it again it can hit something ([respelled](#respelled); for the symlink in a shared place [recreatable-spelling](#recreatable-spelling)). It does not reach the socket either: keep the path you passed and use that (or `bind_ex` to remove the file; [Patterns not to use](#patterns-not-to-use)). The same spelling shows in `Debug` output and in what frameworks log or hand out as the peer address.

On OpenBSD, std's `local_addr()` drops the last byte of the path even for a short path of up to 103 bytes (which sockat hands to std unchanged), as observed in CI. This is std's / the OS's behaviour, not sockat's: std's `UnixListener::bind` alone does the same.

### A bind reaches a shorter file name than a connect

A bind spelling carries R's 11 bytes, so a bind reaches file names of up to 96 bytes on Linux / illumos and 92 on Apple OSes / the BSDs. A connect normally reaches any file name length through the routes without R (proc-held, symlink, ...).

### A path that does not end with a file name is InvalidInput

A path too long for std whose last component is not a file name (`dir/`, `dir/.`, `dir/..`, `..`) is `InvalidInput`, with nothing created: it cannot be split into a directory and a file name. A shorter one goes to std and gets the kernel's error (a bind of `dir/` is `EADDRINUSE` on macOS and Linux, for example).

### Side effects of the fork routes that no peer sees

On the fork routes (fork, fork-sockdir, fork-direct), the nature of fork brings the following. None of them is visible to the peer (server / client).

- The child holds copies of all file descriptors of the process until it exits. A socket or pipe another thread closes in that moment stays open until then (the peer sees the EOF slightly later; a closed listener's backlog can take one more connection)
- The calling process receives a `SIGCHLD`
- The handlers registered with `pthread_atfork` run
- Monitoring tools (process lists, audit logs, ...) see a short-lived child
- A `waitpid(-1)` on another thread may reap the child. The outcome is then read off the socket (a failure is `Other`)

### The directory is resolved slightly before the call

R and `bind_any`'s symlink point to the real path of the socket's directory as it was when they were made. proc-direct, relative, at-direct and fork-direct look the directory up first (open it, or resolve it to a real path) and then connect by the file name. If a directory along the path is renamed or a symlink along it repointed in between, the socket is created in (or looked up in) the directory as it was looked up. widened and a connect's symlink let the kernel resolve the original path, and behave as std does in this respect.

When D can be made nowhere and a bind goes through R (`.sockat.XXXXXXXXXX`) made in the socket's directory itself, and that directory is not sticky and others can write to it, someone else can replace R between its creation and the bind, and the socket can then be bound in another directory (a name already there is `EADDRINUSE`; nothing is overwritten).

### relative is exposed to another thread's chdir

sockat itself never changes the cwd. The relative spelling is checked to name the same directory when it is made, and the cwd is checked again after the connect. If another thread changed the cwd in between, the result is discarded (the connected socket closed) and an error (`Other`) returned. A change undone in between is not seen. A program that changes the cwd while connecting races every relative path in the process anyway; sockat does not make that safe, it only detects the visible case.

### Things can be left behind

If the process dies in the middle of a call, `sockat.XXXXXXXXXX` (D or a symlink) stays in `XDG_RUNTIME_DIR`, `TMPDIR` or `/tmp`, and `.sockat.XXXXXXXXXX` (D or R) in the socket's directory. The names mark them as sockat's.

## What is lost

What taking a route loses compared to std ([the sockat specification's "What is lost"](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-is-lost)). The any functions return these names as `losses`. `bind` / `bind_ex` lose the same on the routes they take (they just do not return it).

| Name | `Loss` | What is lost | When |
|---|---|---|---|
| [respelled](#respelled) | `Respelled` | the address the kernel reports is not the original path | a bind took a route other than standard |
| [cwd-relative](#cwd-relative) | `CwdRelative` | the spelling the kernel keeps is relative to the cwd | a bind took standard with a relative path (not an abstract name) |
| [forked](#forked) | `Forked` | the side effects of forking | fork, fork-sockdir or fork-direct was taken |
| [recreatable-spelling](#recreatable-spelling) | `RecreatableSpelling` | the spelling can be made again by other users | `bind_any` took the symlink in a shared place |

- `Loss` is a `#[non_exhaustive]` enum; `as_str()` and `Display` give the specification's names
- The names are a promise: never changed, only added to. Read a name you do not know only as "something was lost"
- The names have no order or inclusion among them (`forked` and `recreatable-spelling` do not include each other)
- A connect's spelling is left nowhere on the peer, so the only name a connect gets is `forked`
- The symlink in a place only the owner can write to gets `respelled` only

The code in each section is also in the crate documentation, where it runs as a doctest.

### respelled

- What happens: the bound side's `local_addr()` (and the connecting side's `peer_addr()`) reports a spelling that is not the original path ([The address the kernel reports is not the path](#the-address-the-kernel-reports-is-not-the-path)). Closing the listener does not remove the socket file (as with std)
- Watch out for: do not remove or `stat` the file by the spelling the kernel reports. The random name R in it has been removed, so normally it hits nothing, but the spelling may be visible to other users (Linux's `/proc/net/unix`). If the process's cwd is a place others can write to (`/tmp`, ...), after another user sees and makes it again, removing by that spelling may remove a same-named file in a directory that user chose
- How to avoid: keep the original path and use it when removing. Use `bind_ex`'s `SocketFile::discard()`

```rust
use std::io;
use std::path::Path;

fn serve(path: &Path) -> io::Result<()> {
    let (listener, file) = sockat::bind_ex(path)?;
    // accept on listener (it may go to tokio etc.)
    drop(listener);
    file.discard() // removes the file only while it is still the one bound
}

fn serve_and_remove(path: &Path) -> io::Result<()> {
    let listener = sockat::bind(path)?;
    // accept on listener
    drop(listener);
    std::fs::remove_file(path) // remove by the original path
}
```

### cwd-relative

- What happens: the given relative path goes to std unchanged (when std takes its length), and the kernel keeps a relative spelling
- Watch out for: removing or `stat`ing by that spelling (or by the given relative path) after the cwd changes hits another place. A hazard std already has rather than one a route introduced, but it is reported
- How to avoid: make the path absolute before the bind. Remove by the absolute path or `discard()` as well

```rust
use std::io;
use std::path::Path;

fn serve(path: &Path) -> io::Result<()> {
    let path = std::path::absolute(path)?; // make it absolute before the bind
    let (listener, file) = sockat::bind_ex(&path)?;
    // accept on listener
    drop(listener);
    file.discard()
}
```

### forked

- What happens: a child is forked for a moment ([Side effects of the fork routes that no peer sees](#side-effects-of-the-fork-routes-that-no-peer-sees)). The child holds copies of every fd, the parent gets `SIGCHLD`, and `pthread_atfork` handlers run. After the `connect_any` ones the peer pid the server sees is that of the child ([Side effects of a connect through fork-direct](#side-effects-of-a-connect-through-fork-direct))
- Watch out for: a program that uses a `SIGCHLD` handler or `waitpid(-1)` may reap the child or mistake the result. A program that closes an fd to tell the peer EOF gets it across only after the child exits. The server on the other end of a `connect_any` one cannot count on authorizing by the peer's pid
- How to avoid: do not use `waitpid(-1)` in other threads (wait by pid). Where `forked` cannot be accepted, do not use the any functions

```rust
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitStatus};

fn run_tool() -> io::Result<ExitStatus> {
    let mut child = Command::new("true").spawn()?;
    child.wait() // waits for this child's pid (no waitpid(-1))
}

fn dial(path: &Path) -> io::Result<UnixStream> {
    sockat::connect(path) // a connect that does not fork, not connect_any
}
```

### recreatable-spelling

- What happens: the spelling the kernel reports goes through a shared place (`/tmp`, ...) (`<place>/sockat.XXXXXXXXXX/<file name>`)
- Watch out for: the name is random, so it cannot be guessed in advance, but after the bind other users may see it (Linux's `/proc/net/unix`). If another user makes it again and it is then removed by that spelling, a same-named file in a directory the other user chose may be removed. The name's length is not the defense (the name is visible after the bind)
- How to avoid: do not remove by the spelling the kernel reports; remove by the original path or `discard()`. If you provide a place only the owner can write to (`$XDG_RUNTIME_DIR`, or a `$TMPDIR` only the owner can write to), it is used first and this name is not given

```rust
use std::io;
use std::path::Path;

fn serve(path: &Path) -> io::Result<()> {
    let any = sockat::bind_any(path)?; // feature `any`
    // accept on any.listener
    drop(any.listener);
    std::fs::remove_file(path) // by the original path, not local_addr()'s spelling
}
```

## Patterns not to use

Ways of using sockat that remove an unrelated file or fail to remove the file ([the sockat specification's "Patterns not to use"](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#patterns-not-to-use)).

| Pattern | In Rust | Why not | Instead |
|---|---|---|---|
| removing or `stat`ing the socket file by the spelling the kernel reports | `std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())` | for a long path it is not the original path ([respelled](#respelled)). Depending on the cwd or a shared place, it may hit another file someone made again ([cwd-relative](#cwd-relative), [recreatable-spelling](#recreatable-spelling)) | keep the original path and `std::fs::remove_file(path)`<br>`bind_ex`'s `SocketFile::discard()` |
| binding with a relative path and removing by that relative path after the cwd changes | `sockat::bind("app.sock")` followed by<br>`std::env::set_current_dir(..)` and<br>`std::fs::remove_file("app.sock")` | hits a file in another place ([cwd-relative](#cwd-relative)) | make the path absolute with `std::path::absolute` before the bind<br>`discard()` |
| relying on closing the listener to remove the socket file | `drop(listener)` alone | the socket file is not removed (as with std) | `discard()`<br>remove by the original path |
| relying on `discard()` to close the listener too | `file.discard()` alone | `discard()` only removes the file ([DR-0028](./decisions/DR-0028-bind-ex-and-socket-file.md), Japanese) | close the listener yourself (`drop(listener)`) |
| branching on the strings of the any functions' `route` / `skipped` / `reason` | `if any.route == "symlink"` | they are for diagnostics and promise nothing (routes are added and changed) | branch on `losses` (`any.losses.contains(&sockat::Loss::RecreatableSpelling)`) |
| in a program that uses the any functions' fork, another thread reaping children with `waitpid(-1)` | `libc::waitpid(-1, ..)` | it may reap sockat's child and mistake the result ([forked](#forked)) | wait by pid (`std::process::Child::wait`)<br>where `forked` cannot be accepted, do not use the any functions |

The code below lines up the patterns of the table (none of it is to be done).

```rust
use std::io;
use std::path::Path;

fn wrong(path: &Path) -> io::Result<()> {
    let listener = sockat::bind(path)?;
    // removing by the spelling the kernel reports: for a long path it is not the path, and may hit another file
    std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())?;
    // closing the listener does not remove the socket file
    drop(listener);

    let (listener, file) = sockat::bind_ex(path)?;
    // discard() does not close the listener
    file.discard()?;
    let _still_open = listener;
    Ok(())
}

fn wrong_relative() -> io::Result<()> {
    let _listener = sockat::bind("app.sock")?;
    std::env::set_current_dir("/")?;
    // removing by the relative path after the cwd changed: hits a file in another place
    std::fs::remove_file("app.sock")
}

fn wrong_any(path: &Path) -> io::Result<()> {
    let any = sockat::bind_any(path)?; // feature `any`
    // branching on the route's string: it promises nothing (branch on losses)
    if any.route == "symlink" {
        eprintln!("bound through a symlink");
    }
    Ok(())
}
```

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
| No route fits ([Where reaching stops](#where-reaching-stops)); for `connect` / `connect_nonblocking` also when only fork-direct would reach the path | `ENAMETOOLONG` |
| A path too long for std that does not end with a file name (`dir/`, `dir/.`, `..`, ...) | `InvalidInput` |
| A path containing a NUL (at any length; on Linux / Android a path starting with a NUL goes to std as is) | `InvalidInput` |
| The cwd changed during relative | `Other` |
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

`connect_nonblocking` does not wait for the peer, so it can be called on the runtime's worker threads directly. `bind` does not wait for a peer either (the bind and the listen return at once; thread-cwd waits for its thread and the fork routes for their child, both of which bind / connect and end at once). `bind_ex`'s listener goes to the runtime the same way, and its `SocketFile` stays with the caller.

## Not checked

- Android has not even been compiled. It uses the same routes as Linux. Android apps run under a seccomp filter, so thread-cwd is not expected to be used there (not checked)
- NetBSD, OpenBSD, illumos and FreeBSD run the tests in CI VMs. The peer credentials (pid) of a connect through fork-direct and the server's peer a client sees after a fork-route bind have not been checked there
- The error of a nonblocking connect on a full backlog, and that a connect completes within the call, have been checked on Linux and macOS only
- Apple OSes other than macOS (iOS, ...) use the same routes as macOS, but only iOS's clippy has been run. Whether they have `pthread_fchdir_np` has not been checked either (without it, thread-cwd counts as unusable and the next is tried)
