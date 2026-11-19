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
- `connect` too tries every route before fork-direct, so in an ordinary environment it reaches long paths as well
- To remove the socket file later, use `sockat::bind_ex`. It returns the listener and a `SocketFile` that removes only the file the bind made

## Why

std's `bind` / `connect` refuse a path that does not fit `sun_path`. The file system has no such limit, but deep temporary directories hit it quickly (macOS's `$TMPDIR` alone is about 50 bytes, and test harnesses and sandboxes nest further). The limit applies to the argument passed to `bind(2)` / `connect(2)`, so sockat hands the kernel a short spelling that names the same place.

## How a long path is reached

A path that std accepts (up to `sun_path` − 1 bytes) is handed to std unchanged.

| OS | Handed to std |
|---|---|
| Linux / Android / illumos | up to 107 bytes |
| macOS / iOS / FreeBSD / NetBSD / OpenBSD | up to 103 bytes |

A longer path is bound through **the routes through D**. sockat makes a directory D of its own (`sockat.XXXXXXXXXX`, mode 0700) and holds it by an fd, makes in it a symlink R with a random name (pointing to the socket's directory), binds with `R/<file name>` relative to D's fd, and removes R and D. The spelling the kernel keeps is `R/<file name>`; R is gone by then, so wherever it is resolved later, it hits nothing. The routes' names are those of the [sockat specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md) ("route names").

| OS | bind (in this order) |
|---|---|
| Linux / Android | thread-cwd → proc → fork |
| macOS / iOS and the other Apple OSes | thread-cwd → fork |
| FreeBSD | at |
| NetBSD / OpenBSD / illumos / other Unix | fork |
| Windows | not supported (compile error) |

- **thread-cwd**: move the cwd of a throwaway thread, and of it alone, into D (Linux: `unshare(CLONE_FS)` and `fchdir`; Apple OSes: `pthread_fchdir_np`). It has no side effects, so every function tries it. Not used on Linux under a seccomp filter
- **proc**: pass `/proc/self/fd/<D's fd>/R/<file name>`
- **at**: pass D's fd to `bindat` / `connectat`
- **fork**: fork a child that `fchdir`s into D and binds. The caller listens afterwards, so it has no side effect visible to peers
- D goes in `TMPDIR`, `/tmp` or the socket's directory (`.sockat.XXXXXXXXXX`), the first where it can be made. A place needs to be neither safe nor short (D is held by its fd). Where D can be made nowhere, a bind makes R in the socket's directory and tries the same order (thread-cwd-sockdir → proc-sockdir → fork-sockdir; at-sockdir on FreeBSD)

A connect tries the routes that need neither writing (making D, R or a symlink) nor a thread first, and the routes through D (thread-cwd, proc, at, and proc-held, through a symlink in D) and the symlink in a place after them. When an earlier route reaches the socket, neither D nor a symlink is made. The kernel keeps no spelling of a connect, so apart from the routes through D these carry no random part. D is made at most once in a call and shared by the routes that use it.

| OS | connect (in this order) |
|---|---|
| Linux / Android (`/proc` usable) | proc-direct → thread-cwd → proc → proc-held → symlink |
| Linux / Android (no `/proc`) | relative → thread-cwd → symlink |
| macOS / iOS and the other Apple OSes | relative → widened → thread-cwd → symlink |
| FreeBSD | at-direct → at |
| NetBSD / OpenBSD / illumos / other Unix | relative → symlink |

- **proc-direct**: pass `/proc/self/fd/<directory fd>/<file name>`
- **relative**: when std accepts the path relative to the cwd, pass that. Only the path string is rewritten; the cwd is never changed
- **widened**: pass a sockaddr carrying up to 253 bytes, which the macOS kernel accepts (the address of a socket bound through it is cut short and breaks its readers, so a bind never uses it)
- **at-direct**: pass the socket's directory's fd and the file name to `connectat`
- **proc-held**: make a symlink to the socket file in D and pass `/proc/self/fd/<D's fd>/s`
- **symlink**: create a short symlink to the socket file under `TMPDIR` or `/tmp`, in a place where no other user can replace it, go through it, and remove it at once

The `connect_any` variants fork last when none of these reaches the path (fork-direct). None of the routes changes the cwd of the calling process or thread, so they can be used from multi-threaded programs.

`connect` / `connect_nonblocking` never fork. Where only the fork route would reach the path, they return `ENAMETOOLONG`, as a plain connect to a path too long for `sun_path` does. That happens only when the routes through D cannot be used for a connect (NetBSD / OpenBSD / illumos, Linux under a seccomp filter without `/proc`, an Apple OS without `pthread_fchdir_np`) and no route without D reaches the path either. With a usable `/tmp`, `connect` reaches the path whatever the directory depth and the file name length. The length limits of each route, and where reaching stops, are in [docs/MANUAL.md](./docs/MANUAL.md#how-a-long-path-is-reached).

## The exception: the any functions

`bind_any` / `connect_any` / `connect_any_nonblocking` are only for the rare environment where no default route can be used, when the path must be reached at the cost of something.

- `bind_any`: after the order of `bind`, binds last through a symlink to the socket's directory in a place (`XDG_RUNTIME_DIR`, `TMPDIR`, `/tmp`). In a shared place (`/tmp`, ...) another user can make the name in the address the kernel reports again
- `connect_any` / `connect_any_nonblocking`: after the order of `connect`, have a forked child make the connect last (the child exits at once). The peer pid the server sees is then that of the exited child
- The results (`AnyListener` / `AnyConnection`) hold std's listener / stream, what was given up (`losses`: `respelled` / `forked` / `recreatable-spelling` / `cwd-relative`), the name of the route taken (`route`) and the routes tried and passed over (`skipped`, names and reasons). On failure an `AnyError` holds `skipped` too and converts back into the `io::Error` by `?` (the errno kept). `route` and `skipped` are for diagnostics and promise nothing about their content

They are off by default and available only with the feature `any`: `sockat = { version = "0.4", features = ["any"] }`. Details in [docs/MANUAL.md](./docs/MANUAL.md#the-exception-the-any-functions).

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
