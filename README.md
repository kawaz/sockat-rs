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
- `connect` too tries every route before the fork route, so in an ordinary environment it reaches long paths as well
- To remove the socket file later, use `sockat::bind_ex`. It returns the listener and a `SocketFile` that removes only the file the bind made

## Why

std's `bind` / `connect` refuse a path that does not fit `sun_path`. The file system has no such limit, but deep temporary directories hit it quickly (macOS's `$TMPDIR` alone is about 50 bytes, and test harnesses and sandboxes nest further). The limit applies to the argument passed to `bind(2)` / `connect(2)`, so sockat hands the kernel a short spelling that names the same place.

## How a long path is reached

A path that std accepts (up to `sun_path` − 1 bytes) is handed to std unchanged.

| OS | Handed to std |
|---|---|
| Linux / Android / illumos | up to 107 bytes |
| macOS / iOS / FreeBSD / NetBSD / OpenBSD | up to 103 bytes |

A longer path is reached first by **the central route**. sockat makes a directory D of its own (`sockat.XXXXXXXXXX`, mode 0700) and holds it by an fd, makes in it a symlink R with a random name (pointing to the socket's directory), binds / connects with `R/<file name>` relative to D's fd, and removes R and D. The spelling the kernel keeps is `R/<file name>`; R is gone by then, so wherever it is resolved later, it hits nothing.

| OS | Means of going through D's fd (in this order) |
|---|---|
| Linux / Android | thread cwd → /proc version → fork (bind only) |
| macOS / iOS and the other Apple OSes | thread cwd → fork (bind only) |
| FreeBSD | bindat / connectat |
| NetBSD / OpenBSD / illumos / other Unix | fork (bind only) |
| Windows | not supported (compile error) |

- **thread cwd**: move the cwd of a throwaway thread, and of it alone, into D (Linux: `unshare(CLONE_FS)` and `fchdir`; Apple OSes: `pthread_fchdir_np`). It has no side effects, so every function tries it. Not used on Linux under a seccomp filter
- **/proc version**: pass `/proc/self/fd/<D's fd>/R/<file name>`
- **bindat / connectat**: pass D's fd
- **fork**: fork a child that `fchdir`s into D and binds. The caller listens afterwards, so it has no side effect visible to peers
- D goes in `TMPDIR`, `/tmp` or the socket's directory (`.sockat.XXXXXXXXXX`), the first where it can be made. A place needs to be neither safe nor short (D is held by its fd). Where D can be made nowhere, a bind makes R in the socket's directory and forks

When the central route cannot be used, the connects alone try **the fallback routes**. The kernel keeps no spelling of a connect, so these carry no random part.

| OS | Fallback routes (in this order) |
|---|---|
| Linux / Android (`/proc` usable) | /proc → a symlink to the file in D → symlink |
| Linux / Android (no `/proc`) | relative path → symlink |
| macOS / iOS and the other Apple OSes | relative path → widened sockaddr → symlink |
| FreeBSD | connectat |
| NetBSD / OpenBSD / illumos / other Unix | relative path → symlink |

- **/proc**: pass `/proc/self/fd/<directory fd>/<file name>`
- **a symlink to the file in D**: make a symlink to the socket file in D and pass `/proc/self/fd/<D's fd>/s`
- **relative path**: when std accepts the path relative to the cwd, pass that. Only the path string is rewritten; the cwd is never changed
- **widened sockaddr**: pass a sockaddr carrying up to 253 bytes, which the macOS kernel accepts (the address of a socket bound through it is cut short and breaks its readers, so a bind never uses it)
- **symlink**: create a short symlink to the socket file under `TMPDIR` or `/tmp`, in a place where no other user can replace it, go through it, and remove it at once

The `connect_any` variants fork last when none of these reaches the path. None of the routes changes the cwd of the calling process or thread, so they can be used from multi-threaded programs.

`connect` / `connect_nonblocking` never fork. Where only the fork route would reach the path, they return `ENAMETOOLONG`, as a plain connect to a path too long for `sun_path` does. That happens only when the central route cannot be used for a connect (NetBSD / OpenBSD / illumos, Linux under a seccomp filter without `/proc`, an Apple OS without `pthread_fchdir_np`) and no fallback route reaches the path either. With a usable `/tmp`, `connect` reaches the path whatever the directory depth and the file name length. The length limits of each route, and where reaching stops, are in [docs/MANUAL.md](./docs/MANUAL.md#how-a-long-path-is-reached).

## The exception: `connect_any`

`connect_any` / `connect_any_nonblocking` are only for the rare environment that hits the `ENAMETOOLONG` above, when you want the connection at the cost of the fork's side effects. When no other route can be used, they have a forked child make the connect (the child exits at once), and they also return whether they took the fork route (`AnyConnection { stream, forked }`, `forked` is `true` when they did), for example to log or warn about it.

They are off by default and available only with the feature `any`: `sockat = { version = "0.4", features = ["any"] }`

After a connect through the fork route, the peer pid the server sees is that of a process that has already exited (the forked child). Details in [docs/MANUAL.md](./docs/MANUAL.md#the-exception-connect_any).

## What differs from std for a long path

- `local_addr()` / `peer_addr()` do not return the path: they return the spelling handed to the kernel at bind time (`R/<file name>`, `/proc/self/fd/<fd>/R/<file name>`, ...). R is gone, so it hits nothing. Keep the path yourself
- A bind reaches shorter file names, by R's length (up to 96 bytes on Linux / illumos, 92 on the Apple OSes / the BSDs)
- A path longer than std accepts that does not end with a file name (ending in `dir/`, `dir/.`, `..`) is an `InvalidInput` error
- The rest (the fork's side effects, the directory being looked up first, D or R left behind if the process dies midway) is in [docs/MANUAL.md](./docs/MANUAL.md#what-differs-from-std-for-a-long-path)

## Removing the socket file

`bind_ex` returns the listener and a `SocketFile`. `SocketFile::discard()` removes the socket file taken right after the bind, only while what is at its path has the same `(st_dev, st_ino)` (another socket bound at the path since is left alone). The listener stays std's type and may go to tokio etc.; closing it is up to whoever holds it.

```rust
let (listener, file) = sockat::bind_ex(&path)?;
// ...
drop(listener);
file.discard()?;
```

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
