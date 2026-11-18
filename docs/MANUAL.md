# sockat manual

> English | [日本語](./MANUAL-ja.md)

A reference for using sockat: how it reaches a long path, and how it differs from std for long paths. The design decisions and their reasons are in [DR-0001](./decisions/DR-0001-long-socket-path-routes.md) (in Japanese).

## Usage

```toml
[dependencies]
sockat = "0.2"
```

Use `sockat::bind` / `sockat::connect` where you would use std's `bind` / `connect`. In async code, use `sockat::connect_nonblocking`.

| Function | std counterpart | Returns |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listening) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |
| `sockat::connect_nonblocking(path)` | (none) | `io::Result<UnixStream>` (nonblocking) |

There are also `connect_any` / `connect_any_nonblocking`, for rare environments ([below](#the-exception-connect_any)).

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

- A path that std accepts ([the length per OS](#how-a-long-path-is-reached)) goes to the std function unchanged (std has no nonblocking connect, so `connect_nonblocking` connects a socket of its own with the address std would build)
- The socket file is created at the path given. It is not removed when the listener is closed (as with std). To remove it, call `std::fs::remove_file(path)` with the original path (unlink has no `sun_path` limit)
- Sockets sockat creates itself are set up like std's (close-on-exec, `SO_NOSIGPIPE` where std sets it (macOS / iOS, FreeBSD, NetBSD, DragonFly), the same listen backlog as std)

## connect and connect_nonblocking

- **`connect`** replaces std's `UnixStream::connect`. It blocks
- **`connect_nonblocking`** is for async runtimes. The socket is made nonblocking before the connect and returned that way, ready for tokio's `UnixStream::from_std`. When the listener's backlog is full it fails at once (`EAGAIN` on Linux, `ECONNREFUSED` on macOS). A Unix-domain connect completes or fails within the call, so there is nothing to wait for afterwards (checked on Linux and macOS)

Both try every route but the fork route ([How a long path is reached](#how-a-long-path-is-reached)). Where only the fork route would reach the path, they return `ENAMETOOLONG`, as a plain connect to a path too long for `sun_path` does. In an ordinary environment (a writable `/tmp`) they reach a path whatever the directory depth and the file name length ([Where a path stops being reached](#where-a-path-stops-being-reached)).

## How a long path is reached

A path that std accepts (up to `sun_path` − 1 bytes) is handed to std unchanged.

| OS | Handed to std |
|---|---|
| Linux / Android / illumos | up to 107 bytes |
| macOS / iOS / FreeBSD / NetBSD / OpenBSD | up to 103 bytes |

A longer path is tried on the routes below, in this order per OS, and bound / connected on the first route that can be used. None of the routes changes the calling process's cwd, so they can be used from multi-threaded programs.

### Order per OS

| OS | Order tried |
|---|---|
| Linux / Android (`/proc` usable) | [/proc](#proc) |
| Linux / Android (`/proc` not usable) | [relative path](#relative-path) → [symlink](#symlink) → [fork](#fork) |
| FreeBSD | [bindat](#bindat) |
| macOS | [relative path](#relative-path) → [widened sockaddr](#widened-sockaddr) → [symlink](#symlink) → [fork](#fork) |
| NetBSD / OpenBSD / illumos / other Unix | [relative path](#relative-path) → [symlink](#symlink) → [fork](#fork) |
| Windows | not supported (the crate does not compile on non-Unix targets) |

Only `bind` and the `connect_any` variants try the fork route. `connect` / `connect_nonblocking` return `ENAMETOOLONG` instead of forking.

### The routes

#### /proc

Linux / Android. The directory is opened `O_PATH` and `/proc/self/fd/<fd>/<file name>` is passed to std. The kernel resolves that to the opened directory, so the socket is created there.

- The directory part has no length limit. The whole `/proc/self/fd/<fd>/<file name>` must be at most 107 bytes, so the file name can be about 90 bytes (depending on the number of digits of the fd)
- The directory is opened `O_PATH`, so no read permission on it is needed
- When it cannot be used, the other routes are tried in order. When it can, it is the only route used. It cannot be used when:
  - `/proc/self/fd/<fd>` cannot be looked up (a chroot or a minimal container with nothing mounted on `/proc`)
  - the file name does not fit the length above

#### bindat

FreeBSD. `bindat(2)` / `connectat(2)` bind / connect by the file name relative to the directory's fd.

- The directory part has no length limit. The file name can be up to 103 bytes. Longer is `ENAMETOOLONG`
- The directory is opened `O_RDONLY`, so read permission on it is needed as well

#### relative path

When std accepts the path spelled relative to the cwd, that is passed to std. Used only when the path given is absolute. Only the path string is rewritten; the cwd is never changed (no `chdir`).

- If the path is below the cwd (`getcwd`, or a `$PWD` that names the same directory as the cwd), the prefix is cut off. Otherwise both sides are resolved to real paths and the spelling climbs with `..`. The shorter one is used
- The relative spelling is checked to name the same directory as the path before it is used, and the cwd is checked to be unchanged after the bind / connect ([below](#the-relative-route-is-exposed-to-another-threads-chdir))

#### widened sockaddr

macOS. XNU takes an address whose `sun_path` runs past 104 bytes, as long as the path is at most 253 bytes. The path itself is passed when it is at most 253 bytes, failing that the relative spelling when that is.

#### symlink

A symlink `sockat.XXXXXXXXXX` (10 random characters) is created in a temporary directory, the bind / connect goes through it, and the symlink is removed right away.

| | the symlink points to | spelling passed | reached through `/tmp` |
|---|---|---|---|
| bind | the socket's directory | `<place>/sockat.XXXXXXXXXX/<file name>` | a file name of up to 84 bytes (80 where `sun_path` is 104, 230 on macOS) |
| connect | the socket file | `<place>/sockat.XXXXXXXXXX` | whatever the directory depth and the file name length |

- The symlink points to the path as given, so it resolves to the same place as the path
- The places tried are `TMPDIR` (when it is a non-empty absolute path), then `/tmp`. The first one where std accepts the spelling passed (on macOS, where it fits 253 bytes in the widened sockaddr), that is safe, and where the symlink can be created is used. When `TMPDIR` is too deep for it to fit, `/tmp` is used
- **A safe place** is a directory where no other user can replace the symlink in it, one of the following. Nothing is created in a place that is not safe; the next one is tried
  - sticky and owned by root or by the calling user (as `/tmp` is)
  - owned by the calling user with no write permission for group and others (as macOS's `$TMPDIR` is)
- Symlinks are followed to judge a place (macOS's `/tmp` is a symlink to `/private/tmp`)
- When the symlink cannot be created (the place does not exist or is not writable, a sandbox, no free name is found, no random bytes can be had), the next place is tried as well

#### fork

A child is forked that `fchdir`s into the opened directory, binds / connects by the file name alone, and exits at once. After the fork the parent's and the child's socket are the same socket, so the child's bind / connect takes effect on the caller's socket. The listen after a bind is done by the caller.

- **Only `bind` and the `connect_any` variants try it.** `connect` / `connect_nonblocking` return `ENAMETOOLONG` instead (decided after opening the directory, so a missing directory is still `ENOENT`)
- The file name must fit `sun_path`: up to 253 bytes on macOS (in the widened sockaddr), 107 on Linux and illumos, 103 on NetBSD, OpenBSD and the other BSDs. Longer is `ENAMETOOLONG`
- How the directory is opened, and the permission needed: macOS opens it `O_SEARCH`, so the same permissions as a plain bind / connect suffice (search, plus write for bind). Linux uses the fd opened `O_PATH` for the `/proc` route, so no read permission is needed. Other Unix targets open it `O_RDONLY` and need read permission on the directory as well
- A bind through the fork route has no side effect visible to peers, since the child only binds and the caller listens. There is no non-forking bind
- The child makes nothing but async-signal-safe calls (`fchdir`, `bind` / `connect`, `_exit`), so it can be used from multi-threaded processes
- It has side effects: [the peer pid after a connect](#side-effects-of-connecting-through-the-fork-route), and [the copies of the fds the child briefly holds](#the-forked-child-briefly-holds-copies-of-every-fd)

### How a failing route is treated

When a route actually binds / connects and fails (`ENOENT`, `EADDRINUSE`, `ECONNREFUSED`, ...), that error is returned and the next route is not tried. The next route is tried only when a route cannot be used at all (too long, `/proc` not usable, the kernel refusing the widened sockaddr, the place not being safe, the symlink not being creatable). The same goes for the places for the symlink: a bind / connect through one place that fails is not tried again through the next.

### Where a path stops being reached

When a path longer than std accepts is `ENAMETOOLONG`.

| OS | `bind` | `connect` / `connect_nonblocking` | the `connect_any` variants |
|---|---|---|---|
| Linux / Android (with `/proc`) | the file name is over 107 bytes | the file name does not fit the `/proc` spelling, the relative path is over 107 bytes, and the symlink route cannot be used | the file name is over 107 bytes and the symlink route cannot be used |
| Linux / Android (without `/proc`) | the file name is over 107 bytes | the relative path is over 107 bytes and the symlink route cannot be used | the file name is over 107 bytes and the symlink route cannot be used |
| FreeBSD | the file name is over 103 bytes | the file name is over 103 bytes | the file name is over 103 bytes |
| macOS | the file name is over 253 bytes | the path and the relative path are both over 253 bytes and the symlink route cannot be used | the file name is over 253 bytes and the symlink route cannot be used |
| NetBSD / OpenBSD / other BSDs | the file name is over 103 bytes | the relative path is over 103 bytes and the symlink route cannot be used | the file name is over 103 bytes and the symlink route cannot be used |
| illumos | the file name is over 107 bytes | the relative path is over 107 bytes and the symlink route cannot be used | the file name is over 107 bytes and the symlink route cannot be used |

- For a connect, the symlink route cannot be used only when there is no safe place where the symlink can be created (a sandbox where neither `TMPDIR` nor `/tmp` can be used, and the like). Through `/tmp` the length always fits

## The exception: connect_any

`connect_any` / `connect_any_nonblocking` are only for the rare environment where `connect` is `ENAMETOOLONG` in the table above, when you want the connection at the cost of the fork's side effects. Normally they are not needed.

| Function | Returns |
|---|---|
| `sockat::connect_any(path)` | `io::Result<(UnixStream, bool)>` (blocking) |
| `sockat::connect_any_nonblocking(path)` | `io::Result<(UnixStream, bool)>` (nonblocking) |

The order tried is that of `connect`; when no other route can be used, the connect is finally made on the [fork](#fork) route. The `bool` is `true` when the fork route was taken, and `false` otherwise. When it is `true` the side effects below apply, so it can be used, for example, to log or warn about it. FreeBSD never forks, so there it is always `false`.

`connect_any_nonblocking` makes the socket nonblocking before the fork, so the child's connect does not wait either.

### Side effects of connecting through the fork route

**The peer pid the server sees is the pid of a process that has already exited** (the forked child). The socket itself belongs to the caller and works as usual.

- The uid / gid the server sees are the caller's
- On macOS (`LOCAL_PEERPID`), once the caller sends on the socket, the server sees the caller's pid instead. On Linux (`SO_PEERCRED`) it stays the exited child's pid
- If the server looks the pid up for information about the process (its executable, its parent by following the ppid, ...), there is nothing to find: the process is gone
- The number of an exited pid may be reused by another process later. If it has been, the server gets the information of an unrelated process
- So the pid is of no use to the caller or to the server. `connect_any` does not return its number, only whether the fork route was taken
- How peer credentials behave on NetBSD, OpenBSD and illumos was not checked

## What differs from std for a long path

### The address the kernel reports is not the path

`local_addr()` (of the listener and of accepted streams) and `peer_addr()` (of a connected stream) report the spelling handed to the kernel when the listener was bound. Which route the connecting side took does not matter.

| Route of the bind | Reported |
|---|---|
| /proc | `/proc/self/fd/<fd>/<file name>` (the fd is closed by then) |
| relative path | the relative path |
| widened sockaddr | a truncated path |
| symlink | a path through a symlink that no longer exists |
| fork, bindat | the file name alone |

Keep the original path yourself and use that.

On OpenBSD, std's `local_addr()` drops the last byte of the path even for a short path of at most 103 bytes (which sockat hands to std unchanged; seen in CI). This is std's / the OS's behaviour, not sockat's: std's `UnixListener::bind` used directly gives the same.

### A path that does not end with a name is InvalidInput

A path longer than std accepts whose last part is not a file name (`dir/`, `dir/.`, `dir/..`, `..`) is refused with `InvalidInput` before anything is created: it cannot be split into a directory and a file name. A shorter path goes to std unchanged and gets the kernel's error (for example a bind of `dir/` is `EADDRINUSE` on both macOS and Linux).

### The forked child briefly holds copies of every fd

On the fork route (`bind` and the `connect_any` variants), as with any fork, the child holds copies of all file descriptors of the process until it exits. A socket or pipe another thread closes at that moment stays open until then (the peer sees EOF slightly later; a listener closed meanwhile can still take a connection into its backlog).

### The directory is looked up slightly before the call

The /proc, relative path, bindat and fork routes look the directory up first (open it, or resolve it to a real path), then bind / connect by the file name. If a directory along the path is renamed, or a symlink along it repointed, in between, the socket is created in (or, for a connect, looked up in) the directory as it was when looked up. The widened sockaddr and symlink routes leave the path as given for the kernel to resolve, so in this respect they behave as std does.

### The relative route is exposed to another thread's chdir

sockat itself never changes the cwd. When the relative spelling is made it is checked to name the same directory, and after the bind / connect the cwd is checked to be unchanged. If another thread changed the cwd in between, the result is discarded and an error (`Other`) is returned (a socket file it may have created is left alone, since where it went is unknown). A change that is undone in between is not visible. A program that changes the cwd while binding / connecting races every relative path in the process anyway; sockat does not make that safe, it only detects the visible case.

### A symlink can be left behind

If the process dies between creating and removing the symlink, `sockat.XXXXXXXXXX` (pointing to the socket's directory or file) is left under `TMPDIR` or `/tmp`. The `sockat.` prefix tells it apart as sockat's.

## Errors

| Situation | Error |
|---|---|
| the directory does not exist | `ENOENT` |
| nothing to connect to | `ENOENT` |
| a file already exists at the bind path | `EADDRINUSE` |
| a socket file nobody listens on | `ECONNREFUSED` |
| the backlog is full | on Linux `EAGAIN` for a nonblocking connect (a blocking one waits for room), on macOS `ECONNREFUSED` |
| no route reaches the path ([Where a path stops being reached](#where-a-path-stops-being-reached)); for `connect` / `connect_nonblocking`, also when only the fork route would | `ENAMETOOLONG` |
| a path longer than std accepts that does not end with a name (`dir/`, `dir/.`, `..`, ...) | `InvalidInput` |
| a path containing NUL | `InvalidInput` |
| the cwd changed during the relative route | `Other` |
| the forked child did not exit normally; or its exit status was collected elsewhere (`SIGCHLD` set to `SIG_IGN`, a `waitpid(-1)` in another thread) and the socket is not bound / connected | `Other` |

Errors are `std::io::Error`; for OS errors `raw_os_error()` gives the errno.

On macOS a connect also returns `ECONNREFUSED` (blocking or not) when the listener's backlog is full, so `ECONNREFUSED` alone does not tell a stale socket file that nobody listens on. Code that removes the socket file on seeing `ECONNREFUSED` can, on macOS, remove the socket file of a live listener. This is the kernel's behaviour, the same with std's connect (on Linux, with a full backlog, a nonblocking connect returns `EAGAIN` and a blocking one waits for room).

## Async runtimes

The results are std types, so any runtime that adopts std sockets can take them. With tokio:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` does not wait for the peer, so it can be called on the runtime's workers directly. `bind` does not wait for a peer either (the bind and the listen return at once; on the fork route it waits for the child, which binds and exits at once).

## Not checked

- Android has not even been compiled. It uses the same routes as Linux
- NetBSD, OpenBSD and illumos run the tests on VMs in CI, but how the peer credentials (pid) behave after a connect through the fork route was not checked
- The error a nonblocking connect returns for a full backlog, and that a connect completes within the call, were checked only on Linux and macOS
- Apple OSes other than macOS (iOS and others) use the same routes as macOS, but only iOS's clippy was checked
