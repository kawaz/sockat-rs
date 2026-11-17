# sockat

> English | [日本語](./README-ja.md)

A Rust library that binds and connects Unix domain sockets even when the path is longer than `sun_path` allows (104 bytes on macOS, 108 on Linux, NUL included).

```rust
let listener = sockat::bind("/a/very/deep/directory/.../server.sock")?;
let stream = sockat::connect("/a/very/deep/directory/.../server.sock")?;
```

The results are std's own `UnixListener` / `UnixStream`. A path of at most 100 bytes is handed to std unchanged, so short paths behave exactly as with std.

## Why

std's `UnixListener::bind` / `UnixStream::connect` refuse a path that does not fit `sun_path`. The file system has no such limit, but deep temporary directories hit it quickly (macOS's `$TMPDIR` alone is about 50 bytes, and test harnesses and sandboxes nest further). The limit applies to the argument passed to `bind(2)` / `connect(2)`, so sockat hands the kernel a different, short spelling.

## How a long path is reached

| Platform | Way |
|---|---|
| Linux / Android | open the directory `O_PATH` and pass `/proc/self/fd/<fd>/<name>` |
| macOS | in order: the path relative to the cwd → a widened sockaddr carrying up to 253 bytes → through a short symlink under `${TMPDIR:-/tmp}` → a forked child that `fchdir`s into the directory |
| FreeBSD | `bindat(2)` / `connectat(2)` |
| other Unix | a forked child that `fchdir`s into the directory |
| Windows | not supported |

None of these changes the calling process's cwd, and all of them work in multi-threaded programs. The socket file is always created at the path given.

## What differs from std for a long path

- `local_addr()` / `peer_addr()` do not return the path (they return the spelling actually handed to the kernel). Keep the path yourself
- On the fork route, the pid the server sees for the connecting side is briefly the child's
- Details in [docs/MANUAL.md](./docs/MANUAL.md)

## Async runtimes

The only dependency is libc; no runtime is required. With tokio, convert from the std types:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;
```

## Documentation

- [docs/MANUAL.md](./docs/MANUAL.md) — usage, errors, caveats
- [docs/decisions/](./docs/decisions/INDEX.md) — design decisions (DRs, in Japanese)

## License

MIT License, Yoshiaki Kawazu (@kawaz)
