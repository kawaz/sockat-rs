//! Route-level tests: each way of reaching a long path is forced and checked
//! for where the socket file ends up, that data flows both ways, and how
//! failures come out. The public API is tested separately in `tests/`.
//!
//! What every test here leans on:
//!
//! * The socket must appear at the path the caller gave (checked with
//!   `symlink_metadata(...).file_type().is_socket()`), not at whatever spelling
//!   was handed to the kernel. A route that "works" but creates the socket
//!   elsewhere is the failure these tests exist to catch.
//! * Temporary directories have random names and are removed when the test
//!   ends (`tempfile::TempDir`).
//! * Tests that need a particular working directory or a process-wide setting
//!   (SIGCHLD disposition) run in a child process of this test binary
//!   ([`run_child`]); the test runner is multi-threaded, so changing the cwd or
//!   a signal disposition in-process would disturb the other tests.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::path;
use crate::route::{self, ConnectOptions, DIRECT_MAX, Request, Route};
use crate::sys::{self, SockAddr};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A fresh, randomly named directory under `/tmp` (short, so the tests decide
/// the path lengths) that is removed at the end of the test.
fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// A fresh, randomly named directory below the working directory of the test
/// process (`<cwd>/target/sockat-tests/`), for tests that need the socket near
/// the cwd. Cargo runs unit tests with the package root as the cwd.
#[cfg(target_vendor = "apple")]
fn tmp_below_cwd() -> tempfile::TempDir {
    let base = std::env::current_dir().unwrap().join("target/sockat-tests");
    std::fs::create_dir_all(&base).unwrap();
    tempfile::Builder::new()
        .prefix("t")
        .tempdir_in(base)
        .unwrap()
}

/// Create directories under `base` so that `<dirs>/<name>` is exactly `total`
/// bytes long as a path starting with `base`, and return that socket path.
/// Directory names are runs of `d` of at most 60 bytes (well under NAME_MAX).
fn nest(base: &Path, total: usize, name: &str) -> PathBuf {
    let fixed = path::len(base) + 1 + name.len();
    assert!(total >= fixed, "{total} < {fixed}");
    let mut remaining = total - fixed;
    let mut dir = base.to_path_buf();
    while remaining > 0 {
        // Each directory adds "<k bytes>/", so a remainder of 1 cannot be
        // made; avoid leaving one behind.
        assert!(remaining >= 2, "cannot pad by exactly one byte");
        let mut k = (remaining - 1).min(60);
        if remaining - (k + 1) == 1 {
            k -= 1;
        }
        dir.push("d".repeat(k));
        remaining -= k + 1;
    }
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    assert_eq!(path::len(&p), total);
    p
}

/// [`nest`] with the length given for the part below `dir`: the socket path is
/// `<dir>/<tail bytes>`.
#[cfg(target_vendor = "apple")]
fn nest_tail(dir: &Path, tail: usize, name: &str) -> PathBuf {
    nest(dir, path::len(dir) + 1 + tail, name)
}

fn is_socket(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
}

/// Exchange a message in each direction between a client socket and the
/// stream the listener accepts for it.
fn ping(listener: OwnedFd, client: OwnedFd) {
    let listener = UnixListener::from(listener);
    let mut client = UnixStream::from(client);
    let (mut server, _) = listener.accept().unwrap();
    client.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    server.write_all(b"pong").unwrap();
    client.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"pong");
}

/// A bind: what `sockat::bind` does.
const BIND: Request = Request::Bind;

/// A blocking connect that may fork: what `sockat::connect` does.
const CONNECT: Request = Request::Connect(ConnectOptions::DEFAULT);

fn errno(e: &io::Error) -> Option<i32> {
    e.raw_os_error()
}

/// Run one test of this test binary in a child process, with `cwd` as its
/// working directory and `env` added to its environment, and require it to
/// pass. `test` is the test's path below the crate (`tests::...`).
///
/// The child is the same binary run with libtest's `--exact` filter, so it
/// works the same under `cargo test` and `cargo nextest`. The child tests
/// return early (and pass) when `SOCKAT_CHILD` is not set, which is how they
/// look in a normal run.
fn run_child(test: &str, cwd: &Path, env: &[(&str, &OsStr)]) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .current_dir(cwd)
        .env("SOCKAT_CHILD", "1")
        .envs(env.iter().map(|(k, v)| (k, v)))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "child {test} failed\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    // A misspelt filter runs zero tests and still exits 0.
    assert!(
        stdout.contains("1 passed"),
        "child {test} did not run\n{stdout}"
    );
    stdout
}

/// Leave a socket file at `path` that no process listens on, made by a child
/// process that binds and exits.
///
/// Binding and dropping a listener in this process is not enough under
/// `cargo test`: other tests run the fork route on other threads at the same
/// time, and each forked child holds a copy of every fd of this process
/// (including the listener) until it exits, so for that moment the socket is
/// still accepting. A listener that only ever existed in another process is
/// out of their reach.
fn stale_socket(path: &Path) {
    run_child(
        "tests::child_make_stale_socket",
        Path::new("/"),
        &[("SOCKAT_CHILD_PATH", path.as_os_str())],
    );
    assert!(is_socket(path));
}

#[test]
fn child_make_stale_socket() {
    if !in_child() {
        return;
    }
    let p = env_path("SOCKAT_CHILD_PATH");
    route::run(BIND, &p).unwrap();
}

fn in_child() -> bool {
    std::env::var_os("SOCKAT_CHILD").is_some()
}

fn env_path(key: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("{key} not set")))
}

// ---------------------------------------------------------------------------
// helpers for the connect options (Linux and Apple, where the tests run)
// ---------------------------------------------------------------------------

/// A connect that does not block (and may fork).
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
const CONNECT_NB: Request = Request::Connect(ConnectOptions {
    nonblocking: true,
    fork: true,
});

/// What a nonblocking connect answers while the listener's backlog is full
/// (Linux: `EAGAIN`, where a blocking connect waits for room; macOS:
/// `ECONNREFUSED`, as a blocking connect does).
#[cfg(any(target_os = "linux", target_os = "android"))]
const FULL_BACKLOG: i32 = libc::EAGAIN;
#[cfg(target_vendor = "apple")]
const FULL_BACKLOG: i32 = libc::ECONNREFUSED;

/// One socket path reached one fixed way: the route order of the platform
/// with its inputs (`/proc` place, `Config`) chosen so that one route is
/// taken. Called with what to do there.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
type Way = std::sync::Arc<dyn Fn(Request) -> io::Result<(OwnedFd, Route)> + Send + Sync>;

/// Whether a route is the one a [`Way`] must take.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
type Want = fn(Route) -> bool;

/// Run `f` on another thread and return its result, failing the test if it
/// has not returned within 10 seconds.
///
/// The calls run through this must return at once (a nonblocking connect to a
/// full backlog). Should one block instead, as a blocking connect on Linux
/// does until the listener accepts, the test fails here rather than hanging
/// until a runner kills it (plain `cargo test`, which `just test-linux` runs,
/// never does). The 10 seconds only stand for "never returns": a call that
/// returns does so within microseconds, so it is no estimate of a duration.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn at_once<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    use std::sync::mpsc::{RecvTimeoutError, channel};
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(v) => v,
        Err(RecvTimeoutError::Timeout) => panic!("the call blocked"),
        Err(RecvTimeoutError::Disconnected) => panic!("the call panicked (see above)"),
    }
}

/// Whether `fd` is `O_NONBLOCK`.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn nonblocking(fd: &OwnedFd) -> bool {
    use std::os::fd::AsFd;
    sys::is_nonblocking(fd.as_fd()).unwrap()
}

/// The pid the server's peer-credential query names for `server` (an accepted
/// stream): `SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn peer_pid(server: &UnixStream) -> libc::pid_t {
    use std::os::fd::AsFd;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    return sys::peer_cred_pid(server.as_fd()).unwrap();
    #[cfg(target_vendor = "apple")]
    return sys::local_peer_pid(server.as_fd()).unwrap();
}

/// Check the nonblocking connect of `way`, whose route must satisfy `want`.
/// `what` names the route in failure messages.
///
/// * The blocking connect (what `sockat::connect` does) is unchanged: it
///   returns a socket that is not `O_NONBLOCK`.
/// * With room in the backlog, a nonblocking connect succeeds at once on the
///   same route and returns an `O_NONBLOCK` socket: a read with nothing to
///   read answers `WouldBlock` instead of waiting, and data flows both ways.
/// * With the backlog shrunk to 1 and filled, a nonblocking connect fails at
///   once with [`FULL_BACKLOG`] (a blocking one would wait on Linux), within
///   a few attempts (Linux lets backlog + 1 connections wait, macOS 1), and
///   keeps failing that way while it stays full. Every connect that got in
///   meanwhile is `O_NONBLOCK` too.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn check_nonblocking_connect(way: &Way, want: Want, what: &str) {
    use std::os::fd::AsFd;
    let (l, r) = way(BIND).unwrap();
    assert!(want(r), "{what}: bind {r:?}");
    let listener = UnixListener::from(l);

    let (c, r) = way(CONNECT).unwrap();
    assert!(want(r), "{what}: blocking {r:?}");
    assert!(
        !nonblocking(&c),
        "{what}: a blocking connect stays blocking"
    );
    drop(listener.accept().unwrap());

    let w = way.clone();
    let (c, r) = at_once(move || w(CONNECT_NB)).unwrap();
    assert!(want(r), "{what}: nonblocking {r:?}");
    assert!(nonblocking(&c), "{what}: not O_NONBLOCK");
    let mut c = UnixStream::from(c);
    let (mut s, _) = listener.accept().unwrap();
    let mut buf = [0u8; 4];
    let e = c.read(&mut buf).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{what}: {e}");
    c.write_all(b"ping").unwrap();
    s.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    // A Unix stream write lands in the peer's receive queue before it
    // returns, so the client's read finds the data without waiting.
    s.write_all(b"pong").unwrap();
    c.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"pong");

    sys::relisten(listener.as_fd(), 1).unwrap();
    let mut queued = Vec::new();
    let full = loop {
        assert!(queued.len() < 16, "{what}: the backlog never filled");
        let w = way.clone();
        match at_once(move || w(CONNECT_NB)) {
            Ok((c, r)) => {
                assert!(want(r), "{what}: {r:?}");
                assert!(nonblocking(&c), "{what}: not O_NONBLOCK");
                queued.push(c);
            }
            Err(e) => break e,
        }
    };
    assert!(
        !queued.is_empty(),
        "{what}: nothing got in before it was full"
    );
    assert_eq!(errno(&full), Some(FULL_BACKLOG), "{what}: {full}");
    let w = way.clone();
    let e = at_once(move || w(CONNECT_NB)).unwrap_err();
    assert_eq!(errno(&e), Some(FULL_BACKLOG), "{what}: {e}");
}

