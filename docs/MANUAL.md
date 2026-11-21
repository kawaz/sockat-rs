# sockat manual

> English | [日本語](./MANUAL-ja.md)

A reference for how to use sockat, how it reaches a long path, what it leaves and how to clean it up, and what differs from std for a long path. The language-independent behaviour is defined by [the sockat specification](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md); sockat-rs is its Rust implementation. What was decided for Rust is in [DR-0032](./decisions/DR-0032-resolve-through-persistent-base.md) (in Japanese).

## Usage

```toml
[dependencies]
sockat = "0.5"
```

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

```rust
use std::io::{Read, Write};

let path = "/a/very/long/directory/.../server.sock";
let listener = sockat::bind(path)?;
let mut client = sockat::connect(path)?;
let (mut server, _) = listener.accept()?;
client.write_all(b"ping")?;
let mut buf = [0; 4];
server.read_exact(&mut buf)?;
```

- A path of a length std accepts goes to std's function unchanged. Nothing is added, and no environment variable is read (`connect_nonblocking`, which std does not have, connects sockat's own socket with the address std would use)
- The socket file is created at the path given. It stays when the listener is closed (as with std)
- sockat's own sockets are set up as std's are (close-on-exec; `SO_NOSIGPIPE` on the OSes where std sets it (macOS / iOS, FreeBSD, NetBSD, DragonFly); the same listen backlog as std)
- Nothing forks. No route changes the cwd of the process or of the calling thread

## connect and connect_nonblocking

- **`connect`** replaces std's `UnixStream::connect`. It blocks
- **`connect_nonblocking`** is for async runtimes. The socket is made nonblocking before the connect and returned as it is, ready for tokio's `UnixStream::from_std`. When the peer's backlog is full it fails at once (`EAGAIN` on Linux, `ECONNREFUSED` on macOS). A Unix-domain connect completes or fails within the call, and sockat's clean-up afterwards is only an unlink, so there is nothing to wait for

A connect leaves nothing, neither in the process nor in the bases.

## How a long path is reached

