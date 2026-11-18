# sockat manual

> English | [日本語](./MANUAL-ja.md)

How to use sockat, and how it differs from std for long paths. The design decisions and their reasons are in [DR-0001](./decisions/DR-0001-long-socket-path-routes.md) (in Japanese).

## Usage

```toml
[dependencies]
sockat = "0.2"
```

In general, use `sockat::bind` / `sockat::connect` where you would use std's `bind` / `connect`. In async code, use `sockat::connect_nonblocking`.

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

- A path of at most 100 bytes goes to the std function unchanged
- The socket file is created at the path given. It is not removed when the listener is closed (as with std). To remove it, call `std::fs::remove_file(path)` with the original path (unlink has no `sun_path` limit)
- Sockets sockat creates itself are set up like std's (close-on-exec, `SO_NOSIGPIPE` on macOS, the same listen backlog as std)

## connect and connect_nonblocking

- **`connect`** replaces std's `UnixStream::connect`. It blocks
- **`connect_nonblocking`** is for async runtimes. The socket is made nonblocking before the connect and returned that way, ready for tokio's `UnixStream::from_std`. When the listener's backlog is full it fails at once (`EAGAIN` on Linux, `ECONNREFUSED` on macOS). A Unix-domain connect completes within the call, so there is nothing to wait for afterwards

Both try every way up to the short symlink under a temporary directory (`TMPDIR` or `/tmp`). So in an ordinary environment (a writable `/tmp`) they reach a path at any directory depth, unless the file name itself is extremely long (see "Per OS" below). They go no further (no fork): where nothing else reaches the path they return `ENAMETOOLONG`, as a plain connect to a path too long for `sun_path` does.

`bind` forks as a last resort. The child only binds and the listen is done by the calling process, so nothing of the fork stays with the listener (apart from the child briefly holding copies of the fds, below).

## The exception: connect_any

`connect_any` / `connect_any_nonblocking` are only for the rare environment where even `connect` cannot reach the path (Linux with neither `/proc` nor a safe temporary directory, a Unix with neither a dirfd-relative connect nor a usable temporary directory, or a file name too long even for the symlink route), when you want the connection at the cost of the fork's side effects. Normally they are not needed.

| Function | Returns |
|---|---|
| `sockat::connect_any(path)` | `io::Result<(UnixStream, Option<u32>)>` (blocking) |
| `sockat::connect_any_nonblocking(path)` | `io::Result<(UnixStream, Option<u32>)>` (nonblocking) |

When every other route is unusable, a child that `fchdir`s into the directory is forked and makes the connect. The `Option<u32>` is that child's pid when the fork route was taken (`Some`), and `None` otherwise.

### Side effects of connecting through the fork route

**The peer pid the server sees is the pid of a process that has already exited** (the forked child). The socket itself belongs to the caller and works as usual.

- The uid / gid the server sees are the caller's
- On macOS (`LOCAL_PEERPID`), once the caller sends on the socket, the server sees the caller's pid instead. On Linux (`SO_PEERCRED`) it stays the exited child's pid
- If the server looks the pid up for information about the process (its executable, its parent by following the ppid, ...), there is nothing to find: the process is gone. If the number has been reused, the server gets the information of an unrelated process
- The number of an exited pid may be reused by another process later. The pid `connect_any` returns is a record of who connected, not a process to signal; when matching it against the server's logs, read it together with the time
- NetBSD, OpenBSD and illumos were not checked

## Per OS

For a path over 100 bytes: which routes `connect` reaches it by (without forking), and in which rare cases the fork route (`connect_any` and its nonblocking variant, `bind`) is needed.

| OS | Reached by `connect` | Fork needed (`connect` returns `ENAMETOOLONG`) when |
|---|---|---|
| Linux / Android (with `/proc`) | `/proc/self/fd/<fd>/<name>`, at any directory depth | never (it does not fork). A file name over about 90 bytes is `ENAMETOOLONG` for every function |
| FreeBSD | `bindat(2)` / `connectat(2)`, at any directory depth | never (it does not fork) |
| macOS | relative path (up to 100 bytes) → widened sockaddr (up to 253 bytes) → symlink (up to 253 bytes) | the file name is over 230 bytes and both the path and its relative spelling are over 253 bytes; or there is no safe place where the symlink can be created |
| NetBSD / OpenBSD / illumos / other Unix, Linux / Android without `/proc` | relative path (up to 100 bytes) → symlink (up to 100 bytes) | the file name is over 77 bytes and the relative spelling over 100 bytes; or there is no safe place where the symlink can be created |
| Windows | not supported (does not compile) | |

The symlink route hands the kernel `/tmp/sockat.XXXXXXXXXX/<file name>` (23 bytes before the file name), so with `/tmp` usable any directory depth is reached. Forking is needed only for a long file name, or when neither `TMPDIR` nor `/tmp` can be used (sandboxes and the like).

## How a long path is reached

None of the routes changes the calling process's cwd.

### Linux / Android

The directory is opened `O_PATH` and `/proc/self/fd/<fd>/<file name>` is passed to std. The directory part has no length limit. With `/proc` there, nothing else is tried.

Without `/proc` (a chroot or a minimal container with nothing mounted on `/proc`), the routes of "other Unix" below are tried in the same order. When the `/proc` route actually fails, that error is returned; it is not retried on another route.

