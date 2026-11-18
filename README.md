# sockat

> English | [日本語](./README-ja.md)

A Rust library that binds and connects Unix domain sockets even when the path is longer than `sun_path` allows (104 bytes on macOS, 108 on Linux, NUL included).

```rust
let listener = sockat::bind("/a/very/deep/directory/.../server.sock")?;
let stream = sockat::connect("/a/very/deep/directory/.../server.sock")?;
```

The results are std's own `UnixListener` / `UnixStream`. A path of at most 100 bytes is handed to std unchanged, so short paths behave exactly as with std.

In general, use `sockat::bind` / `sockat::connect` where you would use std's `bind` / `connect`. In async code, use `sockat::connect_nonblocking`, which does not wait and returns a nonblocking stream. `connect` tries every way up to a short symlink under a temporary directory, so in an ordinary environment it reaches long paths too.

## Why

std's `UnixListener::bind` / `UnixStream::connect` refuse a path that does not fit `sun_path`. The file system has no such limit, but deep temporary directories hit it quickly (macOS's `$TMPDIR` alone is about 50 bytes, and test harnesses and sandboxes nest further). The limit applies to the argument passed to `bind(2)` / `connect(2)`, so sockat hands the kernel a different, short spelling.

## How a long path is reached

| Platform | Way |
|---|---|
| Linux / Android | open the directory `O_PATH` and pass `/proc/self/fd/<fd>/<name>` (without `/proc`, the order of other Unix) |
| FreeBSD | `bindat(2)` / `connectat(2)` |
| macOS | in order: the path relative to the cwd → a widened sockaddr carrying up to 253 bytes → through a short symlink under `TMPDIR` or `/tmp` → a forked child that `fchdir`s into the directory |
| other Unix (NetBSD, OpenBSD, illumos, ...) | in order: the path relative to the cwd → through a short symlink under `TMPDIR` or `/tmp` → a forked child that `fchdir`s into the directory |
| Windows | not supported |

None of these changes the calling process's cwd, and all of them work in multi-threaded programs. The socket file is always created at the path given. The symlink is created only in a place where no other user can replace it (a sticky directory such as `/tmp`, or one of the user's own).

Only `bind` and the `connect_any` variants of the next section fork. `bind`'s child only binds and the listen is done by the caller, so nothing of the fork stays with the listener.

## The exception: `connect_any`

`connect` / `connect_nonblocking` never fork. Where only a fork would reach the path, they return `ENAMETOOLONG`, as a plain connect to a path too long for `sun_path` does.

`connect_any` / `connect_any_nonblocking` are only for the rare environment where even they cannot reach the path (Linux with neither `/proc` nor a safe temporary directory, a Unix with neither a dirfd-relative connect nor a usable temporary directory, or a file name too long even for the symlink route), when you want the connection at the cost of the fork's side effects. As the last resort they have a forked child make the connect and exit, and they also return whether they took the fork route (`(UnixStream, bool)`, `true` when they did), for example to log or warn about it.

## What differs from std for a long path

- `local_addr()` / `peer_addr()` do not return the path (they return the spelling actually handed to the kernel). Keep the path yourself
- When the `connect_any` variants do take the fork route, the peer pid the server sees is the pid of a process that has already exited (the forked child). The socket itself belongs to the caller and works as usual. The uid / gid are the caller's. On macOS the server sees the caller's pid once the caller sends. Nothing (the process, its ppid) can be looked up from that pid, and its number may be reused by another process
- Details in [docs/MANUAL.md](./docs/MANUAL.md)

## Async runtimes

The only dependency is libc; no runtime is required. With tokio, convert from the std types (`connect_nonblocking` does not wait and returns a nonblocking stream):

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

## Documentation

- [docs/MANUAL.md](./docs/MANUAL.md) — usage, errors, caveats
- [docs/decisions/](./docs/decisions/INDEX.md) — design decisions (DRs, in Japanese)

## License

MIT License, Yoshiaki Kawazu (@kawaz)