A path of up to the length std accepts (`sun_path` − 1 bytes) goes to std unchanged ([standard](#standard)).

| OS | Handed to std up to |
|---|---|
| Linux / Android / illumos | 107 bytes |
| macOS / iOS and other Apple OSes / FreeBSD / NetBSD / OpenBSD | 103 bytes |

A longer path is handed to the kernel by a short spelling that names the same place as the original path. The kernel keeps a bind's spelling for the life of the socket and reports it by `getsockname` / `getpeername`. Libraries above remove the socket file by that spelling, and a DGRAM receiver replies to it. So a bind's spelling must keep naming the same place when it is followed later.

### B, D and E

| | Content | How many | Stays |
|---|---|---|---|
| B | this user's directory in a base (mode 0711). `/tmp/sockat-<uid>`, failing that `$XDG_RUNTIME_DIR/sockat`, `$TMPDIR/sockat-<uid>`, and last `.sockat.<10 random characters>` in the socket's directory | one per user, reused across processes | sockat never removes it |
| D | a directory in B with a name of 10 random characters (mode 0711) | one per process and B | the process keeps an fd with a shared lock (`flock`) on it. A dead process's D is removed by the OS's cleaning of temporary directories or by [`sweep`](#sweep-and-sweep_base) |
| E | a symlink in D: one to the socket's directory (link-dir) or one to the socket file (link-file) | one per bind and per resolve | until `cleanup` / `remove`. The default `bind`'s E is removed together with D after the process ends |

- The base for B is checked and spelled by its real path after realpath (B in macOS's `/tmp` is `/private/tmp/sockat-<uid>`)
- B is used only when every directory from the root to the base is "owned by root or the caller, and either writable by neither group nor others, or sticky", and B itself is "a real directory, owned by the caller, writable by neither group nor others". When B with the default name fails this, `sockat-<uid>.<10 random characters>` is made. When the place is not writable, the next place is tried
- A `$XDG_RUNTIME_DIR` or `$TMPDIR` that is empty or relative is not used as a base
- "The caller" and the `<uid>` in the names are the effective uid
- A process uses one B in bases 1 to 3 (one more only with `usable_cross_uid`). A B in the socket's directory is one per directory
- A B once held is used for the rest of the process. A base where B could not be made is tried again by the next call
- When a B or D held has been removed (its fd's `st_nlink` is 0, or what its name names is not the fd's), the next call makes it again
- When it is known beforehand that the spelling will not fit, no B is made. The next base is not tried either; the routes through B are passed over (the process's B does not change with the length of a path)
- When D cannot be made, the routes that use D are passed over in that call. No other B is looked for
- A B in the socket's directory is used only through the procB spelling
- With [`Options::base`](#options), B is at that place

B is spelled `/proc/<pid>/fd/<B's fd>` (procB, when `/proc` can be used) or by B's real path. A procB spelling can be followed only by processes of the same uid in the same PID namespace, and not after the process ends.

### Order per OS

The first route that can be used makes the bind / connect. When a route actually binds / connects and fails (`ENOENT`, `EADDRINUSE`, `ECONNREFUSED`, ...), that error is returned and the next is not tried.

bind (stream and DGRAM):

| OS | Order |
|---|---|
| Linux / Android | standard → link-dir (procB → B's real path) → thread-cwd |
| macOS / iOS and other Apple OSes | standard → link-dir → link-file → thread-cwd |
| FreeBSD | standard → link-dir → at |
| NetBSD | standard → link-dir → link-file |
| OpenBSD, illumos | standard → link-dir |
| Other Unix | standard → link-dir → link-file (whether a bind follows a symlink is told at run time) |

connect (stream and DGRAM):

| OS | Order |
|---|---|
| Linux / Android (`/proc` usable) | standard → proc-direct → thread-cwd-direct → link-file |
| Linux / Android (`/proc` not usable) | standard → relative → thread-cwd-direct → link-file |
| macOS / iOS and other Apple OSes | standard → relative → widened → thread-cwd-direct → link-file |
| FreeBSD | standard → at-direct → link-file |
| NetBSD, OpenBSD, illumos, other Unix | standard → relative → link-file |

### Routes

#### standard

The path goes to std as it is. The spelling is the original path (`Original` for an absolute path, `CwdRelative` for a relative one).

#### link-dir

An E pointing to the real path of the socket's directory is made in D, and the bind goes by `B/D/E/<file name>`. Unlinking by the spelling follows the E in the middle and removes the socket file.

#### link-file

An E pointing to the socket file (with the cwd put in front for a relative path) is made in D, and the bind goes by `B/D/E`. The spelling holds no file name, so any file name length is reached. Used only on the OSes where a bind follows a symlink in the last component (macOS, NetBSD). Unlinking by the spelling removes E, and the socket file stays.

A connect's link-file makes no D: it makes an E with a random name directly in B, connects by `B/E`, and removes E at once.

#### thread-cwd

A bind whose spelling does not fit (Linux / Android, Apple OSes). A thread started by sockat, and it alone, moves its cwd into D (`unshare(CLONE_FS)` and `fchdir` on Linux, `pthread_fchdir_np` on Apple OSes) and binds by `E/<file name>` through an E in D that is removed at once. The spelling contains the E already cleaned up and names nothing (`Dangling`). Not used on Linux when a seccomp filter is in effect (docker's default profile, ...), because a filter may refuse `unshare` by killing the process.

#### at

FreeBSD's stand-in for thread-cwd. No thread: D's fd is passed to `bindat(2)`.

#### proc-direct

Connects by `/proc/self/fd/<fd of the socket's directory>/<file name>` (Linux). File names of up to about 90 bytes.

#### relative

Connects by a path relative to the cwd. Only the path string is rewritten; the cwd is not changed. The cwd's spelling is cut off the front, and when that is not enough, a spelling climbing with `..` between the two resolved to real paths is made. If the cwd has changed after the connect, the result is discarded and an `Other` error returned.

#### widened

On Apple OSes, connects with a widened sockaddr that holds up to 253 bytes, carrying the original path (failing that, the relative path). Never used for a bind (the address of the socket made does not fit `sockaddr_un` and breaks its readers).

#### at-direct

On FreeBSD, passes the socket's directory's fd and the file name to `connectat(2)`.

#### thread-cwd-direct

A thread started by sockat, and it alone, moves its cwd into the socket's directory and connects by the file name (Linux / Android, Apple OSes). Makes neither D nor E.

### How far it reaches

The approximate upper limit of the file name length in the spelling (with B in `/tmp`).

| Route | Linux | macOS |
|---|---|---|
| link-dir (procB) | 74 to 77 | (none) |
| link-dir (B's real path) | 77 (`/tmp/sockat-1000/`) | 66 (`/private/tmp/sockat-501/`) |
| link-file | (none) | any length |
| thread-cwd | 96 | 92 |
| connect | any length (link-file) | any length (link-file; widened up to 253 bytes) |

A bind on Linux is `ENAMETOOLONG` when the file name is over about 96 bytes, or over about 77 bytes under a seccomp filter. A bind on OpenBSD and illumos has link-dir only. The whole path is limited to `PATH_MAX` (1024 on macOS, 4096 on Linux).

### Paths containing a NUL

A path with a NUL anywhere but at the start is `InvalidInput` at any length, with nothing created (the kernel reads only up to the NUL, so the socket would be made elsewhere). On Linux / Android a path starting with a NUL (the abstract namespace) goes to std as it is at any length (std returns `InvalidInput`; abstract names are handled with std's `SocketAddr::from_abstract_name` and `bind_addr` / `connect_addr`).

### The empty path

An empty path is `InvalidInput` from every function, with nothing created and nothing handed to std (`ExError`'s `route` is `None`, `skipped` empty). Linux's std autobinds a bind with an empty path to an unnamed abstract address (macOS answers `EINVAL`), which is not a socket at the path given.

## Spelling properties: Spelling

`bind_ex` and `resolve_for_bind` tell by `Spelling` whether a bind's spelling can be used later. Branch on it.

| Value | Spelling | Route |
|---|---|---|
| `Original` | the original absolute path | standard |
| `CwdRelative` | the original relative path. Names another place after the cwd changes (a hazard std has on its own, reported all the same) | standard |
| `Linked { .. }` | goes through B, D and E and names the same place as the original path | link-dir, link-file |
| `Dangling` | contains an E already cleaned up. Names nothing | thread-cwd, at |

The fields of `Linked`:

| Field | Content |
|---|---|
| `removable` | whether unlinking by the spelling removes the real file. `true` for link-dir, `false` for link-file (E is removed) |
| `until` | `Until::Process` (procB: cannot be followed after the process ends) or `Until::Cleanup` (B's real path: until E is cleaned up) |
| `usable_cross_uid` | whether a process of another uid can follow it. `false` for procB. For B's real path, `true` when every directory on the way and B itself give others x (B in `/tmp`) |
| `usable_with_private_tmp` | whether the peer can follow it even when this process or the peer has its `/tmp` isolated (systemd's `PrivateTmp`, ...). `true` for procB. For B's real path, `false` when B is under `/tmp` or `/var/tmp` |

## Options

The `_ex` functions and resolve take `sockat::Options`. `Options::default()` is what the plain functions do.

| Field | Content |
|---|---|
| `usable_cross_uid: bool` | make a bind's spelling one another uid can follow too. Only a B in a base where every directory on the way and B itself give others x is used, and procB is not. When that cannot be met, the bind goes on to thread-cwd / at (`Dangling`). Whether a B lets another uid follow it is judged when the process takes that B, and kept (also for the B of `Options::base`). But when B itself was judged not to give others x, B's mode is looked at again on each call with `usable_cross_uid` (when two processes make B for the first time at once, one may take B while it is still 0700, before the other sets it to 0711). The connects do not look at it |
| `base: Option<PathBuf>` | where B itself is (for example `/tmp/my-app-1000/sockat`). Made when missing (mode 0711). When the checks fail, `ENOENT` / `ENOTDIR` / `EPERM`, never falling back to the automatic bases. A relative path and a path that does not end with a name are `InvalidInput`, whatever the length of the socket path. A symlink at the base is `EPERM`. The check is made for a long path on every call, before any route is tried, and B is made only when a route that uses B is reached. Used by both the binds and the connects. This B is sockat's own: put nothing else in it (`sweep_base` looks only at directories named in the form of a D) |

```rust
let options = sockat::Options {
    base: Some("/tmp/my-app-1000/sockat".into()),
    ..Default::default()
};
let bound = sockat::bind_ex(path, options)?;
```

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

The names in `route`, `base` and `skipped`, and `reason`, are for diagnostics and promise nothing about their content (routes are added and changed). Branch on `Spelling`.

## Clean-up

### SocketFile

`bind_ex` returns a `SocketFile` holding the real path and `(st_dev, st_ino)` of the socket file bound, taken right after the bind.

| Method | What it does |
|---|---|
| `remove(&self)` | removes the socket file, the first time only, and does `cleanup` too. Removes it only when what is at that path has the `(st_dev, st_ino)` taken. When it has been replaced or is already gone, removes nothing and returns `Ok(())`. On failure it still goes on to `cleanup`, and returns the error about the file first, else the error of `cleanup` |
| `cleanup(&self)` | removes the E the spelling goes through, the first time only. The spelling then names nothing; the socket file and the socket stay. When E cannot be removed (other than being gone already), returns the error of `unlink` |

- Both do nothing once they have succeeded. A call after a failure tries again. Called from several threads at once, they remove one at a time (also between a `ForBind` and the `SocketFile`s got by calling its `socket_file()` more than once). Dropping does nothing
- When the last component of the path is a symlink (which the bind follows on macOS and NetBSD, making the socket where it points), what is remembered is the socket it led to. `remove` removes that socket and leaves the user's symlink
- `remove` does not close the listener. Closing it is up to the user
- The promise covers replacements while the listener is open. Once it is closed, a file removed and made again cannot be told apart on file systems that reuse inode numbers at once (ext4, the BSDs' UFS / FFS). Call `remove` before closing the listener
- When `bind_ex` cannot remember the socket file, it closes the listener, cleans up E, and returns an error without removing the socket file (removing a file it could not check might remove what replaced it). When what is there right after the bind (where a symlink leads, for a symlink) is not a socket, it was replaced: it is not remembered, and the error is `ENOTSOCK`
- The E removed inside a call (the E of a bind that failed or could not remember the file, the E of thread-cwd and at, the E after a connect) is passed over in silence when it cannot be removed: neither whether the call succeeds nor the original error changes. Only `cleanup` and `remove` called by the user return a failure to remove
- For a relative path, `bind_ex` compares the cwd's `(st_dev, st_ino)` before looking the path up and after the bind and the realpath taken to remember the file; when they differ, it returns an `Other` error as when the file cannot be remembered
- Someone who can write to the socket's directory and swaps the file between the check and the removal is not stopped (POSIX has no way to remove a file by its inode)

```rust
let bound = sockat::bind_ex(path, Default::default())?;
// ... accept on bound.socket (it may go to tokio) ...
bound.file.remove()?; // before the listener is closed
drop(bound.socket);
```

### What stays in the base

- The default `bind` returns no `SocketFile`, so its E stays until the process ends and is removed together with D by the OS's cleaning of temporary directories or by `sweep`. If clean-up matters, use `bind_ex` or resolve. A long-lived process that binds at long paths again and again (a daemon making a socket per session, and the like) piles up E until it ends with the default `bind`, so it cleans each up with `remove()` / `cleanup()` of `bind_ex`
- A connect leaves nothing (when it made B for the first time, that B goes on being used as the process's B)
- sockat never removes B. A B in the socket's directory (`.sockat.<random>`) and a B made after the checks failed (`sockat-<uid>.<random>`) are not covered by `sweep`, and may not be removed by the OS's cleaning either. A B in the socket's directory stays in the user's directory

The OS's cleaning of temporary directories:

| Base | A dead D | A live D |
|---|---|---|
| Linux's `/tmp` | systemd-tmpfiles removes it after 10 days | a directory with a flock on it is skipped (systemd v242 and later). In older environments (RHEL 8, ...) it may be removed after 10 days |
| `$XDG_RUNTIME_DIR` | stays until the last logout or a reboot | the same |
| macOS's `/tmp` | a directory holding a symlink is not removed, so it stays until a reboot | not removed |
| macOS's `$TMPDIR` | dirhelper may remove it after 3 days | the same |
| FreeBSD, NetBSD, OpenBSD, illumos | by default not removed, or removed at a reboot | the same |

When a live D is removed (by hand included), the spelling just cannot be followed any more; nothing unsafe happens, and the socket at the original path keeps working. Across an exec the fd holding D's lock is closed, so even while an inherited listener lives, D and E may be removed by `sweep` or the OS's cleaning. A procB spelling cannot be followed from the exec on.

### sweep and sweep_base

- `sockat::sweep()` looks in those of the default-named B of bases 1 to 3 (`/tmp/sockat-<uid>`, `$XDG_RUNTIME_DIR/sockat`, `$TMPDIR/sockat-<uid>`) that exist, and removes each D whose owning process is dead (a D on which `flock(LOCK_EX | LOCK_NB)` succeeds), with the E in it. It makes no B. A base or B that fails the checks is skipped silently
- `sockat::sweep_base(path)` does the same for a B passed as `Options::base`. The application that manages that B calls it. When B or its parent is not there it does nothing and succeeds. A path of the wrong form is `InvalidInput`, a symlink `EPERM`, something not a directory `ENOTDIR`, and a B owned by someone else, writable by group or others, or reached through a directory others can write to `EPERM`
- They look only at the directories right under B whose name has the form of a D (10 ASCII letters and digits). B is sockat's own; what has another name is left alone
- The D of a live process (this process's included) is not removed. E directly under B, and a D holding anything that is not a symlink, are left alone
- Neither is called automatically (B is listed neither at each resolve or bind, nor when the process starts, nor when it exits). A dead D goes only by a `sweep` the application calls or by the OS's cleaning
- Where tests run over and over (CI, repeated checks), one call of `sweep()` (`sweep_base` for a B at `Options::base`) keeps dead D from piling up

## resolve

For those who hand a spelling to std or another library themselves. A spelling is just a path, so it works whatever the socket type and the operation.

| Function | Spelling | Returns |
|---|---|---|
| `sockat::resolve_for_bind(path, options)` | link-dir's `B/D/E/<file name>`, and on macOS and NetBSD also link-file's `B/D/E` | `ForBind` (`path`, `spelling`, `route`, `base`, `skipped`, `cleanup()`, `socket_file()`) |
| `sockat::resolve_for_connect(path, options)` | link-file's `B/D/E` (any file name length) | `ForConnect` (`path`, `route`, `skipped`, `cleanup()`) |

- A path of a length std accepts is returned as it is (`Original` / `CwdRelative`)
- When no spelling fits (a length only thread-cwd / at reach, or B cannot be made), `ENAMETOOLONG`. To reach such a length, use `bind` / `connect`
- Call `ForBind::socket_file()` after the bind. It follows the spelling at that point and returns a `SocketFile` holding the socket file's real path and `(st_dev, st_ino)` (an error before the bind or after the clean-up; `ENOTSOCK` when what it reaches is not a socket). When the last component of the path is a symlink (which the bind follows on macOS and NetBSD), it takes the socket the symlink led to. Its `remove` removes the file and cleans up the same E as the `ForBind`. Without calling it, there is nothing that removes the file
- `ForConnect`'s spelling can be used for any number of connects; call `cleanup()` at the end
- `resolve_for_connect` does not look at `usable_cross_uid`

```rust
let spelled = sockat::resolve_for_bind(path, Default::default())?;
let listener = std::os::unix::net::UnixListener::bind(&spelled.path)?;
let file = spelled.socket_file()?;
// ...
file.remove()?;
drop(listener);

let to = sockat::resolve_for_connect(path, Default::default())?;
for _ in 0..3 {
    let stream = std::os::unix::net::UnixStream::connect(&to.path)?;
    // ...
}
to.cleanup()?;
```

## datagram

`sockat::datagram` mirrors the functions of std's `UnixDatagram` that take a path, and `unbound`. The routes and their order are those of stream. To a sender bound to a long path, the receiver can reply to the address from `recv_from` while the spelling can be followed (`Spelling::Linked`). On OpenBSD this needs Rust 1.100 or later ("The address the kernel reports" in [What differs from std for a long path](#what-differs-from-std-for-a-long-path)).

```rust
let server = sockat::datagram::bind(server_path)?;
let client = sockat::datagram::unbound()?;
sockat::datagram::connect(&client, server_path)?;
client.send(b"ping")?;
```

`send_to` and SEQPACKET are not there yet ([the specification's open questions](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#open-questions)). To `send_to` a long path, hand the spelling of `resolve_for_connect` to std's `send_to`.

## What differs from std for a long path

| Point | Detail |
|---|---|
| The address the kernel reports | `local_addr()` of the listener and of accepted streams, and `peer_addr()` of the connecting side, report the bind's spelling, not the original path. Whether it can be used later is told by `Spelling`. On illumos, the connecting side's `peer_addr()` reports the spelling passed to connect.<br>On OpenBSD, the `local_addr()`, `peer_addr()` and `recv_from()` of std in Rust 1.99 and earlier drop the last byte of the path (a bug in std, the same for a short path and without sockat; [rust-lang/rust#118925](https://github.com/rust-lang/rust/issues/118925), fixed in Rust 1.100.0). The address the kernel reports is whole. Replying to a datagram at the address from std's `recv_from` needs Rust 1.100 or later there |
| How operational tools show it | the addresses `ss -x`, `lsof` and `/proc/net/unix` show are the bind's spelling too. For link-dir, thread-cwd and at the file name stays at the end; a link-file spelling does not show it |
| When the listener is closed | as with std, the socket file is not removed when the listener is closed |
| The directory is resolved first | the E of link-dir, thread-cwd and at points to the real path of the socket's directory, and proc-direct, at-direct and thread-cwd-direct open the directory first and then pass the file name. A rename or a repointed symlink in between is not reflected |
| Another thread's chdir | relative returns an `Other` error when the cwd changed in between and stays changed. A change and change back is not seen |
| State grows outside the application's own place | B, D and E stay in `/tmp`, `$XDG_RUNTIME_DIR` and `$TMPDIR` ([What stays in the places](#what-stays-in-the-base)) |
| Environments where a live D can be removed | Linux with systemd older than v242, and B in macOS's `$TMPDIR`. The spelling just cannot be followed any more |
| A peer with `PrivateTmp` | when the peer's `/tmp` is isolated, the peer cannot follow a spelling through `/tmp`. On Linux procB is used first, so this hardly happens. `usable_with_private_tmp` tells |
| Permissions | the socket's directory is opened `O_SEARCH` on macOS and `O_PATH` on Linux, which need no more permission than a plain bind / connect. Other Unix targets open it `O_RDONLY`, so proc-direct, at-direct and thread-cwd-direct need read permission too |

## Patterns not to use

Ways of using sockat that remove an unrelated file or fail to remove the file ([the sockat specification's "Patterns not to use"](https://github.com/kawaz/sockat/blob/main/docs/SPEC.md#patterns-not-to-use)).

| Pattern | In Rust | Why not | Instead |
|---|---|---|---|
| removing or `stat`ing the socket file by the spelling the kernel reports, without looking at `Spelling` | `std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())` | a link-file spelling removes only E, and the file stays. A `Dangling` spelling names nothing | `std::fs::remove_file(path)` with the original path<br>`SocketFile::remove()` |
| using an old spelling after its clean-up or after the process ended | connecting by the spelling after `cleanup()` | after the clean-up, someone else may make B in `/tmp` again under the same name. A procB spelling cannot be followed after the process ends | use a spelling only within its `until` |
| binding with a relative path and removing by that relative path after the cwd changes | `sockat::bind("app.sock")` followed by<br>`std::env::set_current_dir(..)` and<br>`std::fs::remove_file("app.sock")` | hits a file in another place (`CwdRelative`) | make the path absolute with `std::path::absolute` before the bind<br>`SocketFile::remove()` |
| relying on closing the listener to remove the socket file | `drop(listener)` alone | it is not removed | `SocketFile::remove()`<br>remove by the original path |
| relying on `remove()` to close the listener too | `bound.file.remove()` alone | `remove()` only removes the file | close the listener yourself |
| calling `remove()` after the listener is closed | `drop(bound.socket); bound.file.remove()` | a different file made again after the close may not be told apart | call it before closing |
| having another uid follow a procB spelling or one whose `usable_cross_uid` is false | | it cannot follow it | pass `Options { usable_cross_uid: true, .. }` and check `Spelling` |
| branching on the strings of `route`, `base`, `skipped` or `reason` | `if bound.route == "link-file"` | they are for diagnostics and promise nothing about their content | branch on `Spelling` |

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

| Situation | Error |
|---|---|
| The directory does not exist | `ENOENT` |
| Something other than a directory is where the directory should be | `ENOTDIR` |
| For a long path, the real path of the socket's directory cannot be resolved | that error |
| Nothing to connect to | `ENOENT` |
| A file already exists where the bind goes | `EADDRINUSE` |
| A socket file nobody listens on | `ECONNREFUSED` |
| Full backlog | Linux: `EAGAIN` for a nonblocking connect (a blocking one waits for room); macOS: `ECONNREFUSED` |
| No route can be used (no spelling fits, no means) | `ENAMETOOLONG` |
| No spelling fits in resolve | `ENAMETOOLONG` |
| A directory on the way to `Options::base` does not exist | `ENOENT` |
| `Options::base` is neither a directory nor a symlink | `ENOTDIR` |
| `Options::base` is a symlink (not a real directory) | `EPERM` |
| `Options::base` fails the owner / permission checks (including, for a bind with `usable_cross_uid`, a directory on the way or the base itself not giving others x) | `EPERM` |
| `Options::base` is relative or does not end with a name (whatever the length of the path, and before a name in the abstract namespace is looked at) | `InvalidInput` |
| `Options::base` passes its check, but B cannot be made | the error of making it |
| A path too long for std that does not end with a name (`dir/`, `dir/.`, `..`, ...) | `InvalidInput` |
| A path containing a NUL (at any length) | `InvalidInput` |
| The empty path | `InvalidInput` |
| The cwd changed during relative, or during `bind_ex` of a relative path | `Other` |
| Right after `bind_ex`'s bind, what is there (where a symlink leads, when the last component is one) is not a socket. What `socket_file()` reaches is not a socket | `ENOTSOCK` |
| `cleanup` / `remove` cannot remove E (other than it being gone already) | the error of `unlink` |

- The directory is looked up first, so `ENOENT` and `ENOTDIR` are settled before the length is judged, and no route is tried (`ExError`'s `route` is `None`, `skipped` empty)
- `Options::base` is checked before the routes; when it fails, `route` is `None` and `skipped` empty. When B cannot be made after the check passed, `route` is `None` and `skipped` holds the routes passed over until then
- The `_ex` functions put the route that actually bound / connected and failed in `ExError::route`
- On macOS a full backlog also returns `ECONNREFUSED`, so `ECONNREFUSED` alone does not tell a socket file nobody listens on (the same with std's connect)

## Async runtimes

The results are std types, so any runtime that adopts std sockets can take them. With tokio:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` does not wait for the peer, and its clean-up is only an unlink, so it can be called on the runtime's worker threads directly. `bind` does not wait for a peer either (thread-cwd waits for its thread to end, but the thread binds and ends at once). `bind_ex`'s listener goes to the runtime the same way, and its `SocketFile` stays with the caller.

## Not checked

- Android has not even been compiled. It uses the same routes as Linux. Android apps run under a seccomp filter, so the thread cwd is not expected to be used
- NetBSD, OpenBSD, illumos and FreeBSD run the tests in CI VMs. The peer credentials have not been checked there
- The error a nonblocking connect returns on a full backlog, and that a connect completes within the call, have been checked on Linux and macOS only
- Apple OSes other than macOS (iOS, ...) use the same routes as macOS, but only iOS's clippy has been run. Whether they have `pthread_fchdir_np`, and whether a bind follows a symlink there, have not been checked either
- Whether link-file can be used for a connect on FreeBSD (it assumes a connect follows a symlink in the last component)