/// Check the child's pid that `way` reports for a connect, blocking and not:
/// when it forks (`forks`), the pid is that of the child, which is what the
/// server's peer-credential query names right after the connect (and not
/// this process); otherwise there is none, and the server names this
/// process.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn check_child_pid(way: &Way, forks: bool, what: &str) {
    let me = std::process::id() as libc::pid_t;
    let (l, _) = way(BIND).unwrap();
    let listener = UnixListener::from(l);
    for req in [CONNECT, CONNECT_NB] {
        let (_c, r) = way(req).unwrap();
        let (s, _) = listener.accept().unwrap();
        let seen = peer_pid(&s);
        if forks {
            assert_eq!(r.child(), Some(seen), "{what} {req:?}: {r:?}");
            assert_ne!(seen, me, "{what} {req:?}");
        } else {
            assert_eq!(r.child(), None, "{what} {req:?}: {r:?}");
            assert_eq!(seen, me, "{what} {req:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// every platform
// ---------------------------------------------------------------------------

// The plain route is the contract that short paths behave exactly like std:
// at most DIRECT_MAX (100) bytes goes to std unchanged, one byte more does not.
// 100 is the same on every platform and below both sun_path sizes (104 / 108
// with the NUL), so a path's route does not depend on where it runs.
#[test]
fn a_path_of_100_bytes_goes_to_std_unchanged_and_101_does_not() {
    let t = tmp();
    let at_limit = nest(t.path(), DIRECT_MAX, "s.sock");
    let (l, r) = route::run(BIND, &at_limit).unwrap();
    assert_eq!(r, Route::Direct);
    let (c, r) = route::run(CONNECT, &at_limit).unwrap();
    assert_eq!(r, Route::Direct);
    ping(l, c);
    // std's own view of the bound address is the path itself on this route.
    let l = UnixListener::bind(t.path().join("x.sock")).unwrap();
    assert_eq!(
        l.local_addr().unwrap().as_pathname(),
        Some(t.path().join("x.sock").as_path())
    );

    let t = tmp();
    let over = nest(t.path(), DIRECT_MAX + 1, "s.sock");
    let (l, r) = route::run(BIND, &over).unwrap();
    assert_ne!(r, Route::Direct);
    assert!(is_socket(&over));
    let (c, _) = route::run(CONNECT, &over).unwrap();
    ping(l, c);
}

// Long paths of several sizes all end up as a socket at the given path, and
// a client reaching it by the same long path talks to that listener. 600 bytes
// is well past both 253 (the widened sockaddr on macOS) and sun_path, so on
// macOS it exercises the symlink or fork route.
#[test]
fn long_paths_bind_at_the_given_path_and_accept_connections() {
    for total in [101, 150, 253, 254, 300, 600] {
        let t = tmp();
        let p = nest(t.path(), total, "s.sock");
        let (l, _) = route::run(BIND, &p).unwrap();
        assert!(is_socket(&p), "{total}");
        let (c, _) = route::run(CONNECT, &p).unwrap();
        ping(l, c);
    }
}

// Failures are the kernel's errors for the path the caller gave, whatever the
// route: a missing directory is ENOENT for both bind and connect; a missing
// socket is ENOENT; an existing file is EADDRINUSE; a socket file nobody listens
// on is ECONNREFUSED. A route must not turn these into "try the next route" and
// end up reporting something else (or creating the socket elsewhere).
#[test]
fn long_path_failures_are_the_kernels_errors() {
    for total in [150, 300, 600] {
        let t = tmp();
        let p = nest(t.path(), total, "s.sock");

        let missing_dir = p.parent().unwrap().join("nope").join("s.sock");
        let e = route::run(BIND, &missing_dir).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{total}: bind {e}");
        let e = route::run(CONNECT, &missing_dir).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{total}: connect {e}");

        let e = route::run(CONNECT, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{total}: no socket {e}");

        let (_l, _) = route::run(BIND, &p).unwrap();
        let e = route::run(BIND, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::EADDRINUSE), "{total}: {e}");

        let stale = p.with_file_name("stale.sock");
        stale_socket(&stale);
        let e = route::run(CONNECT, &stale).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ECONNREFUSED), "{total}: {e}");
    }
}

// A file name that is too long for every route is ENAMETOOLONG. Every route
// that does not hand the whole path to the kernel ends with the file name
// alone in a sockaddr (Linux: behind "/proc/self/fd/N/"; macOS: in a widened
// sockaddr of at most 253 bytes; the fork route: as is), so a name of 254 bytes
// (NAME_MAX is 255) cannot be reached anywhere.
#[test]
fn a_file_name_too_long_for_any_route_is_enametoolong() {
    let t = tmp();
    let name = "n".repeat(254);
    let p = t.path().join(&name);
    for op in [BIND, CONNECT] {
        let e = route::run(op, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{op:?}: {e}");
    }
    assert!(!is_socket(&p));
}

// A long path that does not end with a file name, or that contains a NUL, is
// refused as InvalidInput before anything is created (std refuses a NUL in a
// short path the same way). Paths ending in "/" or "/." name a directory;
// binding them by "the last component" would create a socket named after the
// directory, which is not what the path says.
#[test]
fn long_paths_without_a_file_name_or_with_nul_are_invalid_input() {
    let t = tmp();
    let p = nest(t.path(), 150, "s.sock");
    let dir = p.parent().unwrap();
    let mut slash = dir.as_os_str().to_owned();
    slash.push("/");
    let mut dot = slash.clone();
    dot.push(".");
    let mut nul = dir.join("a").into_os_string();
    nul.push("\0b");
    for bad in [slash, dot, nul] {
        for op in [BIND, CONNECT] {
            let e = route::run(op, Path::new(&bad)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{bad:?} {op:?}");
        }
    }
}

// The crate-level connect with options, as the public API will use it: a long
// path (200 bytes: the /proc route on Linux, the widened sockaddr on macOS,
// neither of which forks) connects with no child's pid, blocking with the
// default options and O_NONBLOCK with `nonblocking`. The route-by-route checks
// are in the platform sections.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn connect_with_returns_the_stream_as_asked_and_no_pid_without_a_fork() {
    use std::os::fd::AsFd;
    let t = tmp();
    let p = nest(t.path(), 200, "s.sock");
    let listener = crate::bind(&p).unwrap();
    for nb in [false, true] {
        let options = ConnectOptions {
            nonblocking: nb,
            ..ConnectOptions::DEFAULT
        };
        let (c, child) = crate::connect_with(&p, options).unwrap();
        assert_eq!(child, None);
        assert_eq!(sys::is_nonblocking(c.as_fd()).unwrap(), nb);
        drop(listener.accept().unwrap());
    }
    let c = crate::connect(&p).unwrap();
    assert!(!sys::is_nonblocking(c.as_fd()).unwrap());
}

// The fork route (forced here; the last resort on macOS and on Linux without
// /proc, and the only long-path route on other Unix) binds and connects relative to the directory fd from a
// forked child that fchdirs. The parent's socket is the one bound / connected
// (fork shares the open file description), the socket file is in the
// directory, and the parent's cwd is untouched.
#[test]
fn the_fork_route_binds_and_connects_through_the_shared_socket() {
    let cwd = std::env::current_dir().unwrap();
    let t = tmp();
    let p = nest(t.path(), 400, "s.sock");
    let (dir, name) = path::split(&p).unwrap();
    let addr = SockAddr::standard(name).unwrap();
    let l = route::fork_op(BIND, dir, &addr).unwrap().0;
    assert!(is_socket(&p));
    let c = route::fork_op(CONNECT, dir, &addr).unwrap().0;
    assert_eq!(std::env::current_dir().unwrap(), cwd);
    ping(l, c);
}

// The child of the fork route reports errno through its exit status, and the
// parent turns it back into the same io::Error the syscall would have given:
// ENOENT for a missing socket or directory, EADDRINUSE for an existing file,
// ECONNREFUSED for a dead socket file.
#[test]
fn the_fork_route_reports_the_childs_errno() {
    let t = tmp();
    let p = nest(t.path(), 400, "s.sock");
    let (dir, name) = path::split(&p).unwrap();
    let addr = SockAddr::standard(name).unwrap();

    let e = route::fork_op(CONNECT, dir, &addr).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");

    let e = route::fork_op(BIND, &dir.join("nope"), &addr).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");

    let _l = route::fork_op(BIND, dir, &addr).unwrap().0;
    let e = route::fork_op(BIND, dir, &addr).unwrap_err();
    assert_eq!(errno(&e), Some(libc::EADDRINUSE), "{e}");

    stale_socket(&dir.join("stale.sock"));
    let stale = SockAddr::standard(OsStr::new("stale.sock")).unwrap();
    let e = route::fork_op(CONNECT, dir, &stale).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ECONNREFUSED), "{e}");
}

