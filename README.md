# sockat

> English | [日本語](./README-ja.md)

A Rust library that binds and connects Unix domain sockets even when the path is longer than `sun_path` allows (104 bytes on macOS and the BSDs, 108 on Linux, NUL included).

The language-independent behaviour is defined by the [sockat specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md); sockat-rs is its Rust implementation.

```rust
let listener = sockat::bind("/a/very/long/directory/.../server.sock")?;
let stream = sockat::connect("/a/very/long/directory/.../server.sock")?;
```

## Usage

Use sockat's functions where you would use std's.

| sockat | std it replaces |
|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` |
| `sockat::connect(path)` | `UnixStream::connect(path)` |
| `sockat::connect_nonblocking(path)` | (none; returns a nonblocking stream without waiting) |
| `sockat::datagram::bind(path)`, `unbound()`, `connect(&sock, path)` | the functions of `UnixDatagram` with the same names |

- The results are std's own types
- A path of a length std accepts goes to std unchanged, so it behaves exactly as with std
- Even for a long path, the socket file is created at the path given
- Nothing forks, and neither the cwd of the process nor that of the calling thread is changed
- To remove the socket file later, or to know whether the spelling can be used later and how the path was reached, use the `_ex` functions (`bind_ex`, ...). To hand a spelling to std yourself, use `resolve_for_bind` / `resolve_for_connect`

## Why

std's `bind` / `connect` refuse a path that does not fit `sun_path`. The file system has no such limit, but deep temporary directories hit it quickly (macOS's `$TMPDIR` alone is about 50 bytes, and test harnesses and sandboxes nest further). The limit applies to the argument passed to `bind(2)` / `connect(2)`, so sockat hands the kernel a short spelling that names the same place.

## How a long path is reached

The kernel keeps a bind's spelling for the life of the socket and reports it by `getsockname`. Libraries above remove the socket file by that spelling, and a DGRAM receiver replies to it. So a bind's spelling must keep naming the same place when it is followed later.

- **B**: this user's directory in a base. `/tmp/sockat-<uid>` (failing that `$XDG_RUNTIME_DIR/sockat`, `$TMPDIR/sockat-<uid>`, the socket's directory). Mode 0711, reused, never removed by sockat
- **D**: a directory per process in B. The process keeps a `flock` on it; a dead process's D is removed by the OS's cleaning of temporary directories or by `sockat::sweep()`
- **E**: a symlink in D. It points to the socket's directory (link-dir, spelling `B/D/E/<file name>`) or to the socket file (link-file, spelling `B/D/E`; macOS and NetBSD, where a bind follows a symlink)

B is spelled `/proc/<pid>/fd/<B's fd>` (Linux) or by its real path. A bind that still does not fit is made by a sockat thread that alone moves its cwd into D (Linux, Apple OSes), and on FreeBSD by `bindat(2)`. A connect's spelling stays nowhere on the peer, so a connect tries first the routes that need neither writing nor a thread (`/proc/self/fd`, a path relative to the cwd, macOS's widened sockaddr, FreeBSD's `connectat(2)`), then the thread cwd, then a symlink made directly in B and removed at once, and leaves nothing.

| OS | bind | connect |
|---|---|---|
| Linux, Android | link-dir, thread-cwd | proc-direct (relative without `/proc`), thread-cwd-direct, link-file |
| macOS, iOS (Apple) | link-dir, link-file, thread-cwd | relative, widened, thread-cwd-direct, link-file |
| FreeBSD | link-dir, at | at-direct, link-file |
| NetBSD | link-dir, link-file | relative, link-file |
| OpenBSD, illumos | link-dir | relative, link-file |

The routes and how far they reach are in the [manual](docs/MANUAL.md#how-a-long-path-is-reached).

## The _ex functions: clean-up and diagnostics

```rust
let bound = sockat::bind_ex(path, Default::default())?;
match bound.spelling {
    sockat::Spelling::Linked { removable, .. } => { /* the spelling can be followed. When removable is false, removing by the spelling removes only E */ }
    sockat::Spelling::Dangling => { /* the spelling names nothing */ }
    _ => { /* the original path */ }
}
// ... accept on bound.socket (it may go to tokio etc.) ...
bound.file.remove()?; // removes the socket file and E. Before the listener is closed
drop(bound.socket);
```

- `bind_ex`, `connect_ex`, `connect_nonblocking_ex`, `datagram::bind_ex` and `datagram::connect_ex` take `sockat::Options` (`usable_cross_uid`: ask for a spelling another uid can follow too; `base`: put B in the application's own place) and return the route taken, the B and the routes passed over
- `bind_ex` returns whether the spelling can be used later (`Spelling`) and a `SocketFile` that removes the socket file (`remove()`, `cleanup()`)
- A failure is an `ExError` (it keeps the errno and converts back into `io::Error` by `?`)

## What differs from std for a long path

- **The address the kernel reports is not the original path.** `local_addr()`, `peer_addr()`, `ss -x` and the like report the bind's spelling. Whether it can be used later is told by `Spelling`. Remove the socket file by the original path or by `SocketFile::remove()`
- **State grows outside the application's own place.** B and D stay in `/tmp` and the like. The default `bind`'s E and a dead process's D stay until the OS's cleaning of temporary directories or `sweep()`. A B in the socket's directory and a B made after the checks failed (`sockat-<uid>.<random>`) may be removed neither by `sweep()` nor by the OS's cleaning
- **Do not use a spelling after its clean-up.** Once B in `/tmp` is gone, someone else may make it again under the same name. A spelling through `/proc` cannot be followed after the process ends
- **There are environments where a live D can be removed.** Linux with systemd older than v242, and B in macOS's `$TMPDIR` (the spelling just cannot be followed any more)
- **A peer with `PrivateTmp`** cannot follow a spelling through `/tmp` (`Spelling`'s `usable_with_private_tmp`)

Details in the [manual](docs/MANUAL.md#what-differs-from-std-for-a-long-path) and [Patterns not to use](docs/MANUAL.md#patterns-not-to-use).

## Async runtimes

The results are std types, so they go to tokio and the like as they are.

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` does not wait (its clean-up is only an unlink) and returns the stream still nonblocking, so it can be called on the runtime's worker threads directly.

## Documentation

- [Manual](docs/MANUAL.md): usage, routes, clean-up, what differs from std, errors
- [sockat specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md): the language-independent behaviour
- [Design decisions (DR)](docs/decisions/INDEX.md), in Japanese

## License

MIT License, Yoshiaki Kawazu (@kawaz)