### FreeBSD

`bindat(2)` / `connectat(2)` bind / connect by the file name relative to the directory's fd.

### macOS, NetBSD, OpenBSD, illumos, other Unix

Tried in this order; the first that can be used binds / connects.

1. **The path relative to the cwd** (if at most 100 bytes). If the path is below the cwd (`getcwd`, or a `$PWD` that names the same directory as the cwd), the prefix is cut off. Otherwise both sides are resolved to real paths and the spelling climbs with `..`
2. **A widened sockaddr** (macOS only, up to 253 bytes). XNU takes an address whose `sun_path` runs past 104 bytes, up to 255 bytes in total. The path itself, or failing that the relative spelling
3. **Through a short symlink.** A symlink `<place>/sockat.XXXXXXXXXX` → the socket's directory is created, the bind / connect goes through it, and it is removed right away. The places tried are `TMPDIR` (when absolute), then `/tmp`; the first one where the path through the symlink fits (up to 253 bytes on macOS, 100 elsewhere), that is safe, and where the symlink can be created is used (when `TMPDIR` is too deep to shorten the path, `/tmp` is used)
   - **A safe place** is a directory where no other user can replace the symlink: sticky and owned by root or by the calling user (as `/tmp` is), or owned by the calling user with no write permission for group and others (as macOS's `$TMPDIR` is). Symlinks are followed for this (macOS's `/tmp` is a symlink to `/private/tmp`). Nothing is created in a place that is not safe; the next one is tried
4. **Fork.** A child that `fchdir`s into the directory's fd binds / connects by the file name alone. Only `bind` and the `connect_any` variants try it; `connect` / `connect_nonblocking` return `ENAMETOOLONG` instead

When a way actually fails (`ENOENT`, `EADDRINUSE`, ...), that error is returned and the next way is not tried. The next way is tried only when a way cannot be used at all (too long, the place is not safe, the symlink cannot be created, ...). The same goes for the places for the symlink: a bind / connect through one place that fails is not tried again through the next.

### Windows

Not supported. The crate does not compile on non-Unix targets.

## What differs from std for a long path

### The address the kernel reports is not the path

`local_addr()` (of the listener and of accepted streams) and `peer_addr()` (of a connected stream) report the spelling actually handed to the kernel.

| Route | Reported |
|---|---|
| Linux's `/proc` | `/proc/self/fd/<fd>/<name>` (the fd is closed by then) |
| relative | the relative path |
| widened sockaddr | a truncated path |
| symlink | a path through a symlink that no longer exists |
| fork / bindat | the file name alone |

Keep the original path yourself and use that.

### The forked child briefly holds copies of every fd

On the fork route (`bind` and the `connect_any` variants), as with any fork, the child holds copies of all file descriptors of the process until it exits. A socket or pipe another thread closes at that moment stays open until then (the peer sees EOF slightly later; a closed listener can still take a connection). On macOS the fork route opens the directory `O_SEARCH`, so it needs the same permissions as a plain bind / connect (search, plus write for bind). On Linux the fork route enters the directory opened `O_PATH`, as the `/proc` route does, and needs only search permission. The fork route on other Unix and FreeBSD's bindat open it `O_RDONLY` and need read permission on the directory as well.

### The directory is looked up slightly before the call

The `/proc`, relative, bindat and fork routes look the directory up first (open it, or resolve it to a real path), then bind / connect by the file name. If a directory along the path is renamed, or a symlink along it repointed, in between, the socket is created in the directory as it was when looked up.

### The relative route depends on the cwd

Before using a relative spelling it is checked to name the same directory, and after the bind / connect the cwd is checked to be unchanged. If another thread changed the cwd in between, the result is discarded and an error returned (a socket file it may have created is left alone, since where it went is unknown). A change that is undone in between is not visible. A program that changes the cwd while binding / connecting races every relative path in the process anyway; sockat does not make that safe, it only detects the visible case.

### A symlink can be left behind

If the process dies between creating and removing the symlink, `sockat.XXXXXXXXXX` (pointing to the socket's directory) is left under `TMPDIR` or `/tmp`. The `sockat.` prefix tells it apart as sockat's. A relative `TMPDIR` is not used for it (`/tmp` still is).

## Errors

| Situation | Error |
|---|---|
| the directory does not exist | `ENOENT` |
| nothing to connect to | `ENOENT` |
| a file already exists at the bind path | `EADDRINUSE` |
| a socket file nobody listens on | `ECONNREFUSED` |
| the backlog is full (nonblocking connect, Linux) | `EAGAIN` |
| `connect` / `connect_nonblocking` to a path only the fork route reaches | `ENAMETOOLONG` |
| the file name alone fits no route (over about 90 bytes on Linux (107 without `/proc`), 253 on macOS, 103 on other Unix) | `ENAMETOOLONG` |
| a path not ending in a name (`dir/`, `dir/.`, `..`), a path containing NUL | `InvalidInput` |
| the cwd changed during the relative route | `Other` |

Errors are `std::io::Error`; for OS errors `raw_os_error()` gives the errno.

## Async runtimes

The results are std types, so any runtime that adopts std sockets can take them. With tokio:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` does not wait, so it can be called on the runtime's workers directly. `bind` has nothing to wait for (the bind and the listen return at once).