// When something else reaps the fork route's child first (SIGCHLD set to
// SIG_IGN makes the kernel do it, and waitpid then fails with ECHILD), the
// outcome cannot come from the exit status; it is read off the socket: bound /
// connected is success, anything else an error. Runs in a child process
// because the SIGCHLD disposition is process-wide.
#[test]
fn the_fork_route_survives_its_child_being_reaped_elsewhere() {
    let t = tmp();
    let p = nest(t.path(), 400, "s.sock");
    run_child(
        "tests::child_fork_with_sigchld_ignored",
        t.path(),
        &[("SOCKAT_CHILD_PATH", p.as_os_str())],
    );
}

#[test]
fn child_fork_with_sigchld_ignored() {
    if !in_child() {
        return;
    }
    sys::ignore_sigchld();
    let p = env_path("SOCKAT_CHILD_PATH");
    let (dir, name) = path::split(&p).unwrap();
    let addr = SockAddr::standard(name).unwrap();
    let l = route::fork_op(BIND, dir, &addr).unwrap().0;
    assert!(is_socket(&p));
    let c = route::fork_op(CONNECT, dir, &addr).unwrap().0;
    ping(l, c);
    // A failed connect cannot be told apart by errno any more, but it is still
    // a failure.
    let missing = SockAddr::standard(OsStr::new("missing.sock")).unwrap();
    assert!(route::fork_op(CONNECT, dir, &missing).is_err());
}

