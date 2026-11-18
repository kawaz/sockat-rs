# sockat

> English | [日本語](./README-ja.md)

A Rust library that binds and connects Unix domain sockets even when the path is longer than `sun_path` allows (104 bytes on macOS and the BSDs, 108 on Linux, NUL included).

The language-independent behaviour is defined by the [sockat specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md); sockat-rs is its Rust implementation.

```rust
let listener = sockat::bind("/a/very/deep/directory/.../server.sock")?;
let stream = sockat::connect("/a/very/deep/directory/.../server.sock")?;
```

## Usage

Use `sockat::bind` / `sockat::connect` where you would use std's `UnixListener::bind` / `UnixStream::connect`. In async code, use `sockat::connect_nonblocking`, which does not wait and returns a nonblocking stream.

- The results are std's own `UnixListener` / `UnixStream`
- A path that std accepts is handed to std unchanged, so it behaves exactly as with std
- Even for a long path, the socket file is created at the path given
- `connect` too tries every route before the fork route (a short symlink under a temporary directory, and on Linux and the Apple OSes a route that moves the cwd of one thread only), so in an ordinary environment it reaches long paths as well

## Why

std's `bind` / `connect` refuse a path that does not fit `sun_path`. The file system has no such limit, but deep temporary directories hit it quickly (macOS's `$TMPDIR` alone is about 50 bytes, and test harnesses and sandboxes nest further). The limit applies to the argument passed to `bind(2)` / `connect(2)`, so sockat hands the kernel a short spelling that names the same place.

## How a long path is reached

A path that std accepts (up to `sun_path` − 1 bytes) is handed to std unchanged.

| OS | Handed to std |
|---|---|
| Linux / Android / illumos | up to 107 bytes |
| macOS / iOS / FreeBSD / NetBSD / OpenBSD | up to 103 bytes |

A longer path is tried on the routes below, in this order per OS, and bound / connected on the first route that can be used.

| OS | Order tried |
|---|---|
| Linux / Android (`/proc` usable) | /proc |
| Linux / Android (`/proc` not usable) | relative path → symlink → thread cwd → fork |
| FreeBSD | bindat |
| macOS / iOS and the other Apple OSes | relative path → widened sockaddr → symlink → thread cwd → fork |
| NetBSD / OpenBSD / illumos / other Unix | relative path → symlink → fork |
| Windows | not supported (compile error) |

- **/proc**: open the directory and pass `/proc/self/fd/<fd>/<file name>`. Not usable without `/proc`, or when the file name does not fit in it (over about 90 bytes)
- **bindat**: `bindat(2)` / `connectat(2)`, by the file name relative to the directory fd
- **relative path**: when std accepts the path relative to the cwd, pass that. Only the path string is rewritten; the cwd is never changed
- **widened sockaddr**: pass a sockaddr carrying up to 253 bytes, which the macOS kernel accepts
- **symlink**: create a short symlink under `TMPDIR` or `/tmp` and go through it. For a bind it points to the socket's directory, for a connect to the socket file. It is created only in a place where no other user can replace it, and removed at once
- **thread cwd**: move the cwd of a throwaway thread, and of it alone, into the directory (Linux: `unshare(CLONE_FS)` and `fchdir`; Apple OSes: `pthread_fchdir_np`) and bind / connect by the file name alone. It has no side effects, so every function tries it
- **fork**: fork a child that `fchdir`s into the directory and binds / connects by the file name alone. **Only `bind` and the `connect_any` variants try it**

None of the routes changes the calling process's cwd, so they can be used from multi-threaded programs.

`connect` / `connect_nonblocking` never fork. Where only the fork route would reach the path, they return `ENAMETOOLONG`, as a plain connect to a path too long for `sun_path` does. That happens only when the path fits neither the `/proc` spelling nor a relative path (nor, on the Apple OSes, the widened sockaddr), there is no safe place to create the symlink, and the thread cwd route cannot be used either (NetBSD / OpenBSD / illumos, Linux where `unshare` is refused, an Apple OS without `pthread_fchdir_np`). With a usable `/tmp`, `connect` reaches the path through the symlink whatever the directory depth and the file name length. The length limits of each route, and where `connect` stops reaching a path, are in [docs/MANUAL.md](./docs/MANUAL.md#how-a-long-path-is-reached).

## The exception: `connect_any`

`connect_any` / `connect_any_nonblocking` are only for the rare environment that hits the `ENAMETOOLONG` above, when you want the connection at the cost of the fork's side effects. When no other route can be used, they have a forked child make the connect (the child exits at once), and they also return whether they took the fork route (`(UnixStream, bool)`, `true` when they did), for example to log or warn about it.

After a connect through the fork route, the peer pid the server sees is that of a process that has already exited (the forked child). Details in [docs/MANUAL.md](./docs/MANUAL.md#the-exception-connect_any).

## What differs from std for a long path

- `local_addr()` / `peer_addr()` do not return the path: they return the spelling handed to the kernel at bind time (a relative path, `/proc/self/fd/...`, a path through a symlink that no longer exists, ...). Keep the path yourself
- A path longer than std accepts that does not end with a file name (ending in `dir/`, `dir/.`, `..`) is an `InvalidInput` error
- The rest (the fork's side effects, the directory being looked up first, a symlink that may be left behind) is in [docs/MANUAL.md](./docs/MANUAL.md#what-differs-from-std-for-a-long-path)

## Async runtimes

The only dependency is libc; no runtime is required. With tokio, convert from the std types:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

## Documentation

- [docs/MANUAL.md](./docs/MANUAL.md) — usage, the routes in detail, what differs from std, errors
- [docs/decisions/](./docs/decisions/INDEX.md) — design decisions (DRs, in Japanese)

## License

MIT License, Yoshiaki Kawazu (@kawaz)
