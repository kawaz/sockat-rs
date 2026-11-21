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
- A bind at a long path leaves small directories and symlinks outside the app's own directories (`/tmp/sockat-<uid>` etc.). What stays and how it goes are in the specification's [What stays in the base](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-stays-in-the-base) and [`sweep`](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#sweep); how to call it from Rust (`sockat::sweep()`, `sockat::sweep_base(path)`) is in the [manual](docs/MANUAL.md#sweep-and-sweep_base)
- To remove the socket file later, or to know whether the spelling can be used later and how the path was reached, use the `_ex` functions (`bind_ex`, ...). To hand a spelling to std yourself, use `resolve_for_bind` / `resolve_for_connect`

## How a long path is reached

A long path is handed to the kernel by a short spelling that names the same place as the original path. B, D and E, the routes and the order tried on each OS are in the specification's [Routes for long paths and their order](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#routes-for-long-paths-and-their-order); how far they reach is in the [reach table](https://github.com/kawaz/sockat/blob/main/pages/reach/README.md). sockat-rs makes every route of the specification and tries them in the specification's order on every OS.

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

What differs regardless of the language, and the patterns not to use, are in the specification's [What differs from the standard for a long path](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-differs-from-the-standard-for-a-long-path) and [Patterns not to use](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#patterns-not-to-use). In Rust:

- For a long path, the address from `local_addr()`, `peer_addr()` and `recv_from()` is the bind's spelling, not the original path. Remove the socket file by the original path or by `SocketFile::remove()`
- On OpenBSD, std in Rust 1.99 and earlier drops the last byte of those addresses. Replying to a datagram at the address from `recv_from` needs Rust 1.100 or later

The forms in Rust and the recommended code are in the manual's ["What differs from std for a long path"](docs/MANUAL.md#what-differs-from-std-for-a-long-path), ["Patterns not to use"](docs/MANUAL.md#patterns-not-to-use) and ["Caveats specific to Rust"](docs/MANUAL.md#caveats-specific-to-rust).

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

- [Manual](docs/MANUAL.md): the functions and types in Rust, the clean-up calls, the patterns not to use in Rust, the error types, the caveats specific to Rust
- [sockat specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md): the language-independent behaviour
- [Per-language caveats](https://github.com/kawaz/sockat/blob/main/pages/langs.md): the caveats specific to each language's implementation
- [Design decisions (DR)](docs/decisions/INDEX.md), in Japanese

## License

MIT License, Yoshiaki Kawazu (@kawaz)