// Routes run concurrently from many threads must not disturb other threads'
// relative path lookups: none of them changes the process's cwd (the fork
// route changes only the child's). Watcher threads resolve a relative path and
// compare getcwd the whole time the workers bind / connect.
#[test]
fn concurrent_routes_never_change_the_cwd() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    let cwd = std::env::current_dir().unwrap();
    assert!(Path::new("Cargo.toml").exists(), "cwd is the package root");
    let stop = Arc::new(AtomicBool::new(false));
    let checks = Arc::new(AtomicUsize::new(0));
    let watchers: Vec<_> = (0..2)
        .map(|_| {
            let (stop, checks, cwd) = (stop.clone(), checks.clone(), cwd.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    assert!(std::fs::metadata("Cargo.toml").is_ok());
                    assert_eq!(std::env::current_dir().unwrap(), cwd);
                    checks.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    let workers: Vec<_> = (0..8)
        .map(|i| {
            std::thread::spawn(move || {
                for round in 0..10 {
                    let t = tmp();
                    let p = nest(t.path(), [150, 300, 600][(i + round) % 3], "s.sock");
                    let (l, _) = route::run(BIND, &p).unwrap();
                    let (c, _) = route::run(CONNECT, &p).unwrap();
                    ping(l, c);
                    let (dir, _) = path::split(&p).unwrap();
                    let fp = dir.join("f.sock");
                    let faddr = SockAddr::standard(OsStr::new("f.sock")).unwrap();
                    let l = route::fork_op(BIND, dir, &faddr).unwrap().0;
                    assert!(is_socket(&fp));
                    let c = route::fork_op(CONNECT, dir, &faddr).unwrap().0;
                    ping(l, c);
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for w in watchers {
        w.join().unwrap();
    }
    assert!(checks.load(Ordering::Relaxed) > 0);
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux {
    use super::*;
    use crate::route::linux;
    use std::os::fd::AsFd;

    // Every path over 100 bytes takes /proc/self/fd/<dirfd>/<name> on Linux,
    // however long the directory part is (the directory never goes into the
    // sockaddr). With /proc there, nothing else is tried.
    #[test]
    fn long_paths_take_the_proc_fd_route() {
        for total in [101, 300, 1000] {
            let t = tmp();
            let p = nest(t.path(), total, "s.sock");
            let (l, r) = route::run(BIND, &p).unwrap();
            assert_eq!(r, Route::ProcFd, "{total}");
            assert!(is_socket(&p));
            let (c, r) = route::run(CONNECT, &p).unwrap();
            assert_eq!(r, Route::ProcFd);
            ping(l, c);
        }
    }

    // The kernel records the spelling that was bound, so local_addr of a
    // listener bound through /proc names a /proc/self/fd path, which means
    // nothing once the directory fd is closed (and nothing in other
    // processes). This is why the docs tell users to keep their own path.
    #[test]
    fn local_addr_reports_the_proc_spelling_not_the_path() {
        let t = tmp();
        let p = nest(t.path(), 200, "s.sock");
        let (l, _) = route::run(BIND, &p).unwrap();
        let l = UnixListener::from(l);
        let addr = l.local_addr().unwrap();
        let shown = addr.as_pathname().unwrap();
        assert!(shown.starts_with("/proc/self/fd/"), "{shown:?}");
        assert!(shown.ends_with("s.sock"));
    }

    // "/proc/self/fd/<n>/" takes about 17 bytes of the 107 that sun_path holds
    // on Linux, so a file name of 95 bytes cannot be reached and must be
    // ENAMETOOLONG (not std's own InvalidInput for a long path, which would
    // differ from the other platforms).
    #[test]
    fn a_file_name_that_does_not_fit_behind_proc_is_enametoolong() {
        let t = tmp();
        let p = t.path().join("n".repeat(95));
        let e = linux::run(BIND, &p, Path::new(linux::PROC_SELF_FD)).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
    }

    /// A stand-in for `/proc/self/fd` on a system without /proc: a path under
    /// `t` with nothing at `<it>/<fd>`. `empty` picks an existing, empty
    /// directory (a /proc mount point with nothing mounted on it) over a path
    /// that does not exist at all (no /proc directory).
    fn no_proc(t: &Path, empty: bool) -> PathBuf {
        let p = t.join("proc/self/fd");
        if empty {
            std::fs::create_dir_all(&p).unwrap();
        }
        p
    }

    // Without /proc (kawaz's ruling) the /proc route cannot be used at all, so
    // Linux takes the fork route, as the other Unix targets do, instead of
    // failing. Both shapes of "no /proc" are checked: no /proc directory, and
    // an empty /proc with nothing mounted. Paths whose directory part is
    // long, and a file name of 95 bytes that does not fit behind
    // /proc/self/fd/<n>/ but does fit the fork route's sockaddr (the length
    // limit of the /proc spelling is not applied when that spelling is not
    // used). Each binds at the given path and accepts a connection made the
    // same way. The real /proc is untouched: only the place to look is
    // changed.
    #[test]
    fn without_proc_the_fork_route_binds_and_connects() {
        for empty in [false, true] {
            let t = tmp();
            let proc = no_proc(t.path(), empty);
            for (total, name) in [(101, "s.sock"), (300, "s.sock"), (1000, "s.sock")] {
                let p = nest(t.path(), total, name);
                let (l, r) = linux::run(BIND, &p, &proc).unwrap();
                assert!(matches!(r, Route::Fork { .. }), "{r:?} {total} {empty}");
                assert!(is_socket(&p));
                let (c, r) = linux::run(CONNECT, &p, &proc).unwrap();
                assert!(matches!(r, Route::Fork { .. }), "{r:?}");
                ping(l, c);
            }
            let p = t.path().join("n".repeat(95));
            let (l, r) = linux::run(BIND, &p, &proc).unwrap();
            assert!(matches!(r, Route::Fork { .. }), "{r:?}");
            assert!(is_socket(&p));
            let (c, _) = linux::run(CONNECT, &p, &proc).unwrap();
            ping(l, c);
        }
    }

    // The fork route taken for lack of /proc reports the kernel's errors for
    // the given path like every other route: ENOENT for a missing directory
    // (from opening it, before any route is chosen) and for a missing socket,
    // EADDRINUSE for an existing file, ECONNREFUSED for a socket nobody
    // listens on. In particular a missing directory must not come out as
    // anything about /proc.
    #[test]
    fn without_proc_failures_are_the_kernels_errors() {
        let t = tmp();
        let proc = no_proc(t.path(), false);
        let p = nest(t.path(), 300, "s.sock");
        let missing_dir = p.parent().unwrap().join("nope").join("s.sock");
        for op in [BIND, CONNECT] {
            let e = linux::run(op, &missing_dir, &proc).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENOENT), "{op:?} {e}");
        }
        let e = linux::run(CONNECT, &p, &proc).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");
        let (_l, _) = linux::run(BIND, &p, &proc).unwrap();
        let e = linux::run(BIND, &p, &proc).unwrap_err();
        assert_eq!(errno(&e), Some(libc::EADDRINUSE), "{e}");
        let stale = p.with_file_name("stale.sock");
        stale_socket(&stale);
        let e = linux::run(CONNECT, &stale, &proc).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ECONNREFUSED), "{e}");
    }

    // The side effect documented for connecting without /proc, through the
    // route order itself: the server's SO_PEERCRED names the forked child,
    // not this process, and keeps naming it after this process has sent
    // data. With /proc the same connect names this process.
    #[test]
    fn without_proc_the_server_sees_the_forked_child_as_the_peer() {
        let me = std::process::id() as libc::pid_t;
        let t = tmp();
        let proc = no_proc(t.path(), false);
        let p = nest(t.path(), 300, "s.sock");
        let (l, _) = linux::run(BIND, &p, &proc).unwrap();
        let l = UnixListener::from(l);
        let (c, r) = linux::run(CONNECT, &p, &proc).unwrap();
        assert!(matches!(r, Route::Fork { .. }), "{r:?}");
        let mut c = UnixStream::from(c);
        let (mut s, _) = l.accept().unwrap();
        let child = sys::peer_cred_pid(s.as_fd()).unwrap();
        assert_ne!(child, me);
        c.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        s.read_exact(&mut b).unwrap();
        assert_eq!(sys::peer_cred_pid(s.as_fd()).unwrap(), child);

        let (_c, r) = linux::run(CONNECT, &p, Path::new(linux::PROC_SELF_FD)).unwrap();
        assert_eq!(r, Route::ProcFd);
        let (s, _) = l.accept().unwrap();
        assert_eq!(sys::peer_cred_pid(s.as_fd()).unwrap(), me);
    }

    // The side effect documented for connecting without /proc: SO_PEERCRED is
    // recorded at connect time and keeps the pid of the child that connected,
    // even after the child has exited and the real client process has sent
    // data. This is why Linux forks only without /proc; pinned here so the
    // reason stays checkable.
    #[test]
    fn so_peercred_keeps_the_pid_of_a_forked_connector() {
        let t = tmp();
        let p = nest(t.path(), 300, "s.sock");
        let (dir, name) = path::split(&p).unwrap();
        let addr = SockAddr::standard(name).unwrap();
        let l = UnixListener::from(route::fork_op(BIND, dir, &addr).unwrap().0);
        let mut c = UnixStream::from(route::fork_op(CONNECT, dir, &addr).unwrap().0);
        let (mut s, _) = l.accept().unwrap();
        let me = std::process::id() as libc::pid_t;
        let before = sys::peer_cred_pid(s.as_fd()).unwrap();
        assert_ne!(before, me);
        c.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        s.read_exact(&mut b).unwrap();
        assert_eq!(sys::peer_cred_pid(s.as_fd()).unwrap(), before);

        // The /proc route connects from this process itself.
        let (c2, _) = route::run(CONNECT, &p).unwrap();
        let (s2, _) = l.accept().unwrap();
        assert_eq!(sys::peer_cred_pid(s2.as_fd()).unwrap(), me);
        drop(c2);
    }

    /// The three Linux routes, each a [`Way`] to its own socket path under
    /// `t`: the path itself (100 bytes), /proc (300 bytes), and the fork route
    /// (300 bytes, with /proc made to look absent). With the route each must
    /// take.
    fn ways(t: &Path) -> [(&'static str, Way, Want); 3] {
        let proc = Path::new(linux::PROC_SELF_FD).to_path_buf();
        let none = no_proc(t, false);
        let direct = nest(t, 100, "direct.sock");
        let via_proc = nest(t, 300, "proc.sock");
        let forked = nest(t, 300, "fork.sock");
        let (p1, p2) = (proc.clone(), proc);
        [
            (
                "Direct",
                std::sync::Arc::new(move |req| linux::run(req, &direct, &p1)),
                |r| r == Route::Direct,
            ),
            (
                "ProcFd",
                std::sync::Arc::new(move |req| linux::run(req, &via_proc, &p2)),
                |r| r == Route::ProcFd,
            ),
            (
                "Fork",
                std::sync::Arc::new(move |req| linux::run(req, &forked, &none)),
                |r| matches!(r, Route::Fork { .. }),
            ),
        ]
    }

    // A nonblocking connect must not wait on a full backlog on any route
    // (the blocking one does, on Linux: it sleeps until the listener
    // accepts). Each route makes its socket O_NONBLOCK before connecting: the
    // path itself and /proc on sockat's own socket rather than std's (std
    // cannot connect a nonblocking socket), the fork route in the parent
    // before the fork, so the child's connect is nonblocking through the open
    // file description they share. See check_nonblocking_connect for what is
    // checked on each.
    #[test]
    fn a_nonblocking_connect_fails_at_once_on_a_full_backlog_on_every_route() {
        let t = tmp();
        for (what, way, want) in ways(t.path()) {
            check_nonblocking_connect(&way, want, what);
        }
    }

    // DR-0001 decision 8: the connect reports the pid the server will see as
    // the peer when that is not this process. Only the fork route connects
    // from another process (the child, whose pid SO_PEERCRED keeps); the path
    // itself and /proc connect from this process and report no pid.
    #[test]
    fn only_the_fork_route_reports_the_childs_pid_and_it_is_the_servers_peer() {
        let t = tmp();
        for (what, way, _) in ways(t.path()) {
            check_child_pid(&way, what == "Fork", what);
        }
    }

    // With forking ruled out, a path only the fork route reaches (here: no
    // /proc) fails with ENAMETOOLONG, the error of a plain connect to a path
    // too long for sun_path, and nothing happens: no connection reaches the
    // listener (its accept would block) and nothing is created next to the
    // socket. Blocking or not makes no difference. The other routes are not
    // affected: with /proc the same path connects through it, and a short path
    // goes as is. A real failure stays what it is: a missing directory is
    // ENOENT (opening it comes before the fork route is chosen).
    #[test]
    fn without_fork_a_path_that_needs_it_is_enametoolong_and_nothing_is_made() {
        let t = tmp();
        let none = no_proc(t.path(), false);
        let p = nest(t.path(), 300, "s.sock");
        let (l, _) = linux::run(BIND, &p, Path::new(linux::PROC_SELF_FD)).unwrap();
        let listener = UnixListener::from(l);
        listener.set_nonblocking(true).unwrap();
        let dir = p.parent().unwrap();
        let before: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        for nonblocking in [false, true] {
            let req = Request::Connect(ConnectOptions {
                nonblocking,
                fork: false,
            });
            let e = linux::run(req, &p, &none).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{nonblocking}: {e}");
            let e = listener.accept().unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "a connection got in");

            let missing = dir.join("nope").join("s.sock");
            let e = linux::run(req, &missing, &none).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENOENT), "{nonblocking}: {e}");

            let (_c, r) = linux::run(req, &p, Path::new(linux::PROC_SELF_FD)).unwrap();
            assert_eq!(r, Route::ProcFd);
            drop(listener.accept().unwrap());
        }
        let after: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after);

        let short = nest(t.path(), 100, "short.sock");
        let (l, _) = route::run(BIND, &short).unwrap();
        let req = Request::Connect(ConnectOptions {
            nonblocking: false,
            fork: false,
        });
        let (c, r) = linux::run(req, &short, &none).unwrap();
        assert_eq!(r, Route::Direct);
        ping(l, c);
    }
}

// ---------------------------------------------------------------------------
// Apple
// ---------------------------------------------------------------------------

#[cfg(target_vendor = "apple")]
mod apple {
    use super::*;
    use crate::route::apple::{
        self, Config, LINK_PREFIX, LINK_RANDOM_LEN, Relative, SymlinkOutcome, TempLink,
    };
    use crate::sys::{Op, WIDE_PATH_MAX};
    use std::ffi::OsString;
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Component;

    fn cfg(tmp_bases: &[&Path], wide: bool) -> Config {
        Config {
            tmp_bases: tmp_bases.iter().map(|b| b.to_path_buf()).collect(),
            wide,
        }
    }

    fn has_dotdot(p: &Path) -> bool {
        p.components().any(|c| c == Component::ParentDir)
    }

    /// Gives a directory back mode 0700 on drop.
    struct RestoreMode<'a>(&'a Path);

    impl Drop for RestoreMode<'_> {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            // TempDir needs to list the directory to remove it.
            let _ = std::fs::set_permissions(self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    /// Make `dir` mode 0300 (write + search, no read) until the guard drops,
    /// and check that it can no longer be opened `O_RDONLY`.
    fn unreadable(dir: &Path) -> RestoreMode<'_> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o300)).unwrap();
        let restore = RestoreMode(dir);
        let e = std::fs::File::open(dir).unwrap_err();
        assert_eq!(
            errno(&e),
            Some(libc::EACCES),
            "the premise: no read permission"
        );
        restore
    }

    fn assert_empty(dir: &Path) {
        let left: Vec<_> = std::fs::read_dir(dir).unwrap().collect();
        assert!(left.is_empty(), "left behind in {dir:?}: {left:?}");
    }

    // XNU takes a socket address of up to SOCK_MAXADDRLEN (255) bytes, so the
    // widened form carries 255 - 2 = 253 bytes of path. Pinned against the
    // running kernel: 253 bytes binds and connects with the socket at the path;
    // SockAddr::wide refuses 254 before the kernel is asked.
    #[test]
    fn the_widened_sockaddr_carries_253_bytes() {
        assert_eq!(WIDE_PATH_MAX, 253);
        let t = tmp();
        let p = nest(t.path(), 253, "s.sock");
        let l = apple::wide_op(BIND, &p).unwrap();
        assert!(is_socket(&p));
        let c = apple::wide_op(CONNECT, &p).unwrap();
        ping(l, c);

        let t = tmp();
        let p = nest(t.path(), 254, "s.sock");
        let e = apple::wide_op(BIND, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG));
    }

    // What std reports for a socket bound through the widened form: XNU's
    // getsockname copies at most the caller's buffer (std passes a plain
    // sockaddr_un) and reports that length, so std shows a truncated path.
    // It must not panic, and it is not the path: users keep their own.
    #[test]
    fn local_addr_of_a_widened_bind_is_truncated() {
        let t = tmp();
        let p = nest(t.path(), 200, "s.sock");
        let l = UnixListener::from(apple::wide_op(BIND, &p).unwrap());
        let addr = l.local_addr().unwrap();
        let shown = addr.as_pathname().unwrap();
        assert_ne!(shown, p.as_path());
        eprintln!(
            "local_addr of a widened bind: {} bytes {shown:?}",
            path::len(shown)
        );
        assert!(
            p.as_os_str()
                .as_bytes()
                .starts_with(shown.as_os_str().as_bytes())
        );
    }

    // Route order 3a: a path over 100 bytes whose spelling relative to the cwd
    // is at most 100 bytes takes the relative route (through std, the plain
    // sockaddr), before the widened form is considered. The socket is at the
    // given absolute path.
    #[test]
    fn a_path_short_relative_to_the_cwd_takes_the_relative_route() {
        let t = tmp_below_cwd();
        let cwd = std::env::current_dir().unwrap();
        // Relative spelling of 99 bytes; the absolute path is cwd + "/" + 99.
        let rel_len = 99;
        let rel_base = t.path().strip_prefix(&cwd).unwrap();
        let p = cwd.join(nest(rel_base, rel_len, "s.sock"));
        assert!(path::len(&p) > DIRECT_MAX);
        let c = cfg(&[Path::new("/nonexistent")], true);
        let (l, r) = apple::run(BIND, &p, &c).unwrap();
        assert_eq!(r, Route::Relative);
        assert!(is_socket(&p));
        let (cl, r) = apple::run(CONNECT, &p, &c).unwrap();
        assert_eq!(r, Route::Relative);
        ping(l, cl);
    }

    // Route order 3b with the path itself: over 100 bytes, no relative spelling
    // of at most 100, and at most 253 bytes: the widened form with the path.
    // Under /tmp the relative spelling from the package root climbs all the way
    // up, so it is long.
    #[test]
    fn a_path_up_to_253_bytes_takes_the_widened_route() {
        for total in [101, 200, 253] {
            let t = tmp();
            let p = nest(t.path(), total, "s.sock");
            let c = cfg(&[Path::new("/nonexistent")], true);
            let (l, r) = apple::run(BIND, &p, &c).unwrap();
            assert_eq!(r, Route::Wide, "{total}");
            assert!(is_socket(&p));
            let (cl, r) = apple::run(CONNECT, &p, &c).unwrap();
            assert_eq!(r, Route::Wide);
            ping(l, cl);
        }
    }

    // Route order 3b with the relative spelling: the path is over 253 bytes but
    // its spelling relative to the cwd is at most 253.
    #[test]
    fn a_path_over_253_bytes_short_relative_to_the_cwd_takes_the_widened_relative_route() {
        let t = tmp_below_cwd();
        let cwd = std::env::current_dir().unwrap();
        let rel_base = t.path().strip_prefix(&cwd).unwrap();
        let p = cwd.join(nest(rel_base, 240, "s.sock"));
        assert!(path::len(&p) > WIDE_PATH_MAX);
        let c = cfg(&[Path::new("/nonexistent")], true);
        let (l, r) = apple::run(BIND, &p, &c).unwrap();
        assert_eq!(r, Route::WideRelative);
        assert!(is_socket(&p));
        let (cl, r) = apple::run(CONNECT, &p, &c).unwrap();
        assert_eq!(r, Route::WideRelative);
        ping(l, cl);
    }

    // Route order 3c: over 253 bytes both ways goes through a symlink
    // `<base>/sockat.XXXXXXXXXX` to the socket's directory. With a short base the
    // path through the link fits the plain sockaddr (Symlink); with a base too
    // long for that it needs the widened form (SymlinkWide). Either way the
    // socket is created in the real directory (the link is to the directory,
    // not to the socket), and the base is empty afterwards: the link is
    // removed once the call returns.
    #[test]
    fn a_path_over_253_bytes_goes_through_a_temporary_symlink() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let short_base = tmp();
        let lb = tmp();
        let long_base = nest(lb.path(), 150, "base");
        std::fs::create_dir(&long_base).unwrap();
        for (base, want, name) in [
            (short_base.path(), Route::Symlink, "a.sock"),
            (long_base.as_path(), Route::SymlinkWide, "b.sock"),
        ] {
            let c = cfg(&[base], true);
            let p = p.with_file_name(name);
            let (l, r) = apple::run(BIND, &p, &c).unwrap();
            assert_eq!(r, want);
            assert!(is_socket(&p));
            let (cl, r) = apple::run(CONNECT, &p, &c).unwrap();
            assert_eq!(r, want);
            ping(l, cl);
            assert_empty(base);
        }
    }

    // The symlink route checks the length through the link before creating
    // anything: a base so long that even the widened form cannot carry
    // `<base>/sockat.XXXXXXXXXX/<name>` is skipped without touching the base, and
    // the fork route takes over.
    #[test]
    fn a_base_too_long_for_the_symlink_route_falls_back_to_fork() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let b = tmp();
        let base = nest(b.path(), 250, "base");
        std::fs::create_dir(&base).unwrap();
        let c = cfg(&[&base], true);
        let (l, r) = apple::run(BIND, &p, &c).unwrap();
        assert!(matches!(r, Route::Fork { .. }), "{r:?}");
        assert!(is_socket(&p));
        assert_empty(&base);
        let (cl, r) = apple::run(CONNECT, &p, &c).unwrap();
        assert!(matches!(r, Route::Fork { .. }), "{r:?}");
        ping(l, cl);
    }

    // When the kernel refuses the widened form (stood in for by
    // `wide: false`), the routes that need it are skipped: a path up to 253
    // bytes goes through the symlink with a short base (plain sockaddr), and
    // with a long base (which would need the widened form) or no usable base
    // the fork route takes over. This is the path of a future macOS that
    // stopped taking oversized addresses.
    #[test]
    fn without_the_widened_form_symlink_and_fork_take_over() {
        let t = tmp();
        let p = nest(t.path(), 200, "s.sock");
        let short_base = tmp();
        let b = tmp();
        let long_base = nest(b.path(), 150, "base");
        std::fs::create_dir(&long_base).unwrap();
        // `true`: the fork route, `false`: the symlink route.
        let cases: [(&[&Path], bool); 4] = [
            (&[short_base.path()], false),
            (&[long_base.as_path()], true),
            (&[Path::new("/nonexistent")], true),
            (&[], true),
        ];
        let way = |r: Route, forked: bool| {
            if forked {
                matches!(r, Route::Fork { .. })
            } else {
                r == Route::Symlink
            }
        };
        for (i, (base, forked)) in cases.into_iter().enumerate() {
            let p = p.with_file_name(format!("{i}.sock"));
            let c = cfg(base, false);
            let (l, r) = apple::run(BIND, &p, &c).unwrap();
            assert!(way(r, forked), "{base:?} {r:?}");
            assert!(is_socket(&p));
            let (cl, r) = apple::run(CONNECT, &p, &c).unwrap();
            assert!(way(r, forked), "{base:?} {r:?}");
            ping(l, cl);
        }
        assert_empty(short_base.path());
        assert_empty(&long_base);
    }

    // The fork route puts the file name in the plain sockaddr, and only a name
    // too long for that (over 103 bytes) in the widened one, when the kernel
    // takes it. Without the widened form such a name is ENAMETOOLONG.
    #[test]
    fn the_fork_route_widens_only_a_name_too_long_for_the_plain_sockaddr() {
        let t = tmp();
        let name = "n".repeat(150);
        let p = nest(t.path(), 600, &name);
        let (l, r) = apple::run(BIND, &p, &cfg(&[], true)).unwrap();
        assert!(matches!(r, Route::Fork { .. }), "{r:?}");
        assert!(is_socket(&p));
        let (c, r) = apple::run(CONNECT, &p, &cfg(&[], true)).unwrap();
        assert!(matches!(r, Route::Fork { .. }), "{r:?}");
        ping(l, c);

        let q = p.with_file_name("m".repeat(150));
        let e = apple::run(BIND, &q, &cfg(&[], false)).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
    }

    // A failure through the symlink is the kernel's error for the target
    // (EADDRINUSE for an existing socket, ENOENT for a missing directory, since
    // a link to a missing directory dangles), it is not retried on the fork
    // route, and the link is removed in the failure case too.
    #[test]
    fn symlink_route_failures_are_reported_and_cleaned_up() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let base = tmp();
        let c = cfg(&[base.path()], true);
        let (_l, _) = apple::run(BIND, &p, &c).unwrap();
        let e = apple::run(BIND, &p, &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::EADDRINUSE), "{e}");
        let missing = p.parent().unwrap().join("nope/s.sock");
        let e = apple::run(BIND, &missing, &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");
        let e = apple::run(CONNECT, &p.with_file_name("x.sock"), &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");
        assert_empty(base.path());
    }

    // The symlink name comes from a fresh random draw each time and symlink(2)
    // never replaces an entry, so two links made back to back are distinct, an
    // existing file under the base is never touched, and dropping the guard
    // removes only its own link.
    #[test]
    fn temporary_links_are_unique_and_removed_on_drop() {
        let base = tmp();
        let target = tmp();
        let keep = base.path().join("keep");
        std::fs::write(&keep, b"x").unwrap();
        let a = TempLink::create(base.path(), target.path()).unwrap();
        let b = TempLink::create(base.path(), target.path()).unwrap();
        assert_ne!(a.path, b.path);
        for l in [&a.path, &b.path] {
            let n = l.file_name().unwrap().to_string_lossy().into_owned();
            assert!(n.starts_with(LINK_PREFIX), "{n}");
            assert_eq!(n.len(), LINK_PREFIX.len() + LINK_RANDOM_LEN, "{n}");
            assert_eq!(std::fs::read_link(l).unwrap(), target.path());
        }
        drop(a);
        drop(b);
        let left: Vec<_> = std::fs::read_dir(base.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![OsString::from("keep")]);
    }

    // A name that is already taken (by anything: a file, another link) is
    // skipped for the next name, and the existing entry is left as it was:
    // symlink(2) fails with EEXIST rather than replacing it. The names come from
    // a stand-in generator here so the collision is certain.
    #[test]
    fn a_taken_symlink_name_is_skipped_for_the_next_one() {
        let base = tmp();
        let target = tmp();
        let taken = format!("{LINK_PREFIX}AAAAAAAAAA");
        let free = format!("{LINK_PREFIX}BBBBBBBBBB");
        std::fs::write(base.path().join(&taken), b"keep").unwrap();
        let mut names = vec![taken.clone(), free.clone()].into_iter();
        let mut calls = 0;
        let link = TempLink::create_named(base.path(), target.path(), || {
            calls += 1;
            names.next().unwrap()
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(link.path, base.path().join(&free));
        assert_eq!(std::fs::read_link(&link.path).unwrap(), target.path());
        assert_eq!(std::fs::read(base.path().join(&taken)).unwrap(), b"keep");
        drop(link);
        assert!(!base.path().join(&free).exists());
    }

    // When every name tried is taken, the attempt ends after a bounded number
    // of tries with AlreadyExists (the symlink route then counts as unusable
    // and the fork route takes over); it does not loop forever and touches
    // nothing.
    #[test]
    fn running_out_of_symlink_names_is_an_error_after_a_bounded_number_of_tries() {
        let base = tmp();
        let target = tmp();
        let taken = format!("{LINK_PREFIX}AAAAAAAAAA");
        std::fs::write(base.path().join(&taken), b"keep").unwrap();
        let mut calls = 0;
        let e = TempLink::create_named(base.path(), target.path(), || {
            calls += 1;
            taken.clone()
        })
        .err()
        .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(calls, TempLink::ATTEMPTS);
        assert_eq!(std::fs::read(base.path().join(&taken)).unwrap(), b"keep");
    }

    // The fork route opens the directory O_SEARCH on Apple, which needs only
    // search permission, like a plain bind / connect by path. A directory of
    // mode 0300 (write + search, no read) cannot be opened O_RDONLY, yet the
    // fork route binds and connects in it. Without O_SEARCH the long path would
    // fail where a short one in the same directory works.
    #[test]
    fn the_fork_route_does_not_need_read_permission_on_the_directory() {
        let t = tmp();
        let p = nest(t.path(), 400, "s.sock");
        let dir = p.parent().unwrap().to_path_buf();
        let _restore = unreadable(&dir);

        let c = cfg(&[], true);
        let (l, r) = apple::run(BIND, &p, &c).unwrap();
        assert!(matches!(r, Route::Fork { .. }), "{r:?}");
        assert!(is_socket(&p));
        let (cl, r) = apple::run(CONNECT, &p, &c).unwrap();
        assert!(matches!(r, Route::Fork { .. }), "{r:?}");
        ping(l, cl);
    }

    // A kernel that does not take O_SEARCH on a directory answers EISDIR (as
    // macOS does for O_EXEC alone) or EINVAL. The fork route must then still
    // work, by opening the directory O_RDONLY instead, rather than fail on a
    // directory it could use. The running kernel takes O_SEARCH, so its
    // refusal is stood in for by handing that answer to search_or_read_only,
    // and the fd it returns is used the way fork_op uses it. Checked:
    //
    // * EISDIR and EINVAL give an fd that the forked child fchdirs into, and
    //   the socket it binds is in that directory and accepts a connection made
    //   the same way;
    // * the fd really comes from O_RDONLY: a mode 0300 directory, which
    //   O_SEARCH opens, now fails with EACCES (and that is the error returned,
    //   not swallowed);
    // * any other error from the O_SEARCH open is the real outcome and is
    //   returned without trying O_RDONLY: an EACCES for a readable directory
    //   stays EACCES (falling back would hide a real permission failure).
    #[test]
    fn a_kernel_refusing_o_search_on_a_directory_falls_back_to_o_rdonly() {
        let refused = |code| Err(io::Error::from_raw_os_error(code));
        let fork_in = |op, dir: &OwnedFd, addr: &SockAddr| {
            let sock = sys::socket(false).unwrap();
            sys::op_in_dir_forked(op, sock.as_fd(), dir.as_fd(), addr)?;
            if op == Op::Bind {
                sys::listen(sock.as_fd()).unwrap();
            }
            io::Result::Ok(sock)
        };

        let t = tmp();
        let dir = t.path();
        for code in [libc::EISDIR, libc::EINVAL] {
            let name = format!("{code}.sock");
            let addr = SockAddr::standard(OsStr::new(&name)).unwrap();
            let fd = route::search_or_read_only(dir, refused(code)).unwrap();
            let l = fork_in(Op::Bind, &fd, &addr).unwrap();
            assert!(is_socket(&dir.join(&name)), "{code}");
            let c = fork_in(Op::Connect, &fd, &addr).unwrap();
            ping(l, c);
        }

        let e = route::search_or_read_only(dir, refused(libc::EACCES)).unwrap_err();
        assert_eq!(errno(&e), Some(libc::EACCES), "{e}");

        let locked = t.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        let _restore = unreadable(&locked);
        // The premise: the fork route as it is (O_SEARCH) binds there.
        let addr = SockAddr::standard(OsStr::new("s.sock")).unwrap();
        let _l = route::fork_op(BIND, &locked, &addr).unwrap().0;
        for code in [libc::EISDIR, libc::EINVAL] {
            let e = route::search_or_read_only(&locked, refused(code)).unwrap_err();
            assert_eq!(errno(&e), Some(libc::EACCES), "{code}: {e}");
        }
    }

    // A base the link cannot be created in (missing, or a file) makes the
    // symlink route unusable rather than an error: the next route is tried.
    #[test]
    fn an_unusable_base_skips_the_symlink_route() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let (dir, name) = path::split(&p).unwrap();
        let file = t.path().join("file");
        std::fs::write(&file, b"").unwrap();
        for base in [Path::new("/nonexistent"), file.as_path()] {
            let out = apple::symlink_route(BIND, base, dir, name, true);
            assert!(matches!(out, SymlinkOutcome::Unusable), "{base:?}");
        }
        assert!(!is_socket(&p));
    }

    // The places tried for the symlink are TMPDIR, then /tmp (kawaz's ruling:
    // a TMPDIR too long to shorten anything must not leave the route to the
    // widened form or fork while /tmp is there). An unset or empty TMPDIR
    // leaves /tmp alone. A relative TMPDIR would make the link's location
    // depend on the cwd, so it is left out, but /tmp is still tried. A TMPDIR
    // that is /tmp (written with or without the trailing slash) is tried once,
    // not twice. The widened form is always on outside tests.
    #[test]
    fn config_tries_tmpdir_then_tmp() {
        let bases =
            |tmpdir: Option<&str>| Config::from_tmpdir(tmpdir.map(OsString::from)).tmp_bases;
        let paths = |ps: &[&str]| ps.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(bases(None), paths(&["/tmp"]));
        assert_eq!(bases(Some("")), paths(&["/tmp"]));
        assert_eq!(bases(Some("/var/x/")), paths(&["/var/x/", "/tmp"]));
        assert_eq!(bases(Some("rel")), paths(&["/tmp"]));
        assert_eq!(bases(Some("/tmp")), paths(&["/tmp"]));
        assert_eq!(bases(Some("/tmp/")), paths(&["/tmp"]));
        assert!(Config::from_tmpdir(None).wide);
    }

    /// Set the modification time of `dir` to a fixed time in the past, so a
    /// later [`assert_untouched`] can tell whether anything was created in it
    /// or removed from it meanwhile (either updates a directory's mtime).
    /// [`assert_empty`] alone cannot: a symlink made and removed again leaves
    /// the directory empty.
    fn mark_untouched(dir: &Path) {
        let f = std::fs::File::open(dir).unwrap();
        f.set_modified(UNTOUCHED).unwrap();
        assert_untouched(dir);
    }

    const UNTOUCHED: std::time::SystemTime = std::time::SystemTime::UNIX_EPOCH;

    fn assert_untouched(dir: &Path) {
        let m = std::fs::metadata(dir).unwrap().modified().unwrap();
        assert_eq!(
            m, UNTOUCHED,
            "something was created in or removed from {dir:?}"
        );
    }

    // The symlink goes under the first place where `<place>/sockat.XXXXXXXXXX/
    // <name>` fits. A TMPDIR so long that even the widened form cannot carry
    // the path through it (the situation of a deeply nested TMPDIR) is passed
    // over without creating anything in it, and /tmp, the next place, is used:
    // through a short place the path fits the plain sockaddr (Symlink). The
    // second case is the same with both places short: the first one is used
    // and the second is never touched.
    #[test]
    fn the_symlink_goes_under_the_first_place_where_the_path_fits() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let b = tmp();
        let long_base = nest(b.path(), 250, "base");
        std::fs::create_dir(&long_base).unwrap();
        let first = tmp();
        let second = tmp();

        mark_untouched(&long_base);
        let c = cfg(&[&long_base, first.path()], true);
        let (l, r) = apple::run(BIND, &p, &c).unwrap();
        assert_eq!(r, Route::Symlink);
        assert!(is_socket(&p));
        let (cl, r) = apple::run(CONNECT, &p, &c).unwrap();
        assert_eq!(r, Route::Symlink);
        ping(l, cl);
        assert_untouched(&long_base);
        assert_empty(first.path());

        mark_untouched(second.path());
        let q = p.with_file_name("q.sock");
        let c = cfg(&[first.path(), second.path()], true);
        let (l, r) = apple::run(BIND, &q, &c).unwrap();
        assert_eq!(r, Route::Symlink);
        let (cl, _) = apple::run(CONNECT, &q, &c).unwrap();
        ping(l, cl);
        assert_untouched(second.path());
        assert_empty(first.path());
    }

    // A place where the symlink cannot be created (here: missing, or a file
    // rather than a directory) makes that place unusable, not the whole
    // route: the next place is tried, and the socket is at the given path.
    // With every place unusable the fork route takes over.
    #[test]
    fn a_place_where_the_symlink_cannot_be_created_moves_on_to_the_next() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let file = t.path().join("file");
        std::fs::write(&file, b"").unwrap();
        let good = tmp();
        let missing = Path::new("/nonexistent");
        for (i, bad) in [missing, file.as_path()].into_iter().enumerate() {
            let p = p.with_file_name(format!("{i}.sock"));
            let c = cfg(&[bad, good.path()], true);
            let (l, r) = apple::run(BIND, &p, &c).unwrap();
            assert_eq!(r, Route::Symlink, "{bad:?}");
            assert!(is_socket(&p));
            let (cl, r) = apple::run(CONNECT, &p, &c).unwrap();
            assert_eq!(r, Route::Symlink);
            ping(l, cl);
        }
        assert_empty(good.path());

        let p = p.with_file_name("fork.sock");
        let c = cfg(&[missing, file.as_path()], true);
        let (l, r) = apple::run(BIND, &p, &c).unwrap();
        assert!(matches!(r, Route::Fork { .. }), "{r:?}");
        let (cl, _) = apple::run(CONNECT, &p, &c).unwrap();
        ping(l, cl);
    }

    // A bind / connect that actually ran through the symlink under the first
    // place is the result: its error is returned and the call is not made
    // again under the next place (which would repeat it, or worse, succeed
    // somewhere the caller did not mean). The second place must stay
    // untouched (no link was even created there), for EADDRINUSE, a missing
    // directory (ENOENT through a dangling link) and a missing socket.
    #[test]
    fn a_failure_through_the_symlink_is_not_retried_under_the_next_place() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let first = tmp();
        let second = tmp();
        let c = cfg(&[first.path(), second.path()], true);
        let (_l, r) = apple::run(BIND, &p, &c).unwrap();
        assert_eq!(r, Route::Symlink);
        mark_untouched(second.path());

        let e = apple::run(BIND, &p, &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::EADDRINUSE), "{e}");
        let missing = p.parent().unwrap().join("nope/s.sock");
        let e = apple::run(BIND, &missing, &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");
        let e = apple::run(CONNECT, &p.with_file_name("x.sock"), &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");

        assert_untouched(second.path());
        assert_empty(first.path());
    }

    // The same through the real environment (Config::from_env, as the public
    // API reads it): with TMPDIR set to a directory too long for the symlink,
    // a 600-byte path still goes through a symlink, under /tmp, and nothing is
    // created in TMPDIR. Runs in a child process because it sets TMPDIR (the
    // cwd is / so the relative spelling is long too).
    #[test]
    fn a_tmpdir_too_long_for_the_symlink_falls_back_to_tmp() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let b = tmp();
        let long_tmpdir = nest(b.path(), 250, "tmpdir");
        std::fs::create_dir(&long_tmpdir).unwrap();
        mark_untouched(&long_tmpdir);
        let out = run_child(
            "tests::apple::child_bind_from_env",
            Path::new("/"),
            &[
                ("TMPDIR", long_tmpdir.as_os_str()),
                ("SOCKAT_CHILD_PATH", p.as_os_str()),
            ],
        );
        assert!(out.contains("sockat-child: Symlink"), "{out}");
        assert!(is_socket(&p));
        assert_untouched(&long_tmpdir);
    }

    #[test]
    fn child_bind_from_env() {
        if !in_child() {
            return;
        }
        let p = env_path("SOCKAT_CHILD_PATH");
        let (_l, route) = route::run(BIND, &p).unwrap();
        println!("sockat-child: {route:?}");
    }

    // The fork route's connect is made by the child, so the server's
    // LOCAL_PEERPID names the child at first. XNU updates it when the real
    // client sends, after which it names the client process. Documented in
    // the crate docs; pinned so a change in XNU's behaviour is noticed. The
    // relative / widened routes connect from this process directly.
    #[test]
    fn local_peerpid_of_a_forked_connect_names_the_child_until_the_client_sends() {
        let me = std::process::id() as libc::pid_t;
        let t = tmp();
        let p = nest(t.path(), 400, "s.sock");
        let (dir, name) = path::split(&p).unwrap();
        let addr = SockAddr::standard(name).unwrap();
        let l = UnixListener::from(route::fork_op(BIND, dir, &addr).unwrap().0);
        let mut c = UnixStream::from(route::fork_op(CONNECT, dir, &addr).unwrap().0);
        let (mut s, _) = l.accept().unwrap();
        let before = sys::local_peer_pid(s.as_fd()).unwrap();
        assert_ne!(before, me);
        c.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        s.read_exact(&mut b).unwrap();
        assert_eq!(sys::local_peer_pid(s.as_fd()).unwrap(), me);

        // The widened route connects from this process itself.
        let wp = nest(t.path(), 200, "w.sock");
        let wl = UnixListener::from(apple::wide_op(BIND, &wp).unwrap());
        let _wc = apple::wide_op(CONNECT, &wp).unwrap();
        let (ws, _) = wl.accept().unwrap();
        assert_eq!(sys::local_peer_pid(ws.as_fd()).unwrap(), me);
    }

    // ----- connect options (nonblocking, the child's pid, no fork) -----

    /// Places and directories one [`ways`] call works in, removed at the end of
    /// the test.
    struct Dirs {
        t: tempfile::TempDir,
        below_cwd: tempfile::TempDir,
        short_base: tempfile::TempDir,
        long_base_dir: tempfile::TempDir,
    }

    impl Dirs {
        fn new() -> Self {
            Self {
                t: tmp(),
                below_cwd: tmp_below_cwd(),
                short_base: tmp(),
                long_base_dir: tmp(),
            }
        }
    }

    /// The seven Apple routes, each a [`Way`] to its own socket path, with the
    /// route each must take: the path itself (100 bytes); the relative
    /// spelling of a path below the cwd (99 bytes relative); the widened
    /// sockaddr (200 bytes under /tmp); the widened sockaddr with the relative
    /// spelling (240 bytes relative, over 253 absolute); the symlink under a
    /// short place (plain sockaddr) and under a 150-byte place (widened); and
    /// the fork route (600 bytes, no place for a symlink). Same inputs as the
    /// route-order tests above use to force each route.
    fn ways(d: &Dirs) -> Vec<(&'static str, Way, Want)> {
        let t = d.t.path();
        let cwd = std::env::current_dir().unwrap();
        let rel_base = d.below_cwd.path().strip_prefix(&cwd).unwrap();
        let long_base = nest(d.long_base_dir.path(), 150, "base");
        std::fs::create_dir_all(&long_base).unwrap();
        let none = Path::new("/nonexistent");
        let cases: Vec<(&'static str, PathBuf, Config, Want)> = vec![
            (
                "Direct",
                nest(t, 100, "direct.sock"),
                cfg(&[none], true),
                |r| r == Route::Direct,
            ),
            (
                "Relative",
                cwd.join(nest(rel_base, 99, "rel.sock")),
                cfg(&[none], true),
                |r| r == Route::Relative,
            ),
            ("Wide", nest(t, 200, "wide.sock"), cfg(&[none], true), |r| {
                r == Route::Wide
            }),
            (
                "WideRelative",
                cwd.join(nest(rel_base, 240, "wrel.sock")),
                cfg(&[none], true),
                |r| r == Route::WideRelative,
            ),
            (
                "Symlink",
                nest(t, 600, "sym.sock"),
                cfg(&[d.short_base.path()], true),
                |r| r == Route::Symlink,
            ),
            (
                "SymlinkWide",
                nest(t, 600, "symw.sock"),
                cfg(&[&long_base], true),
                |r| r == Route::SymlinkWide,
            ),
            ("Fork", nest(t, 600, "fork.sock"), cfg(&[], true), |r| {
                matches!(r, Route::Fork { .. })
            }),
        ];
        cases
            .into_iter()
            .map(|(what, p, c, want)| {
                let way: Way = std::sync::Arc::new(move |req| apple::run(req, &p, &c));
                (what, way, want)
            })
            .collect()
    }

    // A nonblocking connect must come back at once and stay O_NONBLOCK on
    // every route. On macOS a full backlog already fails at once with
    // ECONNREFUSED for a blocking connect too, so what differs is the socket
    // that comes back; the routes that hand a plain sockaddr to std (the path
    // itself, the relative spelling, the symlink) must use sockat's own
    // socket for it (std cannot connect a nonblocking socket), and the fork
    // route must make the parent's socket O_NONBLOCK before the fork. See
    // check_nonblocking_connect for what is checked on each.
    #[test]
    fn a_nonblocking_connect_fails_at_once_on_a_full_backlog_on_every_route() {
        let d = Dirs::new();
        for (what, way, want) in ways(&d) {
            check_nonblocking_connect(&way, want, what);
        }
        assert_empty(d.short_base.path());
    }

    // DR-0001 decision 8: the connect reports the pid the server will see as
    // the peer when that is not this process. Only the fork route connects
    // from another process (the child; LOCAL_PEERPID names it until this
    // process sends); every other route connects from this process and
    // reports no pid.
    #[test]
    fn only_the_fork_route_reports_the_childs_pid_and_it_is_the_servers_peer() {
        let d = Dirs::new();
        for (what, way, _) in ways(&d) {
            check_child_pid(&way, what == "Fork", what);
        }
    }

    // With forking ruled out, a path only the fork route reaches (600 bytes,
    // so neither relative nor widened; the only place for a symlink too long
    // for the path through it) fails with ENAMETOOLONG, the error of a plain
    // connect to a path too long for sun_path, and nothing happens: no
    // connection reaches the listener (its accept would block), nothing is
    // created in the place for the symlink (its mtime is unchanged) or next to
    // the socket. Blocking or not makes no difference. The other routes are
    // not affected: with a short place the same path connects through the
    // symlink. A real failure stays what it is: a missing directory is ENOENT
    // (opening it comes before the fork route is chosen).
    #[test]
    fn without_fork_a_path_that_needs_it_is_enametoolong_and_nothing_is_made() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let b = tmp();
        let long_base = nest(b.path(), 250, "base");
        std::fs::create_dir(&long_base).unwrap();
        let short_base = tmp();
        let (l, _) = apple::run(BIND, &p, &cfg(&[], true)).unwrap();
        let listener = UnixListener::from(l);
        listener.set_nonblocking(true).unwrap();
        let dir = p.parent().unwrap();
        let list = || -> Vec<_> {
            std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect()
        };
        let before = list();
        mark_untouched(&long_base);
        let no_place = cfg(&[&long_base], true);
        for nonblocking in [false, true] {
            let req = Request::Connect(ConnectOptions {
                nonblocking,
                fork: false,
            });
            let e = apple::run(req, &p, &no_place).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{nonblocking}: {e}");
            let e = listener.accept().unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "a connection got in");

            let missing = dir.join("nope").join("s.sock");
            let e = apple::run(req, &missing, &cfg(&[], true)).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENOENT), "{nonblocking}: {e}");

            let (_c, r) = apple::run(req, &p, &cfg(&[short_base.path()], true)).unwrap();
            assert_eq!(r, Route::Symlink);
            drop(listener.accept().unwrap());
        }
        assert_untouched(&long_base);
        assert_eq!(before, list());
        assert_empty(short_base.path());
    }

    // ----- the relative spelling (route 3a / 3b) -----
    //
    // These run in a child process so the cwd (and $PWD) can be chosen. The
    // child binds through `apple::run`, prints the route and the spelling, and
    // the parent checks where the socket appeared.

    /// Bind `path` in a child whose cwd is `cwd` and whose `$PWD` is `pwd`;
    /// return (route, relative spelling or "-") as the child printed them.
    fn bind_in_child(cwd: &Path, pwd: &Path, path: &Path) -> (String, String) {
        let out = run_child(
            "tests::apple::child_bind",
            cwd,
            &[
                ("PWD", pwd.as_os_str()),
                ("SOCKAT_CHILD_PATH", path.as_os_str()),
            ],
        );
        let line = out
            .lines()
            .find_map(|l| l.split_once("sockat-child: ").map(|(_, r)| r))
            .unwrap_or_else(|| panic!("no report\n{out}"));
        let (route, rel) = line.split_once(' ').unwrap();
        (route.to_owned(), rel.to_owned())
    }

    #[test]
    fn child_bind() {
        if !in_child() {
            return;
        }
        let p = env_path("SOCKAT_CHILD_PATH");
        let (dir, _) = path::split(&p).unwrap();
        let rel = Relative::new(&p, dir)
            .map(|r| r.path.display().to_string())
            .unwrap_or_else(|| "-".into());
        let (_l, route) = apple::run(BIND, &p, &cfg(&[], true)).unwrap();
        println!("sockat-child: {route:?} {rel}");
    }

    /// A pair of sibling directories, one reached through a symlink, for the
    /// cwd tests: `<t>/real/` and `<t>/link -> real`.
    fn real_and_link(t: &Path) -> (PathBuf, PathBuf) {
        let real = t.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = t.join("link");
        std::os::unix::fs::symlink("real", &link).unwrap();
        (real, link)
    }

    // $PWD reached through a symlink, and the path written through the same
    // symlink: the path starts with $PWD, so the prefix is cut off and the
    // spelling only descends (no `..`, no realpath). getcwd would not have
    // matched: it is the real path (/private/tmp/.../real). The socket is in
    // the real directory.
    #[test]
    fn a_path_below_pwd_through_a_symlink_is_cut_at_pwd() {
        let t = tmp();
        let (real, link) = real_and_link(t.path());
        // /tmp itself is a symlink to /private/tmp on macOS, so getcwd differs
        // from "<t>/link" in two places.
        let p = nest_tail(&link, 95, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let rel_want = p.strip_prefix(&link).unwrap().to_path_buf();
        let (route, rel) = bind_in_child(&link, &link, &p);
        assert_eq!(route, "Relative");
        assert_eq!(Path::new(&rel), rel_want);
        assert!(is_socket(&real.join(&rel_want)));
    }

    // The cwd is reached through a symlink and $PWD is not set to it (here: a
    // $PWD naming another directory). The path is written through the symlink,
    // so it starts with neither getcwd nor a usable $PWD; both sides are
    // resolved to real paths instead and the spelling descends from the real
    // cwd. The /tmp -> /private/tmp symlink alone would defeat a comparison of
    // the paths as written.
    #[test]
    fn a_cwd_through_a_symlink_is_matched_by_real_paths() {
        let t = tmp();
        let (real, link) = real_and_link(t.path());
        let p = nest_tail(&link, 95, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let rel_want = p.strip_prefix(&link).unwrap().to_path_buf();
        let (route, rel) = bind_in_child(&link, Path::new("/"), &p);
        assert_eq!(route, "Relative");
        assert_eq!(Path::new(&rel), rel_want);
        assert!(is_socket(&real.join(&rel_want)));
    }

    // A symlink in the middle of the path, below the cwd: `<cwd>/link/...`
    // where link -> real. Cutting the cwd off gives "link/..." (descends through
    // the link); resolving gives "real/..." (same length here); either names
    // the socket in the real directory. The point is the place, not the
    // spelling.
    #[test]
    fn a_symlink_in_the_middle_of_the_path_still_lands_in_the_real_directory() {
        let t = tmp();
        let (real, link) = real_and_link(t.path());
        let p = nest_tail(&link, 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let below = p.strip_prefix(&link).unwrap().to_path_buf();
        let (route, rel) = bind_in_child(t.path(), t.path(), &p);
        assert_eq!(route, "Relative");
        assert!(!has_dotdot(Path::new(&rel)), "{rel}");
        assert!(is_socket(&real.join(&below)));
    }

    // The path is in a sibling of the cwd, so the spelling climbs with `..`:
    // cwd <t>/a/b, path <t>/c/... gives "../../c/...". Both sides are real
    // paths, so the climb goes out of the real cwd. The socket is in <t>/c.
    #[test]
    fn a_path_beside_the_cwd_is_spelled_with_dotdot() {
        let t = tmp();
        let cwd = t.path().join("a/b");
        std::fs::create_dir_all(&cwd).unwrap();
        let c = t.path().join("c");
        let p = nest_tail(&c, 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let (route, rel) = bind_in_child(&cwd, &cwd, &p);
        assert_eq!(route, "Relative");
        assert!(rel.starts_with("../../c/"), "{rel}");
        assert!(is_socket(&p));
    }

    // A $PWD that names another directory than the cwd must not be used: cut
    // off the path, it would spell the right subpath relative to the wrong
    // directory. Both directories have the same subtree here, so using $PWD
    // would silently create the socket in the cwd's copy. The socket must be in
    // the $PWD directory (where the path points), reached by climbing out of
    // the real cwd.
    #[test]
    fn a_pwd_that_is_not_the_cwd_is_not_used() {
        let t = tmp();
        let cwd = t.path().join("one");
        let pwd = t.path().join("two");
        let p = nest_tail(&pwd, 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let sub = p.strip_prefix(&pwd).unwrap().to_path_buf();
        std::fs::create_dir_all(cwd.join(sub.parent().unwrap())).unwrap();
        let (route, rel) = bind_in_child(&cwd, &pwd, &p);
        assert_eq!(route, "Relative");
        assert!(rel.starts_with("../two/"), "{rel}");
        assert!(is_socket(&p));
        assert!(!is_socket(&cwd.join(&sub)));
    }

    // A cwd whose name is a byte prefix of the path's next component (cwd
    // <t>/ab, path <t>/abc/...) is not a prefix of the path: cutting bytes would
    // spell "c/..." and name <t>/ab/c/.... The spelling climbs instead.
    #[test]
    fn a_cwd_that_is_only_a_byte_prefix_is_not_cut_off() {
        let t = tmp();
        let cwd = t.path().join("ab");
        std::fs::create_dir(&cwd).unwrap();
        let p = nest_tail(&t.path().join("abc"), 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let (route, rel) = bind_in_child(&cwd, &cwd, &p);
        assert_eq!(route, "Relative");
        assert!(rel.starts_with("../abc/"), "{rel}");
        assert!(is_socket(&p));
        assert!(!cwd.join("c").exists());
    }

    // The relative spelling is resolved against the cwd at the moment of the
    // bind / connect, and the cwd is shared by every thread, so another thread
    // that changes it in between sends the call to another directory. When the
    // cwd is still changed afterwards, Relative::run must notice and turn the
    // result into an error even though the call itself succeeded: a listener
    // in the wrong directory or a stream to the wrong server must not reach
    // the caller as if it were at the path they gave.
    //
    // The change is made from inside the call (the closure chdirs before it
    // binds / connects), standing in for another thread. Two directories with
    // the same subtree make the wrong directory one where the call succeeds:
    //
    // * bind: the error comes back, the socket file is in the other directory
    //   and is left there (where it went is not known to Relative::run, so it
    //   must not remove anything), and nothing appears at the given path;
    // * connect: the error comes back, and the stream that did connect to a
    //   listener in the other directory is closed, which that listener sees as
    //   end of file.
    //
    // Runs in a child process because it changes the cwd.
    #[test]
    fn a_cwd_changed_during_a_relative_bind_or_connect_discards_the_result() {
        let t = tmp();
        let one = t.path().join("one");
        let two = t.path().join("two");
        let p = nest_tail(&one, 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let sub = p.strip_prefix(&one).unwrap().to_path_buf();
        std::fs::create_dir_all(two.join(sub.parent().unwrap())).unwrap();
        run_child(
            "tests::apple::child_relative_with_the_cwd_changed_midway",
            &one,
            &[
                ("PWD", one.as_os_str()),
                ("SOCKAT_CHILD_PATH", p.as_os_str()),
                ("SOCKAT_CHILD_OTHER", two.as_os_str()),
            ],
        );
        assert!(!is_socket(&p));
        assert!(is_socket(&two.join(&sub)));
    }

    #[test]
    fn child_relative_with_the_cwd_changed_midway() {
        if !in_child() {
            return;
        }
        let p = env_path("SOCKAT_CHILD_PATH");
        let one = std::env::current_dir().unwrap();
        let two = env_path("SOCKAT_CHILD_OTHER");
        let (dir, _) = path::split(&p).unwrap();
        let rel = Relative::new(&p, dir).unwrap();
        assert!(!has_dotdot(&rel.path), "{:?}", rel.path);

        let e = rel
            .run(|q| {
                std::env::set_current_dir(&two).unwrap();
                route::plain_op(BIND, q)
            })
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Other, "{e}");
        assert_eq!(errno(&e), None, "{e}");
        assert!(is_socket(&two.join(&rel.path)));
        assert!(!is_socket(&p));

        // A listener in the other directory, at the same relative spelling.
        let c = p.with_file_name("c.sock");
        let (c_dir, _) = path::split(&c).unwrap();
        let c_rel = rel.path.with_file_name("c.sock");
        let listener = UnixListener::bind(&c_rel).unwrap();
        assert!(is_socket(&two.join(&c_rel)));
        std::env::set_current_dir(&one).unwrap();
        let rel = Relative::new(&c, c_dir).unwrap();
        assert_eq!(rel.path, c_rel);

        let e = rel
            .run(|q| {
                std::env::set_current_dir(&two).unwrap();
                route::plain_op(CONNECT, q)
            })
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Other, "{e}");
        let (mut server, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(server.read(&mut buf).unwrap(), 0, "the client is closed");
    }
}
