# sockat reference

> English | [日本語](./REFERENCE-ja.md)

The API documentation of each function and type is on [docs.rs](https://docs.rs/sockat). This page lists the functions and types of sockat-rs (the crate `sockat`), their forms in Rust, and the caveats specific to Rust. How to use them is in the [user manual](./MANUAL.md).

## Specification

The language-independent behaviour is defined by [the sockat specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md); sockat-rs is its Rust implementation. What was decided for Rust is in [DR-0032](./decisions/DR-0032-resolve-through-persistent-base.md) (in Japanese), and all the decisions are listed in [the index of design decisions (DR)](./decisions/INDEX.md) (in Japanese).

What holds regardless of the language is in the specification and the pages:

- [What differs from the standard for a long path](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-differs-from-the-standard-for-a-long-path)
- [Patterns not to use](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#patterns-not-to-use)
- [What stays in the base](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-stays-in-the-base)
- [`sweep`](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#sweep)
- [Routes for long paths and their order](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#routes-for-long-paths-and-their-order), [building the routes](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes.md), [the reach table](https://github.com/kawaz/sockat/blob/main/pages/reach/README.md)
- [Errors](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#errors)
- [Per-language caveats](https://github.com/kawaz/sockat/blob/main/pages/langs.md)

## Functions

Use sockat's functions where you would use std's. They take a path of any length and return std's types.

| Function | std counterpart | Returns |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listening) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |
| `sockat::connect_nonblocking(path)` | (none) | `io::Result<UnixStream>` (nonblocking) |
| `sockat::datagram::bind(path)` | `UnixDatagram::bind(path)` | `io::Result<UnixDatagram>` |
| `sockat::datagram::unbound()` | `UnixDatagram::unbound()` | `io::Result<UnixDatagram>` |
| `sockat::datagram::connect(&sock, path)` | `sock.connect(path)` | `io::Result<()>` |

When you need clean-up and diagnostics, use the `_ex` of each ([The _ex functions: clean-up and diagnostics](#the-_ex-functions-clean-up-and-diagnostics)). To hand a spelling to std or another library yourself, use resolve ([resolve](#resolve)).

- An absolute path of a length std accepts goes to std's function unchanged. Nothing is added, and no environment variable is read (`connect_nonblocking`, which std does not have, connects sockat's own socket with the address std would use)
- The binds (`bind`, `bind_ex`, `datagram::bind`, `datagram::bind_ex`) and resolve make a relative path absolute first. The connects (`connect`, `connect_nonblocking`, `datagram::connect` and each `_ex`) hand a relative path to std as it is, as before. The steps are in the specification's [Making a relative path absolute](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#making-a-relative-path-absolute)
- The socket file is created at the path given. It stays when the listener is closed (as with std)
- sockat's own sockets are set up as std's are (close-on-exec; `SO_NOSIGPIPE` on the OSes where std sets it (macOS / iOS, FreeBSD, NetBSD, DragonFly); the same listen backlog as std)

### connect and connect_nonblocking

- **`connect`** replaces std's `UnixStream::connect`. It blocks
- **`connect_nonblocking`** is for async runtimes. The socket is made nonblocking before the connect and returned as it is, ready for tokio's `UnixStream::from_std`. When the peer's backlog is full it fails at once (`EAGAIN` on Linux, `ECONNREFUSED` on macOS). A Unix-domain connect completes or fails within the call, and sockat's clean-up afterwards is only an unlink, so there is nothing to wait for

## How a long path is reached

Rust's std accepts paths of up to `sun_path` − 1 bytes.

| OS | Handed to std up to |
|---|---|
| Linux / Android / illumos | 107 bytes |
| macOS / iOS and other Apple OSes / FreeBSD / NetBSD / OpenBSD | 103 bytes |

A longer path is handed to the kernel by a short spelling that names the same place as the original path. B, D and E, the routes and the order tried are in the specification's [Routes for long paths and their order](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#routes-for-long-paths-and-their-order) and [building the routes](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes.md); how far they reach is in the [reach table](https://github.com/kawaz/sockat/blob/main/pages/reach/README.md). sockat-rs makes every route of the specification and tries them on each OS as the specification's [Bind order](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#bind-order) and [Connect order](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#connect-order) say.

- Even a long absolute path goes to std by its real path (the symlinks on its way resolved) when that fits std, with nothing made in the bases (`Spelling` is `Original`). `Options::base` is not looked at then
- When the application decides where B is, pass [`Options::base`](#options)
- The route taken and the routes passed over are told by the `_ex` functions' [diagnostics](#diagnostics)

### Paths containing a NUL

A path with a NUL anywhere but at the start is `InvalidInput` at any length, with nothing created. On Linux / Android a path starting with a NUL (the abstract namespace) goes to std as it is at any length (std returns `InvalidInput`; abstract names are handled with std's `SocketAddr::from_abstract_name` and `bind_addr` / `connect_addr`).

### The empty path

An empty path is `InvalidInput` from every function, with nothing created and nothing handed to std (`ExError`'s `route` is `None`, `skipped` empty). Linux's std autobinds a bind with an empty path to an unnamed abstract address (macOS answers `EINVAL`), which is not a socket at the path given.

## Spelling properties: Spelling

`bind_ex`, `resolve_for_bind` and `resolve_for_connect` tell by `Spelling` whether the spelling can be used later. Branch on it. What the values and the fields mean is in the specification's [Spelling properties: `Spelling`](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#spelling-properties-spelling).

| Value | Spelling | Route |
|---|---|---|
| `Original` | the original absolute path, a relative path made absolute, or a long path's real path | standard |
| `CwdRelative` | the original relative path. Names another place after the cwd changes. Only when, the relative path made absolute, no route can be used (step 5 of [the specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#making-a-relative-path-absolute)) | standard |
| `Linked { .. }` | goes through B, D and E and names the same place as the original path | link-dir, link-file |
| `Dangling` | contains an E already cleaned up. Names nothing | thread-cwd, at |

The fields of `Linked`:

| Field | Type | Content |
|---|---|---|
| `removable` | `bool` | whether unlinking by the spelling removes the real file. `true` for link-dir, `false` for link-file (E is removed) |
| `until` | `Until` | `Until::Process` (procB: cannot be followed after the process ends) or `Until::Cleanup` (B's real path: until E is cleaned up) |
| `usable_cross_uid` | `bool` | whether a process of another uid can follow it |
| `usable_with_private_tmp` | `bool` | whether the peer can follow it even when this process or the peer has its `/tmp` isolated (systemd's `PrivateTmp`, ...) |

`ForConnect`'s `spelling` is `Original`, `CwdRelative` or `Linked`. `Linked` is link-file's spelling, with `removable` false; a procB spelling has `until` `Until::Process`.

`Spelling` and `Spelling::Linked` are `#[non_exhaustive]`. A match needs a `_` arm for values it does not know, and `..` in `Linked { removable, .. }`. Read a value you do not know as "do not rely on the spelling".

## Options

The `_ex` functions and resolve take `sockat::Options`. `Options::default()` is what the plain functions do. What each field promises is in the specification's [`Options`](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#options). How to use it is in the user manual's [Choose the base yourself](./manual/bind.md#choose-the-base-yourself).

| Field | Content |
|---|---|
| `usable_cross_uid: bool` | make a bind's spelling one another uid can follow too. When that cannot be met, the bind goes on to thread-cwd / at (`Dangling`). Only the binds look at it; the connects do not |
| `base: Option<PathBuf>` | where B itself is (for example `/tmp/my-app-1000/sockat`). Made when missing (mode 0711); when the checks fail, an error ([Errors](#errors)), never falling back to the automatic bases. Used by both the binds and the connects. Not looked at when a relative path made absolute or a long path's real path fits std. This B is sockat's own: put nothing else in it. The dead D in it go by [`sweep_base`](#sweep-and-sweep_base) |

## The _ex functions: clean-up and diagnostics

| Function | Returns |
|---|---|
| `sockat::bind_ex(path, options)` | `Result<Bound<UnixListener>, ExError>` |
| `sockat::connect_ex(path, options)` | `Result<Connected<UnixStream>, ExError>` |
| `sockat::connect_nonblocking_ex(path, options)` | the same (nonblocking) |
| `sockat::datagram::bind_ex(path, options)` | `Result<Bound<UnixDatagram>, ExError>` |
| `sockat::datagram::connect_ex(&sock, path, options)` | `Result<Report, ExError>` |

| Type | Content |
|---|---|
| `Bound<S>` | `socket` (std's type), `file: SocketFile`, `spelling: Spelling`, `route`, `base`, `skipped` |
| `Connected<S>` | `socket`, `route`, `skipped` |
| `Report` | `route`, `skipped` |
| `ExError` | `error: io::Error`, `route` (the route that actually bound / connected and failed; `None` when there is none), `skipped`. Converts back into `io::Error` by `?` |

`bind_ex`'s listener does not remove the socket file when it is closed, even for a short path. Only `SocketFile::remove` removes it.

### Diagnostics

| Field | Content |
|---|---|
| `route` | the name of the route taken |
| `base` | the B the bind's spelling goes through (`proc`, `tmp`, `runtime`, `tmpdir`, `sockdir`, `given`). `given` is the B of `Options::base`. None for a spelling that goes through no B (standard, thread-cwd, at) and for a connect |
| `skipped` | the routes tried and found unusable (`Skipped { name, reason }`), in the order tried, one per route name. For a long path `standard` comes first |

The names in `route`, `base` and `skipped`, and `reason`, are for diagnostics and promise nothing about their content (routes are added and changed). Branch on `Spelling`. How to use them is in the user manual's [See how it was reached](./manual/connect.md#see-how-it-was-reached).

## Clean-up

What stays in the bases and when it goes (B, D and E, the OS's cleaning of temporary directories, a live D being removed) is in the specification's [What stays in the base](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-stays-in-the-base). In Rust you clean up with the following.

### SocketFile

`bind_ex` returns a `SocketFile` holding the real path and `(st_dev, st_ino)` of the socket file bound, taken right after the bind. What it remembers, and when it removes and when it does not, is in the specification's [`remove`](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#remove). How to use it is in the user manual's [Remove the socket file when done](./manual/bind.md#remove-the-socket-file-when-done).

| Method | What it does |
|---|---|
| `remove(&self) -> io::Result<()>` | removes the socket file, the first time only, and does `cleanup` too. When it has been replaced or is already gone, removes nothing and returns `Ok(())`. On failure it still goes on to `cleanup`, and returns the error about the file first, else the error of `cleanup` |
| `cleanup(&self) -> io::Result<()>` | removes the E the spelling goes through, the first time only. The socket file and the socket stay |

- Both do nothing once they have succeeded. A call after a failure tries again. Called from several threads at once, they remove one at a time (also between a `ForBind` and the `SocketFile`s got by calling its `socket_file()` more than once)
- After the bind, `remove()` removes the file bound even when the cwd has changed (the `SocketFile` of `bind_ex` and that of `ForBind::socket_file()` alike). The cwd is checked (that it is the same before and after the bind) only when `Spelling` is `CwdRelative`
- Dropping does nothing
- `remove` does not close the listener. Call `remove` before closing the listener, then close it

### sweep and sweep_base

| Function | What it does |
|---|---|
| `sockat::sweep() -> io::Result<()>` | removes the D whose owning process is dead in the default-named B of bases 1 to 3 (`/tmp/sockat-<uid>`, `$XDG_RUNTIME_DIR/sockat`, `$TMPDIR/sockat-<uid>`) |
| `sockat::sweep_base(path) -> io::Result<()>` | does the same for a B passed as `Options::base`. The application that manages that B calls it |

What they remove and what they leave is in the specification's [`sweep`](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#sweep). `sweep_base` returns `InvalidInput` for a path of the wrong form, `EPERM` for a symlink, `ENOTDIR` for something not a directory, and `EPERM` for a B owned by someone else, writable by group or others, or reached through a directory others can write to. How to use them is in the user manual's [Clear what was left in the bases](./manual/bind.md#clear-what-was-left-in-the-bases) and [Choose the base yourself](./manual/bind.md#choose-the-base-yourself).

## resolve

For those who hand a spelling to std or another library themselves. A spelling is just a path, so it works whatever the socket type and the operation. What it promises is in the specification's [resolve](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#resolve). How to use it is in the user manual's [Resolve only the path](./manual/resolve.md).

| Function | Spelling | Returns |
|---|---|---|
| `sockat::resolve_for_bind(path, options)` | link-dir's `B/D/E/<file name>`, and on macOS and NetBSD also link-file's `B/D/E` | `Result<ForBind, ExError>` (`path`, `spelling`, `route`, `base`, `skipped`, `cleanup()`, `socket_file()`) |
| `sockat::resolve_for_connect(path, options)` | link-file's `B/D/E` (any file name length) | `Result<ForConnect, ExError>` (`path`, `spelling`, `route`, `skipped`, `cleanup()`) |

- An absolute path of a length std accepts is returned as it is (`Original`)
- A relative path comes back as an absolute spelling (the path made absolute, or a long path's real path, when that fits std: `Original`, with nothing made in the bases; otherwise the spelling through B, D and E above). Only when no spelling can be had does a relative path of a length std accepts come back as it is (`CwdRelative`)
- A long absolute path whose real path fits std comes back as that real path (`Original`, with nothing made in the bases)
- When no spelling fits (a length only thread-cwd / at reach, or B cannot be made), `ENAMETOOLONG`. To reach such a length, use `bind` / `connect`
- Call `ForBind::socket_file(&self) -> io::Result<SocketFile>` after the bind and before the listener is closed. The `remove` of the `SocketFile` it returns removes the file and cleans up the same E as the `ForBind`. Without calling it, there is nothing that removes the file ([`SocketFile` of `ForBind` in the specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#socketfile-of-forbind))
- `ForConnect`'s spelling can be used for any number of connects; call `cleanup()` at the end
- `resolve_for_connect` does not look at `usable_cross_uid`

## datagram

`sockat::datagram` mirrors the functions of std's `UnixDatagram` that take a path, and `unbound`. The routes and their order are those of stream. To a sender bound to a long path, the receiver can reply to the address from `recv_from` while the spelling can be followed (`Spelling::Linked`). On OpenBSD this needs Rust 1.100 or later ([Caveats specific to Rust](#caveats-specific-to-rust)). How to use it is in the user manual's [Use DGRAM](./manual/datagram.md).

`send_to` and SEQPACKET are not there yet ([the specification's open questions](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#open-questions)). To `send_to` a long path, hand the spelling of `resolve_for_connect` to std's `send_to` ([send_to a long path](./manual/datagram.md#send_to-a-long-path)).

## What differs from std for a long path

What differs regardless of the language (the address the kernel reports, how operational tools show it, the directory being resolved first, state growing outside the application's own place, environments where a live D can be removed, a peer with `PrivateTmp`) is in the specification's [What differs from the standard for a long path](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-differs-from-the-standard-for-a-long-path). In Rust:

- After a bind to a relative path, even a short one, the address from `local_addr()`, `peer_addr()` and `recv_from()` is an absolute path, not the relative path given. In a deep cwd, even a short relative path makes B, D and E in the bases (the rows "Relative path:" of the specification's [What differs from the standard for a long path](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#what-differs-from-the-standard-for-a-long-path))
- The address the kernel reports is the address from `local_addr()` of the listener and of accepted streams, `peer_addr()` of the connecting side, and a datagram's `recv_from()`. For a long path it is the bind's spelling, not the original path. Whether it can be used later is told by `Spelling`
- When a listener is closed, neither the listener of `sockat::bind` nor that of `bind_ex` removes the socket file, as with std's `UnixListener`. Remove it by the original path or by `SocketFile::remove()`
- relative returns an `Other` error when the cwd changed in between and stays changed
- On OpenBSD, std in Rust 1.99 and earlier drops the last byte of the address ([Caveats specific to Rust](#caveats-specific-to-rust))

## Patterns not to use

The list and why each is wrong are in the specification's [Patterns not to use](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#patterns-not-to-use). The forms in Rust and what to do instead:

| Pattern | In Rust | Instead |
|---|---|---|
| removing or `stat`ing the socket file by the spelling the kernel reports, without looking at `Spelling` | `std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())` | `std::fs::remove_file(path)` with the original path<br>`SocketFile::remove()` |
| using an old spelling after its clean-up or after the process ended | connecting by the spelling after `cleanup()` | use a spelling only within its `until` |
| binding with a relative path and removing by the relative string given after the cwd changes | `sockat::bind("app.sock")` followed by<br>`std::env::set_current_dir(..)` and<br>`std::fs::remove_file("app.sock")` | `SocketFile::remove()`<br>to make the path absolute yourself, `std::path::absolute` (it does not fold `..`) |
| relying on closing the listener to remove the socket file | `drop(listener)` alone | `SocketFile::remove()`<br>remove by the original path |
| relying on `remove()` to close the listener too | `bound.file.remove()` alone | close the listener yourself |
| calling `remove()` after the listener is closed | `drop(bound.socket); bound.file.remove()` | call it before closing |
| having another uid follow a procB spelling or one whose `usable_cross_uid` is false | handing the spelling of a `bind_ex` with `Options::default()` to a process of another uid | pass `Options { usable_cross_uid: true, .. }` and check `Spelling` |
| branching on the strings of `route`, `base`, `skipped` or `reason` | `if bound.route == "link-file"` | branch on `Spelling` |

What to do instead (run as a test in the crate documentation):

```rust
use std::io;
use std::path::Path;

fn serve(path: &Path) -> io::Result<()> {
    let path = std::path::absolute(path)?; // absolute before the bind
    let bound = sockat::bind_ex(&path, Default::default())?;
    if let sockat::Spelling::Linked { removable: false, .. } = bound.spelling {
        // removing by local_addr()'s spelling removes only E: remove by the original path or remove()
    }
    // ... accept on bound.socket ...
    bound.file.remove()?; // before the listener is closed
    drop(bound.socket);
    Ok(())
}
```

## Errors

The error for each situation is in the specification's [Errors](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#errors). The types in Rust:

- The plain functions return `io::Error`; the `_ex` functions and resolve return `ExError` (`error: io::Error`, `route`, `skipped`; converts back into `io::Error` by `?`)
- OS errors are not rewritten; `io::Error::raw_os_error()` gives the errno
- The specification's "argument error" is `io::ErrorKind::InvalidInput`, and its "general error" is `io::ErrorKind::Other`

Errors specific to the Rust calls:

| Situation | Error |
|---|---|
| For a long path, the real path of the socket's directory cannot be resolved | that error |
| `Options::base` passes its check, but B cannot be made | the error of making it |
| The cwd changed during a `bind_ex` that binds `CwdRelative` | `Other` |
| Right after `bind_ex`'s bind, what is there (where a symlink leads, when the last component is one) is not a socket. What `socket_file()` reaches is not a socket | `ENOTSOCK` |
| Right after `bind_ex`'s bind, the real path of where to remember (the specification's [`remove`](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#remove)) cannot be resolved (the file was removed, or on a route that looks the original path up again a symlink on the way was switched to where nothing is) | the error of resolving the real path (`ENOENT` etc.) |
| `socket_file()` is called before the bind or after the clean-up | the error of following the spelling |
| `cleanup` / `remove` cannot remove E (other than it being gone already) | the error of `unlink` |

`route` and `skipped` of `ExError`:

- The directory is looked up first, so `ENOENT` and `ENOTDIR` are settled before any route is tried (`route` is `None`, `skipped` empty)
- A bind / resolve of a relative path whose socket's directory is missing is `ENOENT` even when short, with `route` `None`
- `Options::base` is checked before the routes; when it fails, `route` is `None` and `skipped` empty. When B cannot be made after the check passed, `route` is `None` and `skipped` holds the routes passed over until then
- The route that actually bound / connected and failed goes in `route`

## Async runtimes

The results are std types, so any runtime that adopts std sockets can take them. The tokio examples are in the user manual's [Listen with tokio](./manual/bind.md#listen-with-tokio) and [Connect with tokio](./manual/connect.md#connect-with-tokio).

`connect_nonblocking` does not wait for the peer, and its clean-up is only an unlink, so it can be called on the runtime's worker threads directly. `bind` does not wait for a peer either (thread-cwd waits for its thread to end, but the thread binds and ends at once). `bind_ex`'s listener goes to the runtime the same way, and its `SocketFile` stays with the caller.

## Caveats specific to Rust

- **On OpenBSD, std in Rust 1.99 and earlier drops the last byte of an address.** The path from `local_addr()`, `peer_addr()` and `recv_from()` lacks its last byte (a bug in std, the same for a short path and without sockat; [rust-lang/rust#118925](https://github.com/rust-lang/rust/issues/118925), fixed in Rust 1.100.0). The address the kernel reports is whole. Replying to a datagram at the address from std's `recv_from` needs Rust 1.100 or later
- **Linux's std autobinds a bind with an empty path to an abstract address.** sockat does not hand an empty path to std and makes it `InvalidInput` ([The empty path](#the-empty-path))
- **std does not take an abstract name (a leading NUL) given as a path.** `bind` and `bind_ex` hand it to std and return std's `InvalidInput`. Bind an abstract name with std's `SocketAddr::from_abstract_name` and `bind_addr`. `resolve_for_bind` hands an abstract name back as it is, and its `socket_file()` gives a `SocketFile` with no file (its `remove()` removes no file; the spec's [`remove`](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#remove))

- **With a cwd deeper than PATH_MAX, `bind_ex` of a relative path fails.** `ERANGE` on macOS, `ENAMETOOLONG` on Linux (`route` is `standard`, the listener closed): the socket file cannot be remembered by its absolute real path. The plain `bind` goes through (to std as it is, `CwdRelative`)

## Not checked

- Android has not even been compiled. It uses the same routes as Linux. Android apps run under a seccomp filter, so the thread cwd is not expected to be used
- NetBSD, OpenBSD, illumos and FreeBSD run the tests in CI VMs. The peer credentials have not been checked there
- The error a nonblocking connect returns on a full backlog, and that a connect completes within the call, have been checked on Linux and macOS only
- Apple OSes other than macOS (iOS, ...) use the same routes as macOS, but only iOS's clippy has been run. Whether they have `pthread_fchdir_np`, and whether a bind follows a symlink there, have not been checked either
- Whether link-file can be used for a connect on FreeBSD (it assumes a connect follows a symlink in the last component)
