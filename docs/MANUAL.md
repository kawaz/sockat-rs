# sockat manual

> English | [日本語](./MANUAL-ja.md)

How to use sockat, and how it differs from std for long paths. The design decisions and their reasons are in [DR-0001](./decisions/DR-0001-long-socket-path-routes.md) (in Japanese).

## Usage

```toml
[dependencies]
sockat = "0.1"
```

There are two functions.

| Function | std counterpart | Returns |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listening) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |

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

## How a long path is reached

A path over 100 bytes is reached as follows, per platform. None of these changes the calling process's cwd.

### Linux / Android

The directory is opened `O_PATH` and `/proc/self/fd/<fd>/<file name>` is passed to std. The directory part has no length limit. `/proc` is required.

### macOS

Tried in this order; the first that can be used binds / connects.

1. **The path relative to the cwd** (if at most 100 bytes). If the path is below the cwd (`getcwd`, or a `$PWD` that names the same directory as the cwd), the prefix is cut off. Otherwise both sides are resolved to real paths and the spelling climbs with `..`
2. **A widened sockaddr** (up to 253 bytes). XNU takes an address whose `sun_path` runs past 104 bytes, up to 255 bytes in total. The path itself, or failing that the relative spelling
3. **Through a short symlink.** A symlink `${TMPDIR:-/tmp}/sockat.XXXXXXXXXX` → the socket's directory is created, the bind / connect goes through it, and it is removed right away
4. **Fork.** A child that `fchdir`s into the directory's fd binds / connects by the file name alone

When a way actually fails (`ENOENT`, `EADDRINUSE`, ...), that error is returned and the next way is not tried. The next way is tried only when a way cannot be used at all (too long, the symlink cannot be created, ...).

### FreeBSD

`bindat(2)` / `connectat(2)` bind / connect by the file name relative to the directory's fd.

### Other Unix

Fork (as in step 4 for macOS).

### Windows

Not supported. The crate does not compile on non-Unix targets.

## What differs from std for a long path

### The address the kernel reports is not the path

`local_addr()` (of the listener and of accepted streams) and `peer_addr()` (of a connected stream) report the spelling actually handed to the kernel.

| Route | Reported |
|---|---|
| Linux | `/proc/self/fd/<fd>/<name>` (the fd is closed by then) |
| relative | the relative path |
| widened sockaddr | a truncated path |
| symlink | a path through a symlink that no longer exists |
| fork / bindat | the file name alone |

Keep the original path yourself and use that.

### On the fork route the connecting pid is the child's

When a client connects through the fork route (macOS step 4, other Unix), the peer the server sees is a short-lived child. On macOS `LOCAL_PEERPID` reports the child's pid, and the client's pid after the client has sent something. The uid / gid are the client's either way. Linux never forks.

### The forked child briefly holds copies of every fd

As with any fork, the child holds copies of all file descriptors of the process until it exits. A socket or pipe another thread closes at that moment stays open until then (the peer sees EOF slightly later; a closed listener can still take a connection). On macOS the fork route opens the directory `O_SEARCH`, so it needs the same permissions as a plain bind / connect (search, plus write for bind). The fork route on other Unix and FreeBSD's bindat open it `O_RDONLY` and need read permission on the directory as well.

### The directory is looked up slightly before the call

The Linux, relative, bindat and fork routes look the directory up first (open it, or resolve it to a real path), then bind / connect by the file name. If a directory along the path is renamed, or a symlink along it repointed, in between, the socket is created in the directory as it was when looked up.

### The relative route depends on the cwd (macOS)

Before using a relative spelling it is checked to name the same directory, and after the bind / connect the cwd is checked to be unchanged. If another thread changed the cwd in between, the result is discarded and an error returned (a socket file it may have created is left alone, since where it went is unknown). A change that is undone in between is not visible. A program that changes the cwd while binding / connecting races every relative path in the process anyway; sockat does not make that safe, it only detects the visible case.

### A symlink can be left behind (macOS)

If the process dies between creating and removing the symlink, `${TMPDIR:-/tmp}/sockat.XXXXXXXXXX` (pointing to the socket's directory) is left. The `sockat.` prefix tells it apart as sockat's. A relative `TMPDIR` disables the symlink route.

## Errors

| Situation | Error |
|---|---|
| the directory does not exist | `ENOENT` |
| nothing to connect to | `ENOENT` |
| a file already exists at the bind path | `EADDRINUSE` |
| a socket file nobody listens on | `ECONNREFUSED` |
| the file name alone fits no route (over about 90 bytes on Linux, 253 on macOS) | `ENAMETOOLONG` |
| a path not ending in a name (`dir/`, `dir/.`, `..`), a path containing NUL | `InvalidInput` |
| the cwd changed during the relative route (macOS) | `Other` |
| no `/proc` on Linux | an error saying `/proc` is needed |

Errors are `std::io::Error`; for OS errors `raw_os_error()` gives the errno.

## Async runtimes

The results are std types, so any runtime that adopts std sockets can take them. With tokio:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect(&path)?;
stream.set_nonblocking(true)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`bind` / `connect` block like std's. A local `connect(2)` returns at once unless the listener's backlog is full; to keep even that off the runtime's workers, call it inside `spawn_blocking`.
