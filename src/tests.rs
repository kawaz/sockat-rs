//! Route-level tests: each way of reaching a long path is forced and checked
//! for where the socket file ends up, that data flows both ways, what is left
//! behind, and how failures come out. The public API is tested separately in
//! `tests/`.
//!
//! What every test here leans on:
//!
//! * The socket must appear at the path the caller gave (checked with
//!   `symlink_metadata(...).file_type().is_socket()`), not at whatever spelling
//!   was handed to the kernel. A route that "works" but creates the socket
//!   elsewhere is the failure these tests exist to catch.
//! * Which route is taken is forced through the route order's inputs
//!   ([`Config`]: the places for sockat's own directory D, the places for a
//!   connect's symlink, where /proc is, whether the thread-cwd means and the
//!   widened sockaddr may be used, how the socket's directory and D are
//!   opened), never through the process environment.
//! * Temporary directories have random names and are removed when the test
//!   ends (`tempfile::TempDir`).
//! * Tests that need a particular working directory or a process-wide setting
//!   (SIGCHLD disposition, umask) run in a child process of this test binary
//!   ([`run_child`]); the test runner is multi-threaded, so changing them
//!   in-process would disturb the other tests.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::path;
use crate::route::{
    self, BindOptions, Config, ConnectOptions, DIRECT_MAX, Place, Request, Route, own,
};
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
#[cfg(not(target_os = "freebsd"))]
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
#[cfg(not(target_os = "freebsd"))]
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

/// [`ping`] through a duplicate of `listener`, which stays open.
fn ping_kept(listener: &UnixListener, client: OwnedFd) {
    ping(OwnedFd::from(listener.try_clone().unwrap()), client);
}

/// A bind: what `sockat::bind` does.
const BIND: Request = Request::Bind(BindOptions { last_resort: false });

/// A bind that may take the last resort, symlink-dir: what
/// `sockat::bind_ex(path, LastResort::Allow)` does.
const BIND_LAST: Request = Request::Bind(BindOptions { last_resort: true });

/// A blocking connect that may take the last resort, fork-cwd-direct: what
/// `sockat::connect_ex(path, LastResort::Allow)` does.
const CONNECT_LAST: Request = Request::Connect(ConnectOptions {
    nonblocking: false,
    last_resort: true,
});

/// A blocking connect that does not fork: what `sockat::connect` does.
const CONNECT: Request = Request::Connect(ConnectOptions {
    nonblocking: false,
    last_resort: false,
});

/// A nonblocking connect that does not fork: what
/// `sockat::connect_nonblocking` does.
const CONNECT_NB: Request = Request::Connect(ConnectOptions {
    nonblocking: true,
    last_resort: false,
});

/// A nonblocking connect that may take the last resort: what
/// `sockat::connect_nonblocking_ex(path, LastResort::Allow)` does.
const CONNECT_NB_LAST: Request = Request::Connect(ConnectOptions {
    nonblocking: true,
    last_resort: true,
});

/// The four connects.
const CONNECTS: [Request; 4] = [CONNECT, CONNECT_NB, CONNECT_LAST, CONNECT_NB_LAST];

/// `p` as a place for D.
fn at(p: &Path) -> Place {
    Place::Path(p.to_path_buf())
}

/// The route order's inputs as the public API reads them, except that D may
/// be made in `places` (in that order; the socket's directory only when it is
/// among them as [`Place::SocketDir`]) and a connect's symlink may go in
/// `links` (not on FreeBSD, which has no such route). The widened sockaddr is
/// on (Apple), /proc is looked for where it is (Linux), the thread-cwd means
/// is on (Linux, Apple). A bind's symlink-file, where the bind's order has
/// it, is off ([`with_symlink_file_bind`]).
fn config(places: &[Place], links: &[&Path]) -> Config {
    let mut c = Config::from_vars(None, None);
    c.own_dir_places = places.to_vec();
    #[cfg(not(target_os = "freebsd"))]
    {
        c.file_link_places = links.iter().map(|b| b.to_path_buf()).collect();
    }
    #[cfg(target_os = "freebsd")]
    let _ = links;
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "illumos"
    )))]
    {
        c.symlink_file_bind = false;
    }
    c
}

/// `c` with a bind's symlink-file on, as outside the tests: [`config`] turns
/// it off so a bind goes on to the routes through D. Only where the bind's
/// order has the route: not on the OSes whose bind was measured not to follow
/// a symlink (Linux / Android, FreeBSD, OpenBSD, illumos).
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "illumos"
)))]
fn with_symlink_file_bind(c: Config) -> Config {
    Config {
        symlink_file_bind: true,
        ..c
    }
}

/// `c` with the thread-cwd means turned off, standing in for a system where it
/// cannot be used (a seccomp filter in effect, as under docker's default
/// profile; `unshare` refused; no `pthread_fchdir_np`), so that the order
/// goes on to its next route. On the targets without it, `c` as it is.
fn no_thread_cwd(c: Config) -> Config {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    return Config {
        thread_cwd: false,
        ..c
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    return c;
}

/// `c` with every means through D that comes before the fork turned off: the thread-cwd means ([`no_thread_cwd`]) and, on Linux, /proc
/// (looked for where nothing is), so that a bind, with D or without it, goes
/// on to the fork route. FreeBSD's bindat cannot turn out unusable and stays.
fn fork_means_only(c: Config) -> Config {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    return Config {
        proc_self_fd: PathBuf::from("/nonexistent/proc/self/fd"),
        ..no_thread_cwd(c)
    };
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    return no_thread_cwd(c);
}

/// `c` with the connect routes that need neither writing nor a thread
/// unusable, so that a connect goes on to the routes through D (SPEC "connect
/// の順"): the socket's directory cannot be opened ([`refuse_socket_dir`]: no
/// proc-direct, at-direct or fork-cwd-direct) and the widened sockaddr is off
/// (Apple). relative is left to the path: one hundreds of bytes long under
/// `/tmp` is too far from the cwd for it.
fn no_direct(c: Config) -> Config {
    #[allow(unused_mut)]
    let mut c = Config {
        open_socket_dir: refuse_socket_dir,
        ..c
    };
    #[cfg(target_vendor = "apple")]
    {
        c.wide = false;
    }
    c
}

/// Why the thread-cwd means cannot be used in this environment, or `None`
/// when it can: one bind through it, in a directory of its own.
///
/// The means depends on the system, not only on the target: on Linux it is
/// not used while a seccomp filter is in effect (docker's default profile
/// installs one) and is used elsewhere, and a future Apple libSystem may lack
/// `pthread_fchdir_np`. The tests that run it unforced read this to know which
/// route the order must have taken.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn thread_cwd_refusal() -> Option<io::Error> {
    let t = tmp();
    let addr = SockAddr::standard(OsStr::new("probe.sock")).unwrap();
    match route::thread_cwd_op(BIND, t.path(), &addr) {
        sys::OnThread::Done(done) => {
            done.unwrap();
            assert!(is_socket(&t.path().join("probe.sock")));
            None
        }
        sys::OnThread::Unusable(e) => Some(e),
    }
}

/// Whether the thread-cwd means can be used on this system, saying which on
/// stderr (so a run shows the branch its tests took). The answer must be the
/// one expected, not just any failure of the means:
///
/// * Linux: usable exactly when this thread's seccomp mode reads 0 (the
///   started thread inherits this thread's filters). With a filter, or a mode
///   that cannot be read, the means must not even have called `unshare`, so
///   its refusal carries no errno: a filter may kill the process for that
///   call (SIGSYS) instead of answering it. The route order's own check
///   before making anything ([`sys::thread_cwd_available`]) must agree;
/// * macOS: always usable, its libSystem having `pthread_fchdir_np`.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn thread_cwd_usable() -> bool {
    let refusal = thread_cwd_refusal();
    assert_eq!(
        sys::thread_cwd_available(),
        refusal.is_none(),
        "{refusal:?}"
    );
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let filtered = sys::seccomp_mode().ok() != Some(0);
        assert_eq!(
            refusal.is_some(),
            filtered,
            "seccomp filter in effect: {filtered}, refusal: {refusal:?}"
        );
    }
    let Some(e) = refusal else {
        eprintln!("the thread-cwd means is usable here");
        return true;
    };
    #[cfg(target_os = "macos")]
    panic!("macOS has pthread_fchdir_np, yet the thread-cwd means is refused: {e}");
    #[cfg(not(target_os = "macos"))]
    {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            assert_eq!(e.kind(), io::ErrorKind::Unsupported, "{e}");
            assert_eq!(errno(&e), None, "unshare was called: {e}");
        }
        eprintln!("the thread-cwd means is not used here: {e}");
        false
    }
}

/// Whether the thread-cwd means can be used here ([`thread_cwd_usable`]);
/// `false` on the targets without it.
fn thread_here() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    return thread_cwd_usable();
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    return false;
}

/// Whether `r` is the route through D that `req` takes with the default means
/// of the platform and a short file name (a connect with the routes before
/// D off, [`no_direct`]): the thread-cwd means where it is usable (`thread`);
/// else on Linux the /proc version, on FreeBSD bindat / connectat, and
/// elsewhere a forked child in D for a bind (a connect has no route through D
/// left there, [`connects_through_d`]).
fn route_through_d(req: Request, thread: bool, r: Route) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    if thread {
        return r == Route::ThreadCwd;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    return {
        let _ = req;
        r == Route::Proc
    };
    #[cfg(target_os = "freebsd")]
    return {
        let _ = (req, thread);
        r == Route::At
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    return {
        let _ = thread;
        req == BIND && r.forked()
    };
}

/// Whether `r` is the route a bind takes without D (D made nowhere) with the
/// default means of the platform and a short file name: as [`route_through_d`],
/// the `-sockdir` one.
fn sockdir_route(thread: bool, r: Route) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    if thread {
        return r == Route::ThreadCwdSockdir;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    return r == Route::ProcSockdir;
    #[cfg(target_os = "freebsd")]
    return {
        let _ = thread;
        r == Route::AtSockdir
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    return {
        let _ = thread;
        matches!(r, Route::ForkCwdSockdir { .. })
    };
}

/// Whether the platform has a connect route through D here: thread-cwd
/// (`thread`), on Linux also proc, on FreeBSD at. NetBSD, OpenBSD, illumos
/// and the other Unix targets have none, and their connects take only the
/// routes that need no D (relative, symlink).
fn connects_through_d(thread: bool) -> bool {
    if cfg!(target_os = "freebsd") {
        return true;
    }
    if cfg!(any(target_os = "linux", target_os = "android")) {
        return thread || Path::new("/proc/self/fd").exists();
    }
    cfg!(target_vendor = "apple") && thread
}

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

/// The entries of `dir`, which must be none.
fn assert_empty(dir: &Path) {
    let left: Vec<_> = std::fs::read_dir(dir).unwrap().collect();
    assert!(left.is_empty(), "left behind in {dir:?}: {left:?}");
}

/// The names in `dir` that start with `prefix`, which must be none: what
/// sockat makes beside the user's files (D or R named `.sockat.*`).
fn assert_none_named(dir: &Path, prefix: &str) {
    let left: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().starts_with(prefix))
        .collect();
    assert!(left.is_empty(), "left behind in {dir:?}: {left:?}");
}

/// Set the modification time of `dir` to a fixed time in the past, so a
/// later [`assert_untouched`] can tell whether anything was created in it
/// or removed from it meanwhile (either updates a directory's mtime), and
/// [`assert_touched`] that something was. [`assert_empty`] alone cannot: a
/// directory made and removed again leaves the place empty.
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

fn assert_touched(dir: &Path) {
    let m = std::fs::metadata(dir).unwrap().modified().unwrap();
    assert_ne!(m, UNTOUCHED, "nothing was created in {dir:?}");
}

/// What `listener`'s `local_addr()` reports: the spelling the kernel keeps for
/// the bound socket.
fn reported(listener: &UnixListener) -> PathBuf {
    listener
        .local_addr()
        .unwrap()
        .as_pathname()
        .unwrap()
        .to_path_buf()
}

/// Whether `c` is a name sockat draws: `prefix` and 10 letters and digits.
fn is_drawn(c: &OsStr, prefix: &str) -> bool {
    let c = c.to_string_lossy();
    c.strip_prefix(prefix)
        .is_some_and(|r| r.len() == own::RANDOM_LEN && r.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Whether `spelling` is `<R>/<name>`, R a link name sockat draws with
/// `prefix` (none for R in D, `.sockat.` beside the socket).
fn through_link(spelling: &Path, prefix: &str, name: &str) -> bool {
    let parts: Vec<_> = spelling.iter().collect();
    parts.len() == 2 && is_drawn(parts[0], prefix) && parts[1] == name
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

// ---------------------------------------------------------------------------
// every platform
// ---------------------------------------------------------------------------

// sockat's own sockets are set up like std's: where std sets SO_NOSIGPIPE on
// its Unix sockets (Apple, FreeBSD, NetBSD, DragonFly), so does sockat, so a
// write to a closed peer returns EPIPE instead of raising SIGPIPE on every
// route. Checked against std's own socket on the same system rather than
// against a fixed answer, so the promise is "the same as std".
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
#[test]
fn sockets_carry_so_nosigpipe_where_std_sets_it() {
    use std::os::fd::AsFd;
    let t = tmp();
    let short = t.path().join("std.sock");
    let std_listener = UnixListener::bind(&short).unwrap();
    let std_client = UnixStream::connect(&short).unwrap();
    let std_says = sys::so_nosigpipe(std_client.as_fd()).unwrap();
    assert_eq!(sys::so_nosigpipe(std_listener.as_fd()).unwrap(), std_says);
    assert!(std_says, "std sets SO_NOSIGPIPE on this system");

    let long = nest(t.path(), 300, "s.sock");
    let listener = crate::bind(&long).unwrap();
    assert_eq!(sys::so_nosigpipe(listener.as_fd()).unwrap(), std_says);
    let client = crate::connect(&long).unwrap();
    assert_eq!(sys::so_nosigpipe(client.as_fd()).unwrap(), std_says);
    let client = crate::connect_nonblocking(&long).unwrap();
    assert_eq!(sys::so_nosigpipe(client.as_fd()).unwrap(), std_says);
}

// The plain route is the contract that every path std accepts behaves exactly
// like std: at most DIRECT_MAX bytes goes to std unchanged, one byte more does
// not. DIRECT_MAX is sun_path less the NUL (107 on Linux, Android and illumos,
// 103 on Apple and the BSDs), which is std's own limit: std binds and connects
// a path of DIRECT_MAX bytes and refuses one byte more with InvalidInput
// (checked here against std itself). So no path std can take is sent down
// another route, and none that std refuses is handed to it.
#[test]
fn a_path_std_accepts_goes_to_std_unchanged_and_one_byte_more_does_not() {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "illumos"))]
    assert_eq!(DIRECT_MAX, 107);
    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    assert_eq!(DIRECT_MAX, 103);

    let t = tmp();
    let std_at = nest(t.path(), DIRECT_MAX, "a.sock");
    let l = UnixListener::bind(&std_at).unwrap();
    let c = UnixStream::connect(&std_at).unwrap();
    ping(OwnedFd::from(l), OwnedFd::from(c));
    let std_over = nest(t.path(), DIRECT_MAX + 1, "o.sock");
    let e = UnixListener::bind(&std_over).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
    let e = UnixStream::connect(&std_over).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");

    let t = tmp();
    let at_limit = nest(t.path(), DIRECT_MAX, "s.sock");
    let (l, r) = route::run(BIND, &at_limit).unwrap();
    assert_eq!(r, Route::Standard);
    let (c, r) = route::run(CONNECT_LAST, &at_limit).unwrap();
    assert_eq!(r, Route::Standard);
    ping(l, c);
    // std's own view of the bound address is the path itself on this route.
    // Not on OpenBSD: there std's own local_addr drops the last byte of the
    // path (seen in CI: "/tmp/sockat-XXXXXX/x.soc" for ".../x.sock"), with
    // std's UnixListener::bind and no sockat involved. This only checks a
    // premise about std, not something sockat promises (DR-0013).
    #[cfg(not(target_os = "openbsd"))]
    {
        let l = UnixListener::bind(t.path().join("x.sock")).unwrap();
        assert_eq!(reported(&l), t.path().join("x.sock"));
    }

    let t = tmp();
    let over = nest(t.path(), DIRECT_MAX + 1, "s.sock");
    let (l, r) = route::run(BIND, &over).unwrap();
    assert_ne!(r, Route::Standard);
    assert!(is_socket(&over));
    let (c, _) = route::run(CONNECT_LAST, &over).unwrap();
    ping(l, c);
}

// Long paths of several sizes all end up as a socket at the given path, and
// a client reaching it by the same long path talks to that listener, through
// the public route order (whatever this system allows). 600 bytes is well past
// both 253 (the widened sockaddr on macOS) and sun_path.
#[test]
fn long_paths_bind_at_the_given_path_and_accept_connections() {
    for total in [DIRECT_MAX + 1, 150, 253, 254, 300, 600] {
        let t = tmp();
        let p = nest(t.path(), total, "s.sock");
        let (l, _) = route::run(BIND, &p).unwrap();
        assert!(is_socket(&p), "{total}");
        let (c, _) = route::run(CONNECT_LAST, &p).unwrap();
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
        let e = route::run(CONNECT_LAST, &missing_dir).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{total}: connect {e}");

        let e = route::run(CONNECT_LAST, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{total}: no socket {e}");

        let (_l, _) = route::run(BIND, &p).unwrap();
        let e = route::run(BIND, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::EADDRINUSE), "{total}: {e}");

        let stale = p.with_file_name("stale.sock");
        stale_socket(&stale);
        let e = route::run(CONNECT_LAST, &stale).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ECONNREFUSED), "{total}: {e}");
    }
}

// A missing directory is ENOENT even when the file name (254 bytes) fits no
// sockaddr, for bind and for every connect, on every target (DR-0012): the
// directory is opened before anything else is looked at, so the caller learns
// that the directory is missing, not that the path is too long. Checked
// through the public order and with no place for D or the symlink, so every
// request would otherwise run down to its last route.
#[test]
fn a_missing_directory_is_enoent_even_when_the_name_fits_no_sockaddr() {
    let t = tmp();
    let p = t.path().join("nope").join("n".repeat(254));
    for req in [BIND, CONNECT, CONNECT_LAST] {
        let e = route::run(req, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{req:?}: {e}");
        let e = route::run_long(req, &p, &config(&[], &[])).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{req:?} with no place: {e}");
    }
    assert!(!t.path().join("nope").exists());
}

// A file name of 254 bytes (NAME_MAX is 255) directly under a short directory.
//
// * bind: the spellings through D and beside the socket hold the file name in
//   the plain sockaddr (`R/<name>`, `.sockat.XXXXXXXXXX/<name>`), so they do
//   not reach it: ENAMETOOLONG, and nothing is created, where a bind does not
//   follow a symlink (Linux / Android, FreeBSD, OpenBSD, illumos; symlink-file
//   is not in their bind's order). On macOS and NetBSD the bind goes through
//   symlink-file, whose spelling holds no file name, and reaches it (hub
//   DR-0016).
// * connect: the routes that keep the file name out of the spelling reach the
//   socket file (on Linux with /proc the symlink in D, `/proc/self/fd/<n>/s`;
//   elsewhere the symlink to the file under /tmp), so the name's length does
//   not matter, and the plain `connect` reaches it without forking (every
//   target but FreeBSD, where connectat takes the name as bindat does). A
//   socket bound under a short name and renamed to the long one is reached and
//   talks; with nothing there the error is the kernel's ENOENT, not
//   ENAMETOOLONG. The route order here is the public one.
#[test]
fn a_file_name_of_254_bytes_is_bound_only_through_symlink_file_but_connected_to() {
    let t = tmp();
    let name = "n".repeat(254);
    let p = t.path().join(&name);
    #[cfg(any(target_os = "macos", target_os = "netbsd"))]
    {
        let (l, r) = route::run(BIND, &p).unwrap();
        assert_eq!(r, Route::SymlinkFile);
        assert!(is_socket(&p));
        drop(l);
        std::fs::remove_file(&p).unwrap();
    }
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "illumos"
    ))]
    {
        let e = route::run(BIND, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
        assert!(!is_socket(&p));
    }
    // Whether a bind follows the symlink is not measured here: either way.
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "illumos",
        target_os = "macos",
        target_os = "netbsd"
    )))]
    match route::run(BIND, &p) {
        Ok((_l, r)) => {
            assert_eq!(r, Route::SymlinkFile);
            std::fs::remove_file(&p).unwrap();
        }
        Err(e) => assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}"),
    }

    #[cfg(target_os = "freebsd")]
    {
        let e = route::run(CONNECT_LAST, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        let e = route::run(CONNECT, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");
        let (l, _) = route::run(BIND, &t.path().join("s.sock")).unwrap();
        std::fs::rename(t.path().join("s.sock"), &p).unwrap();
        let (c, r) = route::run(CONNECT, &p).unwrap();
        assert!(!r.forked(), "{r:?}");
        ping(l, c);
    }
    assert_none_named(t.path(), own::HIDDEN_PREFIX);
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
        for op in [BIND, CONNECT_LAST] {
            let e = route::run(op, Path::new(&bad)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{bad:?} {op:?}");
        }
    }
}

// A NUL in the path, past its first byte, is refused as InvalidInput at any
// length, for bind and every connect, and before anything is created. The
// kernel would read the path only up to the NUL, so the socket would be made
// (or looked up) at the part before it, which the caller did not name: with a
// NUL in the directory part, at a file named after the directory's first
// letters. On every target but Linux / Android a leading NUL is refused the
// same way (only Linux has an abstract namespace, see
// linux::a_leading_nul_goes_to_std_at_any_length).
//
// "Nothing is created" covers the routes too: a long path is refused before
// one is tried. The routes through D in particular would otherwise make D in a
// place before the call through it failed, and the symlink route a symlink. So
// the calls run in a child whose TMPDIR is a short directory (the first place
// for D and the symlink), and that place, the socket's directory (the last place
// for D) and the directory above it must keep their modification times (each
// would change if an entry were created in it and removed again).
//
// std itself refuses a NUL anywhere in a path with InvalidInput, so for a
// short path this is what std answers too.
// The socket's directory's real path is taken before any route, and when it
// cannot be resolved that error is what the caller gets (hub SPEC "最初にソケット
// のディレクトリを引く"): without it no R can be made, and reporting
// ENAMETOOLONG would blame the length for what is something else. The case
// built here: the directory is "." in a cwd that was removed. On Linux, the
// Apple OSes and FreeBSD stat(".") still succeeds there while realpath
// (getcwd) fails with ENOENT, which is the case this is about. NetBSD and
// OpenBSD cannot look a removed cwd up at all (stat(".") fails, observed in
// the CI's VMs), so there the case cannot be built and what is checked is the
// lookup before it: the call answers the error of stat("."). On illumos,
// which of the two fails is not observed yet, so the error of whichever does
// is expected. It runs in a child process because it removes the child's own
// cwd; another process (rmdir, run in "/") removes it, since illumos refuses
// to remove the caller's own cwd (rmdir(2): EINVAL). Nothing is made in the
// place for D either, since the real path comes before D.
#[test]
fn an_unresolvable_socket_directory_reports_its_error_not_enametoolong() {
    let gone = tmp();
    let place = tmp();
    mark_untouched(place.path());
    run_child(
        "tests::child_bind_in_a_removed_cwd",
        gone.path(),
        &[
            ("TMPDIR", place.path().as_os_str()),
            ("SOCKAT_CHILD_PATH", gone.path().as_os_str()),
        ],
    );
    assert_untouched(place.path());
}

#[test]
fn child_bind_in_a_removed_cwd() {
    if !in_child() {
        return;
    }
    let rmdir = Command::new("rmdir")
        .arg(env_path("SOCKAT_CHILD_PATH"))
        .current_dir("/")
        .status()
        .unwrap();
    assert!(rmdir.success(), "rmdir: {rmdir}");
    let stat = std::fs::metadata(".").map(drop);
    let real = std::fs::canonicalize(".").map(drop);
    #[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
    let expected = {
        let e = stat.expect_err("premise: stat(\".\") fails");
        errno(&e)
    };
    #[cfg(target_os = "illumos")]
    let expected = match (stat, real) {
        (Err(e), _) | (Ok(()), Err(e)) => errno(&e),
        (Ok(()), Ok(())) => panic!("premise: stat(\".\") or realpath fails"),
    };
    #[cfg(not(any(target_os = "netbsd", target_os = "openbsd", target_os = "illumos")))]
    let expected = {
        assert!(stat.is_ok(), "premise: stat(\".\") works");
        let e = real.expect_err("premise: realpath fails");
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");
        errno(&e)
    };
    #[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
    drop(real);
    assert_ne!(expected, Some(libc::ENAMETOOLONG));
    let p = PathBuf::from(format!("./{}", "n".repeat(120)));
    for req in [BIND, CONNECT] {
        let e = route::run(req, &p).unwrap_err();
        assert_eq!(errno(&e), expected, "{req:?}: {e}");
    }
}

#[test]
fn a_nul_is_invalid_input_at_any_length_and_nothing_is_made() {
    let t = tmp();
    let nested = nest(t.path(), 600, "x");
    let dir = nested.parent().unwrap().to_path_buf();
    let place = tmp();
    for d in [t.path(), &dir, place.path()] {
        mark_untouched(d);
    }
    run_child(
        "tests::child_nul_paths",
        Path::new("/"),
        &[
            ("TMPDIR", place.path().as_os_str()),
            ("SOCKAT_CHILD_PATH", t.path().as_os_str()),
            ("SOCKAT_CHILD_OTHER", dir.as_os_str()),
        ],
    );
    for d in [t.path(), &dir, place.path()] {
        assert_untouched(d);
    }
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[test]
fn child_nul_paths() {
    use std::os::unix::ffi::OsStrExt;
    if !in_child() {
        return;
    }
    let top = env_path("SOCKAT_CHILD_PATH");
    let deep = env_path("SOCKAT_CHILD_OTHER");
    let with = |base: &Path, bytes: &[u8]| {
        let mut p = base.as_os_str().as_bytes().to_vec();
        p.extend_from_slice(bytes);
        PathBuf::from(OsStr::from_bytes(&p))
    };
    #[allow(unused_mut)]
    let mut bad = vec![
        with(&top, b"/a\0b.sock"),
        with(&top, b"/d\0d/s.sock"),
        with(&deep, b"/a\0b.sock"),
        with(&deep, b"\0/s.sock"),
        with(
            &top,
            &[b"/d\0".as_slice(), "d".repeat(300).as_bytes(), b"/s.sock"].concat(),
        ),
    ];
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        bad.push(with(
            Path::new("\0"),
            top.join("s.sock").as_os_str().as_bytes(),
        ));
        bad.push(with(
            Path::new("\0"),
            deep.join("s.sock").as_os_str().as_bytes(),
        ));
    }
    for p in &bad {
        for req in [BIND, CONNECT, CONNECT_NB, CONNECT_LAST] {
            let e = route::run(req, p).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{p:?} {req:?}: {e}");
        }
    }
    assert!(bad.iter().any(|p| path::len(p) <= DIRECT_MAX));
    assert!(bad.iter().any(|p| path::len(p) > 600));
}

// The four public connects, as they map onto the connect options: on a long
// path (200 bytes, short file name: the first route of the connect order
// reaches it, proc-direct on Linux, widened on Apple), each connects from this
// process (the server sees its pid), the two that may fork
// report that they did not, and each returns a stream that is O_NONBLOCK
// exactly for the two nonblocking ones. Which of them may fork is checked route
// by route (check_fork_and_peer) and through the public API in
// tests/public_api.rs.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn the_public_connects_return_the_stream_as_asked_and_report_no_fork_without_one() {
    use std::os::fd::AsFd;
    let me = std::process::id() as libc::pid_t;
    let t = tmp();
    let p = nest(t.path(), 200, "s.sock");
    let listener = crate::bind(&p).unwrap();
    type Connect = fn(&Path) -> io::Result<(UnixStream, bool)>;
    let mut connects: Vec<(&str, Connect, bool)> = vec![
        ("connect", |p| crate::connect(p).map(|c| (c, false)), false),
        (
            "connect_nonblocking",
            |p| crate::connect_nonblocking(p).map(|c| (c, false)),
            true,
        ),
    ];
    connects.extend([
        (
            "connect_ex",
            (|p| {
                crate::connect_ex(p, crate::LastResort::Allow)
                    .map(|a| (a.stream, a.losses.contains(&crate::Loss::Forked)))
                    .map_err(io::Error::from)
            }) as Connect,
            false,
        ),
        (
            "connect_nonblocking_ex",
            |p| {
                crate::connect_nonblocking_ex(p, crate::LastResort::Allow)
                    .map(|a| (a.stream, a.losses.contains(&crate::Loss::Forked)))
                    .map_err(io::Error::from)
            },
            true,
        ),
    ]);
    for (what, connect, nb) in connects {
        let (c, forked) = connect(&p).unwrap();
        assert!(!forked, "{what}");
        assert_eq!(sys::is_nonblocking(c.as_fd()).unwrap(), nb, "{what}");
        let (s, _) = listener.accept().unwrap();
        assert_eq!(peer_pid(&s), me, "{what}");
    }
}

// The fork route (forced here) binds and connects relative to a directory fd
// from a forked child that fchdirs. The parent's socket is the one bound /
// connected (fork shares the open file description), the socket file is in the
// directory, and the parent's cwd is untouched.
#[test]
fn fork_binds_and_connects_through_the_shared_socket() {
    let cwd = std::env::current_dir().unwrap();
    let t = tmp();
    let p = nest(t.path(), 400, "s.sock");
    let (dir, name) = path::split(&p).unwrap();
    let addr = SockAddr::standard(name).unwrap();
    let l = route::fork_op(BIND, dir, &addr).unwrap().0;
    assert!(is_socket(&p));
    let c = route::fork_op(CONNECT_LAST, dir, &addr).unwrap().0;
    assert_eq!(std::env::current_dir().unwrap(), cwd);
    ping(l, c);
}

// The fork route's bind leaves the listen to this process (DR-0025): the child
// only binds and exits, and this process listens. The process that listens is
// the one a client's peer-credential query names as the server
// (LOCAL_PEERPID on macOS, SO_PEERCRED on Linux, asked on the client's own
// socket), so a client must see this process there, not the exited child. A
// child that listened would show itself there: on Linux for good, on macOS
// until something is sent. So it is checked right after the connect (before
// the server accepts and before anything is sent), after the accept, and after
// data went both ways. The client is an ordinary connect of this process (it
// does not fork), so the pid it sees is the server's alone. Three binds: the
// route forced (fork_op); the bind through D with its other means unusable
// (the thread-cwd means off, and a file name of 85 bytes, too long behind
// /proc/self/fd/<n>/R/ on Linux), forking in D; and with D made nowhere and
// the means before the fork off (thread cwd, /proc), forking with R beside the
// socket (a name of 80 bytes, within the 19 bytes `.sockat.XXXXXXXXXX/` leaves
// on Apple).
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn a_client_sees_this_process_as_the_server_of_a_bind_by_fork() {
    let me = std::process::id() as libc::pid_t;
    let t = tmp();
    let place = tmp();
    let forced = nest(t.path(), 400, "forced.sock");
    let (dir, name) = path::split(&forced).unwrap();
    let addr = SockAddr::standard(name).unwrap();
    let (forced_l, child) = route::fork_op(BIND, dir, &addr).unwrap();
    assert_ne!(child, me);
    let in_d = nest(t.path(), 400, &"d".repeat(85));
    let c = no_thread_cwd(config(&[at(place.path())], &[]));
    let (in_d_l, r) = route::run_long(BIND, &in_d, &c).unwrap();
    assert!(r.forked(), "{r:?}");
    let beside = nest(t.path(), 400, &"b".repeat(80));
    let (beside_l, r) = route::run_long(BIND, &beside, &fork_means_only(config(&[], &[]))).unwrap();
    assert!(r.forked(), "{r:?}");

    for (what, l, p) in [
        ("forced", forced_l, &forced),
        ("in D", in_d_l, &in_d),
        ("beside", beside_l, &beside),
    ] {
        let listener = UnixListener::from(l);
        let (c, r) = route::run(CONNECT, p).unwrap();
        assert!(!r.forked(), "{what}: {r:?}");
        let mut c = UnixStream::from(c);
        assert_eq!(peer_pid(&c), me, "{what}: before the accept");
        let (mut s, _) = listener.accept().unwrap();
        assert_eq!(peer_pid(&c), me, "{what}: before anything is sent");
        c.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).unwrap();
        s.write_all(b"pong").unwrap();
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"pong");
        assert_eq!(peer_pid(&c), me, "{what}: after both sent");
    }
    assert_empty(place.path());
}

// The child of the fork route reports errno through its exit status, and the
// parent turns it back into the same io::Error the syscall would have given:
// ENOENT for a missing socket or directory, EADDRINUSE for an existing file,
// ECONNREFUSED for a dead socket file.
#[test]
fn the_forked_child_reports_its_errno() {
    let t = tmp();
    let p = nest(t.path(), 400, "s.sock");
    let (dir, name) = path::split(&p).unwrap();
    let addr = SockAddr::standard(name).unwrap();

    let e = route::fork_op(CONNECT_LAST, dir, &addr).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");

    let e = route::fork_op(BIND, &dir.join("nope"), &addr).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");

    let _l = route::fork_op(BIND, dir, &addr).unwrap().0;
    let e = route::fork_op(BIND, dir, &addr).unwrap_err();
    assert_eq!(errno(&e), Some(libc::EADDRINUSE), "{e}");

    stale_socket(&dir.join("stale.sock"));
    let stale = SockAddr::standard(OsStr::new("stale.sock")).unwrap();
    let e = route::fork_op(CONNECT_LAST, dir, &stale).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ECONNREFUSED), "{e}");
}

// When something else reaps the fork route's child first (SIGCHLD set to
// SIG_IGN makes the kernel do it, and waitpid then fails with ECHILD), the
// outcome cannot come from the exit status; it is read off the socket: bound /
// connected is success, anything else an error. Runs in a child process
// because the SIGCHLD disposition is process-wide.
#[test]
fn the_forked_child_may_be_reaped_elsewhere() {
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
    let c = route::fork_op(CONNECT_LAST, dir, &addr).unwrap().0;
    ping(l, c);
    // A failed connect cannot be told apart by errno any more, but it is still
    // a failure.
    let missing = SockAddr::standard(OsStr::new("missing.sock")).unwrap();
    assert!(route::fork_op(CONNECT_LAST, dir, &missing).is_err());
}

// Routes run concurrently from many threads must not disturb other threads'
// relative path lookups: none of them changes the process's cwd (the thread-cwd
// means changes only its own thread's, the fork route only the child's).
// Watcher threads resolve a relative path and compare getcwd the whole time the
// workers bind / connect, through the public order and the forced fork route.
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
                    let (c, _) = route::run(CONNECT_LAST, &p).unwrap();
                    ping(l, c);
                    let (dir, _) = path::split(&p).unwrap();
                    let fp = dir.join("f.sock");
                    let faddr = SockAddr::standard(OsStr::new("f.sock")).unwrap();
                    let l = route::fork_op(BIND, dir, &faddr).unwrap().0;
                    assert!(is_socket(&fp));
                    let c = route::fork_op(CONNECT_LAST, dir, &faddr).unwrap().0;
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
// the routes through D (every platform)
// ---------------------------------------------------------------------------

// The routes through D as the platform takes them, with one place for D: a
// bind and the four connects of a 600-byte path (a short file name; for the
// connects the routes that need neither writing nor a thread off,
// [`no_direct`]) go through D, and afterwards:
//
// * the socket is at the path given and data flows both ways;
// * the place was used (its mtime changed: D was made in it) and is empty
//   again (D and R removed whatever the outcome);
// * the spelling the kernel keeps for the bound socket is `R/<name>`, R being
//   10 random letters and digits (or `/proc/self/fd/<n>/R/<name>` for the /proc
//   version): nothing of the place, nothing that still exists;
// * the cwd of the process is the one it had.
//
// The means: the thread-cwd one where this system allows it (Linux without a
// seccomp filter, Apple), otherwise on Linux the /proc version, on FreeBSD
// bindat / connectat, on the other targets a forked child in D for the bind
// (their connects have no route through D and take the symlink under a place
// for it, which leaves the place for D untouched).
#[test]
fn the_routes_through_d_go_through_r_and_leave_nothing_behind() {
    let thread = thread_here();
    let cwd = std::env::current_dir().unwrap();
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let c = config(&[at(place.path())], &[links.path()]);
    let connect_c = no_direct(c.clone());
    let p = nest(t.path(), 600, "s.sock");

    mark_untouched(place.path());
    let (l, r) = route::run_long(BIND, &p, &c).unwrap();
    assert!(route_through_d(BIND, thread, r), "bind {r:?}");
    assert!(is_socket(&p));
    assert_touched(place.path());
    assert_empty(place.path());
    let listener = UnixListener::from(l);
    #[cfg(not(target_os = "openbsd"))]
    {
        let shown = reported(&listener);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let shown = match shown.strip_prefix(route::linux::PROC_SELF_FD) {
            Ok(rest) => rest.iter().skip(1).collect(),
            Err(_) => shown,
        };
        assert!(through_link(&shown, "", "s.sock"), "{shown:?}");
    }

    for req in CONNECTS {
        mark_untouched(place.path());
        let (cl, r) = route::run_long(req, &p, &connect_c).unwrap();
        if connects_through_d(thread) {
            assert!(route_through_d(req, thread, r), "{req:?} {r:?}");
            assert_touched(place.path());
        } else {
            assert_untouched(place.path());
        }
        assert!(!r.forked(), "{req:?} {r:?}");
        ping_kept(&listener, cl);
        assert_empty(place.path());
    }
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

// The working directory of the calling thread is left as it is, also when the
// caller set one of its own on that thread (Apple: pthread_fchdir_np; Linux:
// unshare(CLONE_FS) and chdir). The thread-cwd means starts a thread of its
// own for the call instead of moving the caller's (and moving it back with
// pthread_fchdir_np(-1) would drop the caller's own setting). Checked from a
// test thread that sets its own cwd to a directory X, binds and connects
// through D by thread-cwd, then reads its cwd: still X; and the process's
// cwd, read on this thread, unchanged. Where the thread-cwd means is not used
// (a seccomp filter), the caller cannot set a thread cwd of its own on Linux
// either, and there is nothing to check.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn the_callers_own_thread_cwd_is_left_as_it_is() {
    if !thread_cwd_usable() {
        return;
    }
    let cwd = std::env::current_dir().unwrap();
    let t = tmp();
    let x = tmp();
    let place = tmp();
    let p = nest(t.path(), 600, "s.sock");
    let x_real = std::fs::canonicalize(x.path()).unwrap();
    let x_path = x.path().to_path_buf();
    let place_path = place.path().to_path_buf();
    let seen = std::thread::spawn(move || {
        #[cfg(target_vendor = "apple")]
        {
            use std::os::fd::AsFd;
            let x = route::open_dir(&x_path).unwrap();
            sys::set_thread_cwd(x.as_fd()).unwrap();
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            sys::unshare_fs().unwrap();
            std::env::set_current_dir(&x_path).unwrap();
        }
        let before = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let c = config(&[at(&place_path)], &[]);
        let (l, r) = route::run_long(BIND, &p, &c).unwrap();
        assert_eq!(r, Route::ThreadCwd);
        let (cl, r) = route::run_long(CONNECT, &p, &no_direct(c)).unwrap();
        assert_eq!(r, Route::ThreadCwd);
        ping(l, cl);
        let after = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        (before, after)
    })
    .join()
    .unwrap();
    assert_eq!(seen.0, x_real, "the premise: the thread's own cwd is X");
    assert_eq!(seen.1, x_real);
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

// The spelling the kernel keeps for a socket bound through D or beside the
// socket names nothing once the call has returned, wherever it is resolved later.
// Libraries above sockat unlink or stat the bound address (Python's asyncio
// cleaning up, Go's UnixListener.Close, a user's remove_file(local_addr)), so
// the spelling must not land on an unrelated file:
//
// * `R/<name>` (thread cwd, fork, bindat) resolved against the cwd: the cwd
//   holds a file `<name>` and a directory that is not R; R itself is gone.
//   Removing / stat'ing the spelling is ENOENT and the file stays;
// * `/proc/self/fd/<n>/R/<name>` (Linux, the /proc version) after the fd
//   number n is reused by another directory that holds `<name>`: ENOENT again,
//   and that file stays. The premise is checked too: the same spelling without
//   R (what a bind straight through /proc would leave) does reach the file;
// * `.sockat.XXXXXXXXXX/<name>` and `/proc/self/fd/<n>/.sockat.XXXXXXXXXX/<name>`
//   (D made nowhere, by the platform's first means and by the fork) the same
//   as `R/<name>` and its /proc version.
//
// Runs in a child process: its cwd is a directory of decoys, and reusing an fd
// number by opening directories until it comes up needs a process where no
// other thread opens files meanwhile.
#[cfg(not(target_os = "openbsd"))]
#[test]
fn the_spelling_kept_for_a_bound_socket_names_nothing_afterwards() {
    let t = tmp();
    let decoys = tmp();
    std::fs::write(decoys.path().join("s.sock"), b"keep").unwrap();
    std::fs::create_dir(decoys.path().join("XXXXXXXXXX")).unwrap();
    let place = tmp();
    run_child(
        "tests::child_spelling_names_nothing",
        decoys.path(),
        &[
            ("SOCKAT_CHILD_PATH", t.path().as_os_str()),
            ("SOCKAT_CHILD_OTHER", place.path().as_os_str()),
        ],
    );
    assert_eq!(
        std::fs::read(decoys.path().join("s.sock")).unwrap(),
        b"keep"
    );
    assert_empty(place.path());
}

#[cfg(not(target_os = "openbsd"))]
#[test]
fn child_spelling_names_nothing() {
    if !in_child() {
        return;
    }
    let t = env_path("SOCKAT_CHILD_PATH");
    let place = env_path("SOCKAT_CHILD_OTHER");
    let decoy = std::env::current_dir().unwrap().join("s.sock");
    let gone = |spelling: &Path, what: &str| {
        let e = std::fs::remove_file(spelling).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{what}: {spelling:?}");
        assert!(std::fs::symlink_metadata(spelling).is_err(), "{what}");
        assert_eq!(std::fs::read(&decoy).unwrap(), b"keep", "{what}");
    };

    let mut configs = vec![("in D", no_thread_cwd(config(&[at(&place)], &[])))];
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    configs.push(("thread cwd", config(&[at(&place)], &[])));
    configs.push(("beside", config(&[], &[])));
    configs.push(("beside, fork", fork_means_only(config(&[], &[]))));
    for (i, (what, c)) in configs.into_iter().enumerate() {
        let p = nest(&t.join(i.to_string()), 600, "s.sock");
        let (l, _) = route::run_long(BIND, &p, &c).unwrap();
        let l = UnixListener::from(l);
        let shown = reported(&l);
        let prefix = if what.starts_with("beside") {
            own::HIDDEN_PREFIX
        } else {
            ""
        };
        #[cfg(any(target_os = "linux", target_os = "android"))]
        if let Ok(rest) = shown.strip_prefix(route::linux::PROC_SELF_FD) {
            child_proc_spelling_names_nothing(rest, prefix);
            continue;
        }
        assert!(through_link(&shown, prefix, "s.sock"), "{what}: {shown:?}");
        gone(&shown, what);
    }
}

/// `rest` is `<n>/R/s.sock` of a `/proc/self/fd/...` spelling, R drawn with
/// `prefix` (none in D, `.sockat.` beside the socket): reuse the fd number n
/// for a directory holding `s.sock`, then check that the spelling does not
/// reach it while the same spelling without R does.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn child_proc_spelling_names_nothing(rest: &Path, prefix: &str) {
    use std::os::fd::AsRawFd;
    let parts: Vec<_> = rest.iter().collect();
    assert_eq!(parts.len(), 3, "{rest:?}");
    assert!(is_drawn(parts[1], prefix), "{rest:?}");
    let n: i32 = parts[0].to_str().unwrap().parse().unwrap();
    let other = tmp();
    std::fs::write(other.path().join("s.sock"), b"theirs").unwrap();
    let mut held = Vec::new();
    let reused = loop {
        let f = std::fs::File::open(other.path()).unwrap();
        let fd = f.as_raw_fd();
        held.push(f);
        if fd == n {
            break true;
        }
        if fd > n + 64 {
            break false;
        }
    };
    assert!(reused, "fd {n} did not come up again");
    let proc = Path::new(route::linux::PROC_SELF_FD).join(parts[0]);
    assert!(
        proc.join("s.sock").exists(),
        "the premise: /proc/self/fd/{n}/s.sock now reaches the other directory"
    );
    let spelling = proc.join(parts[1]).join("s.sock");
    let e = std::fs::remove_file(&spelling).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound, "{spelling:?}");
    assert_eq!(
        std::fs::read(other.path().join("s.sock")).unwrap(),
        b"theirs"
    );
}

/// Whether `shown`, an address the kernel reports for a socket bound through
/// a link, is `<link>/<name>` with the link a name sockat draws with `prefix`
/// (or `/proc/self/fd/<n>/<link>/<name>` on Linux): neither the path given nor
/// the bare file name.
fn holds_r(shown: &Path, prefix: &str, name: &str) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Ok(rest) = shown.strip_prefix(route::linux::PROC_SELF_FD) {
        let rest: PathBuf = rest.iter().skip(1).collect();
        return through_link(&rest, prefix, name);
    }
    through_link(shown, prefix, name)
}

// What the kernel reports for a socket bound through D (each means the
// platform has: the thread-cwd one where usable, otherwise the next; and with
// the thread-cwd means off) and without D (R beside the socket): the
// listener's local address, the local address of the stream it accepts, and
// the connected client's peer address are each the spelling of the bind, with
// R's random part in it: never the path given (which sockat does not hand to
// the kernel) and never the bare file name (which would resolve against
// whatever cwd it is read in). The client connects as the platform's order
// takes it; what it reports is the server's address all the same, except on
// illumos, whose getpeername reports the address the client connected by
// (observed in the CI's VM): there the client's peer is the spelling of the
// connect, symlink-file's `<links>/sockat.XXXXXXXXXX` (the path is too long
// for relative), which is not the path given or the bare file name either.
// OpenBSD's std drops the last byte of what it reports, so the check is left
// out there.
#[cfg(not(target_os = "openbsd"))]
#[test]
fn the_addresses_of_a_socket_bound_through_r_hold_its_random_part() {
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let with_d = config(&[at(place.path())], &[links.path()]);
    let cases = [
        ("default", with_d.clone(), ""),
        ("thread cwd off", no_thread_cwd(with_d), ""),
        (
            "without D",
            config(&[], &[links.path()]),
            own::HIDDEN_PREFIX,
        ),
    ];
    for (i, (what, c, prefix)) in cases.into_iter().enumerate() {
        let name = format!("{i}.sock");
        let p = nest(t.path(), 600, &name);
        let (l, r) = route::run_long(BIND, &p, &c).unwrap();
        let listener = UnixListener::from(l);
        let (cl, by) = route::run_long(CONNECT, &p, &c).unwrap();
        #[cfg(not(target_os = "illumos"))]
        let _ = by;
        let client = UnixStream::from(cl);
        let (accepted, _) = listener.accept().unwrap();
        for (whose, addr) in [
            ("listener", listener.local_addr().unwrap()),
            ("accepted", accepted.local_addr().unwrap()),
            ("client's peer", client.peer_addr().unwrap()),
        ] {
            let shown = addr.as_pathname().unwrap();
            #[cfg(target_os = "illumos")]
            if whose == "client's peer" {
                assert_eq!(by, Route::SymlinkFile, "{what}");
                assert_eq!(shown.parent(), Some(links.path()), "{what}: {shown:?}");
                assert!(
                    is_drawn(shown.file_name().unwrap(), own::PREFIX),
                    "{what}: {shown:?}"
                );
                assert_ne!(shown, p.as_path(), "{what} {whose}");
                continue;
            }
            assert!(
                holds_r(shown, prefix, &name),
                "{what} {r:?} {whose}: {shown:?}"
            );
            assert_ne!(shown, p.as_path(), "{what} {whose}");
            assert_ne!(shown, Path::new(&name), "{what} {whose}");
        }
    }
    assert_empty(place.path());
    assert_empty(links.path());
}

// What bind_ex keeps is taken inside the route, right after the bind: the
// real path of the socket file and its (st_dev, st_ino), on every bind route
// (std for a short path; through D by each means; without D). Checked against
// realpath(3) and lstat(2) of the path given, which name the same file.
#[test]
fn the_bound_file_is_taken_right_after_the_bind_on_every_route() {
    use std::os::unix::fs::MetadataExt;
    let t = tmp();
    let place = tmp();
    let with_d = config(&[at(place.path())], &[]);
    let check = |p: &Path, bound: Option<route::Bound>, what: &str| {
        let bound = bound.unwrap_or_else(|| panic!("{what}: nothing taken"));
        let m = std::fs::symlink_metadata(p).unwrap();
        assert_eq!(bound.path, std::fs::canonicalize(p).unwrap(), "{what}");
        assert_eq!(bound.id, (m.dev(), m.ino()), "{what}");
    };
    let short = t.path().join("short.sock");
    let r = route::run_reporting(BIND, &short).unwrap();
    assert_eq!(r.route, Route::Standard);
    check(&short, r.bound, "short");
    let cases = [
        ("default", with_d.clone()),
        ("thread cwd off", no_thread_cwd(with_d)),
        ("without D", config(&[], &[])),
    ];
    let mut listeners = Vec::new();
    for (i, (what, c)) in cases.into_iter().enumerate() {
        let p = nest(t.path(), 600, &format!("{i}.sock"));
        let r = route::run_long_with(BIND, &p, &c, true).unwrap();
        check(&p, r.bound, &format!("{what} {:?}", r.route));
        listeners.push((r.fd, p));
    }
    let (_, p) = &listeners[0];
    let r = route::run_long_with(CONNECT_LAST, p, &config(&[], &[]), true).unwrap();
    assert_eq!(r.bound, None, "a connect takes nothing");
    assert_empty(place.path());
}

// The routes through D never open the socket's directory: R in D points to
// its real path, which the kernel resolves with search permission alone, as
// for a plain bind / connect by path. So a socket's directory without read
// permission (mode 0300: write + search) is reached through D on every target,
// also where a directory is opened O_RDONLY (the BSDs other than Apple,
// illumos), which would fail there. The bind goes through D and the connects
// as the platform takes them (proc-direct, which opens the directory O_PATH;
// through D; or the symlink, which needs no fd of the directory either). Only
// the routes that need the directory's fd (proc-direct, at-direct, a fork in
// it) open it, and where that fails they cannot be used. Root bypasses permission checks, so as root the
// test cannot tell anything and says so.
#[test]
fn a_directory_without_read_permission_is_reached_through_d() {
    use std::os::unix::fs::PermissionsExt;
    if sys::effective_uid() == 0 {
        eprintln!("skipped: running as root, which bypasses the permission under test");
        return;
    }
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let c = config(&[at(place.path())], &[links.path()]);
    let p = nest(t.path(), 400, "s.sock");
    let dir = p.parent().unwrap().to_path_buf();
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    let _restore = Restore(dir.clone());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300)).unwrap();
    let e = std::fs::read_dir(&dir).unwrap_err();
    assert_eq!(
        errno(&e),
        Some(libc::EACCES),
        "the premise: no read permission"
    );
    let (l, r) = route::run_long(BIND, &p, &c).unwrap();
    assert!(route_through_d(BIND, thread_here(), r), "{r:?}");
    assert!(is_socket(&p));
    let listener = UnixListener::from(l);
    for req in CONNECTS {
        let (cl, r) = route::run_long(req, &p, &c).unwrap();
        assert!(!r.forked(), "{req:?} {r:?}");
        ping_kept(&listener, cl);
    }
    assert_empty(place.path());
    assert_empty(links.path());
}

thread_local! {
    /// How often [`refuse_socket_dir`] was called on this thread.
    static SOCKET_DIR_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A stand-in for opening the socket's directory ([`Config::open_socket_dir`])
/// where that is refused (`EACCES`, as `O_RDONLY` answers for a directory
/// without read permission), counting the calls ([`socket_dir_opens`]). The
/// route order opens it on the calling thread, and each test runs on a thread
/// of its own, so the count is this test's.
fn refuse_socket_dir(_: &Path) -> io::Result<OwnedFd> {
    SOCKET_DIR_OPENS.with(|n| n.set(n.get() + 1));
    Err(io::Error::from_raw_os_error(libc::EACCES))
}

/// How often the socket's directory was asked for since the last call, by
/// [`refuse_socket_dir`] on this thread.
fn socket_dir_opens() -> usize {
    SOCKET_DIR_OPENS.with(|n| n.replace(0))
}

// The socket's directory is opened only by the routes that need its fd
// (proc-direct, at-direct, a fork in it, D or R made in it), so a directory
// that cannot be opened costs only those (SPEC). With its open refused (and
// the widened sockaddr off on Apple, [`no_direct`]) and one place for D, the
// bind and the connects still go through D made there, by the platform's
// default means and, with the thread-cwd means off, by the next one: on Linux
// the /proc version, which checks /proc without the socket's directory;
// elsewhere a fork in D for a bind, and for a connect at (FreeBSD) or the
// symlink, which does not fork. The bind does not even ask for the socket's
// directory (D in a place other than it needs no fd of it); a connect asks
// once, for the route before D that needs it (proc-direct on Linux, at-direct
// on FreeBSD), and nowhere else.
#[test]
fn a_socket_directory_that_cannot_be_opened_costs_only_the_routes_that_need_its_fd() {
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let asked_by_a_connect = usize::from(cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd"
    )));
    socket_dir_opens();
    for (i, thread) in [thread_here(), false].into_iter().enumerate() {
        let mut c = no_direct(config(&[at(place.path())], &[links.path()]));
        if !thread {
            c = no_thread_cwd(c);
        }
        let p = nest(&t.path().join(i.to_string()), 400, "s.sock");
        let (l, r) = route::run_long(BIND, &p, &c).unwrap();
        assert!(
            route_through_d(BIND, thread, r),
            "thread {thread}: bind {r:?}"
        );
        assert!(is_socket(&p));
        assert_eq!(socket_dir_opens(), 0, "thread {thread}: bind");
        let listener = UnixListener::from(l);
        for req in CONNECTS {
            let (cl, r) = route::run_long(req, &p, &c).unwrap();
            if connects_through_d(thread) {
                assert!(
                    route_through_d(req, thread, r),
                    "thread {thread}: {req:?} {r:?}"
                );
            } else {
                assert!(!r.forked(), "thread {thread}: {req:?} {r:?}");
            }
            assert_eq!(
                socket_dir_opens(),
                asked_by_a_connect,
                "thread {thread}: {req:?}"
            );
            ping_kept(&listener, cl);
        }
        assert_empty(place.path());
        assert_empty(links.path());
    }
}

// D is tried in its places in order, each passed over when D cannot be made
// in it, and the first where it can is used (DR-0027): a place that does not
// exist, a place that is a file, then two directories. The first directory is
// used (its mtime changes; empty afterwards) and the second untouched. A bind
// goes through D by this platform's means.
#[test]
fn d_goes_in_the_first_place_where_it_can_be_made() {
    let t = tmp();
    let file = t.path().join("file");
    std::fs::write(&file, b"keep").unwrap();
    let first = tmp();
    let second = tmp();
    let p = nest(t.path(), 600, "s.sock");
    let c = config(
        &[
            at(Path::new("/nonexistent")),
            at(&file),
            at(first.path()),
            at(second.path()),
        ],
        &[],
    );
    mark_untouched(first.path());
    mark_untouched(second.path());
    let (l, r) = route::run_long(BIND, &p, &c).unwrap();
    assert!(!matches!(r, Route::Standard), "{r:?}");
    assert!(is_socket(&p));
    assert_touched(first.path());
    assert_untouched(second.path());
    assert_empty(first.path());
    assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    let (cl, _) = route::run_long(CONNECT_LAST, &p, &c).unwrap();
    ping(l, cl);
}

// A place that is not safe for a symlink (anyone may write to it, not sticky:
// mode 0777) is good enough for D: D is held by its fd and checked to be the
// one made, so whoever else can rename entries there cannot redirect the call.
// The bind goes through D made there, and the place is empty afterwards. And
// a place where D cannot be created (no write permission: mode 0500, as a user
// other than root, which writes anywhere) is passed over for the next.
#[test]
fn d_needs_no_safe_place() {
    use std::os::unix::fs::PermissionsExt;
    let t = tmp();
    let places = tmp();
    let open = places.path().join("open");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    let p = nest(t.path(), 600, "s.sock");
    mark_untouched(&open);
    let (l, _) = route::run_long(BIND, &p, &config(&[at(&open)], &[])).unwrap();
    assert_touched(&open);
    assert_empty(&open);
    drop(l);

    if sys::effective_uid() == 0 {
        eprintln!("skipped the place without write permission: root writes anywhere");
        return;
    }
    let locked = places.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let next = tmp();
    mark_untouched(next.path());
    let q = p.with_file_name("q.sock");
    let (_l, _) = route::run_long(BIND, &q, &config(&[at(&locked), at(next.path())], &[])).unwrap();
    assert_touched(next.path());
    assert_empty(next.path());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_empty(&locked);
}

// D made in the socket's directory is named `.sockat.XXXXXXXXXX`, in any other
// place `sockat.XXXXXXXXXX`: the socket's directory is the user's, and a D left
// behind by a process that died is recognisable as sockat's and stays out of
// plain listings. Made directly (OwnDir::create), the name is checked; through
// the route order with no other place usable, a connect makes D in the
// socket's directory (its mtime changes, and a connect makes nothing else
// there) and leaves nothing named `.sockat.`. The routes before D are passed
// over by the path: 600 bytes (too long for relative and widened) and on
// Linux a file name of 93 bytes (too long for proc-direct, short enough for
// `R/<name>` of thread-cwd and for proc-held). This is checked where the
// connect has a route through D after them (Linux, Apple); FreeBSD's
// at-direct takes every file name its at takes.
#[test]
fn d_in_the_socket_directory_is_hidden_and_removed() {
    use std::os::fd::AsFd;
    let t = tmp();
    let place = tmp();
    let name = if cfg!(any(target_os = "linux", target_os = "android")) {
        "n".repeat(93)
    } else {
        "s.sock".to_string()
    };
    let p = nest(t.path(), 600, &name);
    let dir = p.parent().unwrap();
    let dir_fd = route::open_dir(dir).unwrap();
    let target = std::fs::canonicalize(dir).unwrap();
    for (where_, prefix) in [
        (Place::SocketDir, own::HIDDEN_PREFIX),
        (at(place.path()), own::PREFIX),
    ] {
        let d =
            own::OwnDir::create(&where_, Some(dir_fd.as_fd()), None, own::open_own_dir).unwrap();
        d.with_r(&target).unwrap();
        assert!(is_drawn(&d.name, prefix), "{where_:?}: {:?}", d.name);
        drop(d);
    }
    assert_none_named(dir, own::HIDDEN_PREFIX);
    assert_empty(place.path());

    let c = config(&[at(Path::new("/nonexistent")), Place::SocketDir], &[]);
    let (l, _) = route::run_long(BIND, &p, &c).unwrap();
    assert_none_named(dir, own::HIDDEN_PREFIX);
    let linux = cfg!(any(target_os = "linux", target_os = "android"));
    if linux || cfg!(target_vendor = "apple") && thread_here() {
        mark_untouched(dir);
        let (cl, r) = route::run_long(CONNECT, &p, &c).unwrap();
        assert!(!r.forked(), "{r:?}");
        assert_touched(dir);
        assert_none_named(dir, own::HIDDEN_PREFIX);
        ping(l, cl);
    }
}

/// Whether `r` is the route a bind takes beside the socket with
/// [`fork_means_only`]: fork-cwd-sockdir; on FreeBSD at-sockdir, which cannot be
/// turned off and comes first.
fn bind_forks_beside(r: Route) -> bool {
    #[cfg(target_os = "freebsd")]
    return r == Route::AtSockdir;
    #[cfg(not(target_os = "freebsd"))]
    return matches!(r, Route::ForkCwdSockdir { .. });
}

// With D made nowhere (no place given, standing in for every place failing:
// TMPDIR, /tmp and the socket's directory) and the means before the fork off
// (thread cwd, /proc: [`fork_means_only`]), a bind forks with R made in the
// socket's directory, `.sockat.XXXXXXXXXX` to the directory itself, so the
// spelling the kernel keeps is `.sockat.XXXXXXXXXX/<name>`, and R is removed
// afterwards. The socket is at the path given and reached; the client sees
// this process as the server (the listen is the parent's). A connect reaches
// the socket by the routes that need no D (with /proc off on Linux, the
// symlink under a place for it), not forking.
// The spelling costs 19 bytes: a file name of DIRECT_MAX - 19 binds, one byte
// more is ENAMETOOLONG with nothing made (even where R/<name> would fit in D).
// (FreeBSD binds by bindat instead, with the same spelling.)
#[test]
fn with_d_made_nowhere_a_bind_forks_with_r_beside_the_socket() {
    let t = tmp();
    let links = tmp();
    let c = fork_means_only(config(&[], &[links.path()]));
    let p = nest(t.path(), 600, "s.sock");
    let dir = p.parent().unwrap();
    let (l, r) = route::run_long(BIND, &p, &c).unwrap();
    assert!(bind_forks_beside(r), "{r:?}");
    assert!(is_socket(&p));
    assert_none_named(dir, own::HIDDEN_PREFIX);
    let listener = UnixListener::from(l);
    #[cfg(not(target_os = "openbsd"))]
    assert!(
        through_link(&reported(&listener), own::HIDDEN_PREFIX, "s.sock"),
        "{:?}",
        reported(&listener)
    );
    for req in CONNECTS {
        let (cl, r) = route::run_long(req, &p, &c).unwrap();
        assert!(!r.forked(), "{req:?} {r:?}");
        ping_kept(&listener, cl);
    }
    assert_empty(links.path());

    let longest = DIRECT_MAX - 19;
    let fits = t.path().join("f".repeat(longest));
    let (_l, r) = route::run_long(BIND, &fits, &c).unwrap();
    assert!(bind_forks_beside(r), "{r:?}");
    let over = t.path().join("o".repeat(longest + 1));
    mark_untouched(t.path());
    let e = route::run_long(BIND, &over, &c).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
    assert_untouched(t.path());
}

// With D made nowhere, a bind tries the means through D in the same order as
// with D, beside the socket: R (`.sockat.XXXXXXXXXX`, to the socket's
// directory) made in the socket's directory and the call made with
// `.sockat.XXXXXXXXXX/<name>` relative to its fd (kawaz 2026-11-19, D12-Q1).
// By default that is the platform's first means (the thread-cwd means where it
// is usable, else /proc on Linux, bindat on FreeBSD, the fork elsewhere); with
// the thread-cwd means off, Linux takes the /proc version
// `/proc/self/fd/<n>/.sockat.XXXXXXXXXX/<name>` while the name fits it (about
// 72 bytes), and a name of 80 bytes, which does not, goes on to the fork. The
// spelling the kernel keeps holds R with `.sockat.`, the socket is reached,
// and no R is left in the socket's directory. When the socket's directory
// cannot be opened (its open refused, standing in for a directory without
// read permission where it is opened O_RDONLY), R cannot be made beside it:
// the bind is ENAMETOOLONG and nothing is made there. (The connects reach the
// socket by the routes that need no D.)
#[test]
fn with_d_made_nowhere_a_bind_takes_the_sockdir_routes() {
    let t = tmp();
    let links = tmp();
    let nowhere = config(&[], &[links.path()]);
    let default: fn(Route) -> bool = |r| sockdir_route(thread_here(), r);
    #[allow(unused_mut)]
    let mut cases = vec![("default", nowhere.clone(), "s.sock".to_string(), default)];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let proc: fn(Route) -> bool = |r| r == Route::ProcSockdir;
        let fork: fn(Route) -> bool = |r| matches!(r, Route::ForkCwdSockdir { .. });
        let off = no_thread_cwd(nowhere.clone());
        cases.push(("thread cwd off", off.clone(), "p.sock".to_string(), proc));
        cases.push(("thread cwd off, 80", off, "f".repeat(80), fork));
    }
    for (i, (what, c, name, want)) in cases.into_iter().enumerate() {
        let p = nest(&t.path().join(i.to_string()), 400, &name);
        let dir = p.parent().unwrap();
        let (l, r) = route::run_long(BIND, &p, &c).unwrap();
        assert!(want(r), "{what}: {r:?}");
        assert!(is_socket(&p), "{what}");
        assert_none_named(dir, own::HIDDEN_PREFIX);
        let listener = UnixListener::from(l);
        #[cfg(not(target_os = "openbsd"))]
        {
            let shown = reported(&listener);
            assert!(
                holds_r(&shown, own::HIDDEN_PREFIX, &name),
                "{what}: {shown:?}"
            );
        }
        let (cl, _) = route::run_long(CONNECT, &p, &c).unwrap();
        ping_kept(&listener, cl);
    }

    let p = nest(&t.path().join("refused"), 400, "s.sock");
    let dir = p.parent().unwrap();
    let c = Config {
        open_socket_dir: refuse_socket_dir,
        ..nowhere
    };
    mark_untouched(dir);
    socket_dir_opens();
    let e = route::run_long(BIND, &p, &c).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
    assert_untouched(dir);
    assert_eq!(socket_dir_opens(), 1, "the try beside the socket asks once");
    assert_empty(links.path());
}

// ----- the last resort of bind_ex: symlink-dir (SPEC "symlink-dir", "symlink の置き場と名前") -----

/// The route order's inputs where no route of a plain bind can be used: no
/// place for D, and the socket's directory cannot be opened (so no R beside
/// it either). `places` are where symlink-dir (bind_ex's last resort) may go, in order.
fn bind_nowhere(places: &[&Path]) -> Config {
    Config {
        open_socket_dir: refuse_socket_dir,
        dir_link_places: places.iter().map(|p| p.to_path_buf()).collect(),
        ..config(&[], &[])
    }
}

/// A directory `name` under `parent` with exactly `mode` (set after creating
/// it, so the umask does not interfere).
fn dir_with_mode(parent: &Path, name: &str, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let d = parent.join(name);
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
    d
}

// Where every route of a plain bind is unusable (no place for D, the socket's
// directory cannot be opened), bind is ENAMETOOLONG with nothing made in the
// places for the symlink: a plain bind never takes the symlink route (SPEC "試す
// 順番"). bind_ex with the last resort then binds through a symlink `sockat.XXXXXXXXXX` to the
// socket's directory in a place: the socket is at the path given and reached,
// the address the kernel reports is `<place>/sockat.XXXXXXXXXX/<name>`, and the
// place is empty again afterwards. A place only this user can write to (a
// fresh temporary directory, 0700) is not shared; a sticky one writable by
// all (1777, owned by this user, as /tmp is by root) is.
#[test]
fn symlink_dir_binds_through_a_symlink_in_a_place_where_bind_cannot() {
    let t = tmp();
    let places = tmp();
    let own = dir_with_mode(places.path(), "own", 0o700);
    let shared = dir_with_mode(places.path(), "shared", 0o1777);
    for (i, (place, want_shared)) in [(&own, false), (&shared, true)].into_iter().enumerate() {
        let c = bind_nowhere(&[place]);
        let p = nest(&t.path().join(i.to_string()), 400, "s.sock");
        mark_untouched(place);
        let e = route::run_long(BIND, &p, &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{place:?}: {e}");
        assert_untouched(place);
        let (l, r) = route::run_long(BIND_LAST, &p, &c).unwrap();
        assert_eq!(
            r,
            Route::SymlinkDir {
                shared: want_shared
            },
            "{place:?}"
        );
        assert_eq!(r.name(), "symlink-dir");
        assert!(is_socket(&p));
        assert_touched(place);
        assert_empty(place);
        let listener = UnixListener::from(l);
        #[cfg(not(target_os = "openbsd"))]
        {
            let shown = reported(&listener);
            let rest = shown
                .strip_prefix(place)
                .unwrap_or_else(|_| panic!("{shown:?}"));
            assert!(through_link(rest, own::PREFIX, "s.sock"), "{shown:?}");
        }
        let (cl, _) = route::run_long(CONNECT_LAST, &p, &config(&[], &[])).unwrap();
        ping_kept(&listener, cl);
    }
}

// The places for symlink-dir (bind_ex's last resort): those only this user can write to come
// first, whatever their position among the candidates, then the shared ones
// (SPEC "bind の試し方"). A place where another user could replace the
// symlink (0777, not sticky) is no place at all, and one whose spelling would
// not fit the sockaddr (a deep directory) is passed over; nothing is made in
// either. With only those, bind_ex with the last resort is ENAMETOOLONG.
#[test]
fn symlink_dir_tries_the_places_only_this_user_can_write_to_first() {
    let t = tmp();
    let places = tmp();
    let open = dir_with_mode(places.path(), "open", 0o777);
    let shared = dir_with_mode(places.path(), "shared", 0o1777);
    let deep_base = tmp();
    let deep = nest(deep_base.path(), 200, "deep");
    std::fs::create_dir(&deep).unwrap();
    let own = tmp();
    for d in [&open, &shared, &deep, &own.path().to_path_buf()] {
        mark_untouched(d);
    }
    let p = nest(t.path(), 400, "s.sock");
    let c = bind_nowhere(&[&open, &shared, &deep, own.path()]);
    let (_l, r) = route::run_long(BIND_LAST, &p, &c).unwrap();
    assert_eq!(r, Route::SymlinkDir { shared: false });
    assert_touched(own.path());
    assert_empty(own.path());
    for d in [&open, &shared, &deep] {
        assert_untouched(d);
    }

    let q = p.with_file_name("q.sock");
    let (_l, r) = route::run_long(BIND_LAST, &q, &bind_nowhere(&[&open, &deep, &shared])).unwrap();
    assert_eq!(r, Route::SymlinkDir { shared: true });
    assert_empty(&shared);
    assert_untouched(&open);
    assert_untouched(&deep);

    let e = route::run_long(
        BIND_LAST,
        &p.with_file_name("x.sock"),
        &bind_nowhere(&[&open, &deep]),
    )
    .unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
    assert_untouched(&open);
    assert_untouched(&deep);
}

// A bind through the symlink that fails for real (an existing file:
// EADDRINUSE) is the result: the next place is not tried (untouched), and the
// place used is empty again (SPEC "経路の失敗の扱い").
#[test]
fn symlink_dir_does_not_retry_a_real_failure_in_the_next_place() {
    let t = tmp();
    let first = tmp();
    let second = tmp();
    let p = nest(t.path(), 400, "s.sock");
    std::fs::write(&p, b"keep").unwrap();
    mark_untouched(second.path());
    let e =
        route::run_long(BIND_LAST, &p, &bind_nowhere(&[first.path(), second.path()])).unwrap_err();
    assert_eq!(errno(&e), Some(libc::EADDRINUSE), "{e}");
    assert_empty(first.path());
    assert_untouched(second.path());
    assert_eq!(std::fs::read(&p).unwrap(), b"keep");
}

// Where a route of a plain bind can be used, bind_ex with the last resort takes it as bind does
// (the symlink is the last resort): with a place for D, the platform's
// default route through D, and nothing made in the place for the symlink.
#[test]
fn bind_ex_with_the_last_resort_takes_the_routes_of_bind_first() {
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let c = Config {
        dir_link_places: vec![links.path().to_path_buf()],
        ..config(&[at(place.path())], &[])
    };
    mark_untouched(links.path());
    let p = nest(t.path(), 400, "s.sock");
    let (_l, r) = route::run_long(BIND_LAST, &p, &c).unwrap();
    assert!(route_through_d(BIND, thread_here(), r), "{r:?}");
    assert_untouched(links.path());
}

// ----- a D that cannot be opened (D12-Q5) -----

/// A stand-in for opening D right after it is made, refusing as `O_SEARCH`
/// (Apple) or `O_RDONLY` (other targets) do once a umask took the owner's
/// bits; Linux's `O_PATH` opens it anyway, so there the case exists only
/// through this.
fn refuse_own_dir(_: std::os::fd::BorrowedFd<'_>, _: &OsStr) -> io::Result<OwnedFd> {
    Err(io::Error::from_raw_os_error(libc::EACCES))
}

// A D that cannot be opened right after it was made is removed by its name
// when it passes the checks there (a directory of the effective uid with no
// permission for group and others), and the place is passed over (SPEC "D と
// R"): with D opened by a stand-in that refuses, every place gets D made and
// removed again (touched, empty), and the bind goes on beside the socket.
#[test]
fn a_d_that_cannot_be_opened_is_removed_and_the_place_passed_over() {
    let t = tmp();
    let first = tmp();
    let second = tmp();
    let c = Config {
        open_own_dir: refuse_own_dir,
        ..config(&[at(first.path()), at(second.path())], &[])
    };
    mark_untouched(first.path());
    mark_untouched(second.path());
    let p = nest(t.path(), 400, "s.sock");
    let (_l, r) = route::run_long(BIND, &p, &c).unwrap();
    assert!(sockdir_route(thread_here(), r), "{r:?}");
    for place in [first.path(), second.path()] {
        assert_touched(place);
        assert_empty(place);
    }
}

// What is removed in place of a D that could not be opened is only what the
// checks would have held: looked up by the name without following a symlink,
// an empty directory of this user with no permission for group and others
// (0700, or less for the owner as a umask leaves it: 0300, 0500) is removed;
// a directory with group permission, a non-empty one (AT_REMOVEDIR refuses
// it), a symlink to a directory of ours, and a file are left as they are.
#[test]
fn only_what_passes_the_checks_is_removed_for_a_d_not_opened() {
    use std::os::fd::AsFd;
    let t = tmp();
    let base = route::open_dir(t.path()).unwrap();
    let ours = |name: &str, mode: u32| dir_with_mode(t.path(), name, mode);
    for (name, mode) in [("d700", 0o700), ("d300", 0o300), ("d500", 0o500)] {
        ours(name, mode);
        own::remove_unopened(base.as_fd(), OsStr::new(name));
        assert!(!t.path().join(name).exists(), "{name}");
    }
    let kept = [
        ours("group", 0o770),
        {
            let d = ours("full", 0o700);
            std::fs::write(d.join("x"), b"x").unwrap();
            d
        },
        {
            let target = ours("target", 0o700);
            let link = t.path().join("link");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            link
        },
        {
            let f = t.path().join("file");
            std::fs::write(&f, b"").unwrap();
            f
        },
    ];
    for k in &kept {
        own::remove_unopened(base.as_fd(), k.file_name().unwrap());
        assert!(std::fs::symlink_metadata(k).is_ok(), "{k:?}");
    }
    assert!(t.path().join("target").exists());
}

// When a call fails, it reports the routes passed over and the route that
// made the call and failed, if one did (SPEC "公開する操作", what an any
// function's error carries), and the error keeps its errno:
//
// * no route can be used (no place for D, the socket's directory refused, no
//   place for symlink-dir (bind_ex's last resort)): ENAMETOOLONG, no route, every route
//   passed over, the symlink last;
// * a route fails for real (bind_ex with the last resort through the symlink onto an existing
//   file: EADDRINUSE, after the routes beside the socket, the last of which
//   is at-sockdir on FreeBSD and fork-cwd-sockdir elsewhere; a connect to a
//   missing socket by the first route of the platform's connect order that
//   can be used: ENOENT): that route's name, and the ones before it passed
//   over. The connect is given a place for its symlink as well as one for D:
//   on the OSes whose connect order is relative → symlink-file alone (no
//   route through D: NetBSD, OpenBSD, illumos), symlink-file is then the
//   route that can be used when the cwd is not an ancestor of the path;
// * the directory is missing: ENOENT before any route, and nothing passed
//   over (the length is judged only once the directory is looked up, so not
//   even `standard` has been tried).
//
// Through the public functions the error converts into the io::Error with the
// same errno (ExError: From).
#[test]
fn a_failure_reports_the_routes_tried_and_keeps_the_errno() {
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let p = nest(t.path(), 400, "s.sock");
    let f = route::run_long_with(BIND_LAST, &p, &bind_nowhere(&[]), false)
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG), "{f:?}");
    assert_eq!(f.route, None);
    let names: Vec<&str> = f.skipped.iter().map(|s| s.name).collect();
    assert_eq!(names.first(), Some(&"standard"), "{names:?}");
    assert_eq!(names.last(), Some(&"symlink-dir"), "{names:?}");
    assert!(names.iter().any(|n| n.ends_with("-sockdir")), "{names:?}");

    std::fs::write(&p, b"keep").unwrap();
    let f = route::run_long_with(BIND_LAST, &p, &bind_nowhere(&[place.path()]), false)
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::EADDRINUSE), "{f:?}");
    assert_eq!(f.route, Some("symlink-dir"));
    let last_beside = if cfg!(target_os = "freebsd") {
        "at-sockdir"
    } else {
        "fork-cwd-sockdir"
    };
    assert_eq!(f.skipped.last().map(|s| s.name), Some(last_beside), "{f:?}");
    assert_empty(place.path());

    let c = config(&[at(place.path())], &[links.path()]);
    let missing = p.with_file_name("missing.sock");
    let f = route::run_long_with(CONNECT, &missing, &c, false)
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENOENT), "{f:?}");
    let name = f.route.unwrap_or_else(|| panic!("{f:?}"));
    assert!(
        [
            "proc-direct",
            "relative",
            "widened",
            "at-direct",
            "thread-cwd",
            "proc",
            "at",
            "proc-held",
            "symlink-file",
        ]
        .contains(&name),
        "{name}"
    );
    assert_eq!(f.skipped.first().map(|s| s.name), Some("standard"));
    assert_empty(place.path());
    assert_empty(links.path());

    let gone = nest(&t.path().join("gone"), 400, "s.sock");
    std::fs::remove_dir(gone.parent().unwrap()).unwrap();
    let f = route::run_long_with(BIND_LAST, &gone, &c, false)
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENOENT), "{f:?}");
    assert_eq!(f.route, None);
    assert_eq!(f.skipped, []);

    {
        let e = crate::connect_ex(&missing, crate::LastResort::Allow).unwrap_err();
        assert_eq!(e.error.raw_os_error(), Some(libc::ENOENT), "{e:?}");
        assert!(e.route.is_some(), "{e:?}");
        assert_eq!(e.skipped.first().map(|s| s.name), Some("standard"));
        assert_eq!(e.to_string(), e.error.to_string());
        assert_eq!(io::Error::from(e).raw_os_error(), Some(libc::ENOENT));
    }
}

// `cwd-relative` (SPEC "失うもの"): a relative path std takes as it is is
// bound by that relative spelling, which the kernel keeps; so bind_ex with the last resort of a
// short relative path reports it, alone (standard gives up nothing else),
// and of a short absolute one nothing. A connect gives up nothing either way
// (its spelling is kept nowhere). On Linux, a name in the abstract namespace
// (a leading NUL) is not a relative path: std refuses it (InvalidInput, std
// takes abstract names only by SocketAddr), so bind_ex with the last resort fails there with the
// error of std, by the route `standard`, and reports no loss. Runs in a child
// process whose cwd is a temporary directory.
#[test]
fn bind_ex_of_a_relative_short_path_gives_up_cwd_relative() {
    let cwd = tmp();
    run_child("tests::child_bind_ex_relative", cwd.path(), &[]);
    assert!(is_socket(&cwd.path().join("rel.sock")));
}

#[test]
fn child_bind_ex_relative() {
    use crate::Loss;
    if !in_child() {
        return;
    }
    let rel = crate::bind_ex("rel.sock", crate::LastResort::Allow).unwrap();
    assert_eq!(
        (rel.losses.as_slice(), rel.route),
        (&[Loss::CwdRelative][..], "standard")
    );
    assert_eq!(rel.skipped, []);
    assert_eq!(Loss::CwdRelative.to_string(), "cwd-relative");
    let c = crate::connect_ex("rel.sock", crate::LastResort::Allow).unwrap();
    assert_eq!((c.losses.as_slice(), c.route), (&[][..], "standard"));
    let abs = std::env::current_dir().unwrap().join("abs.sock");
    assert!(
        path::len(&abs) <= DIRECT_MAX,
        "the premise: {abs:?} is short"
    );
    let a = crate::bind_ex(&abs, crate::LastResort::Allow).unwrap();
    assert_eq!((a.losses.as_slice(), a.route), (&[][..], "standard"));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let e = crate::bind_ex(
            OsStr::from_bytes(b"\0sockat-abstract"),
            crate::LastResort::Allow,
        )
        .unwrap_err();
        assert_eq!(e.error.kind(), io::ErrorKind::InvalidInput, "{e:?}");
        assert_eq!(e.route, Some("standard"));
    }
}

// A relative path is resolved against the cwd twice in bind_ex: by the bind
// and by the realpath of what is kept. Another thread changing the cwd in
// between makes them name files in different directories, and discard() would
// then remove a file of the same name the bind never made. So bind_ex compares
// the cwd's (st_dev, st_ino) before the realpath and after the bind and, when
// they differ, treats it as a file that cannot be looked up (SPEC "_ex"): the
// listener is closed, nothing is removed, and the error names the route that
// bound. The cwd is changed by the test hook at the point between the two
// look-ups, to a directory with the same relative directories in it:
// * short path: std binds first, so the bind is under the first cwd and the
//   realpath under the second, where an unrelated file of the same name is;
// * long path through D, through R beside the socket and (bind_ex's last
//   resort) symlink-dir: the bind goes to the socket's directory's real path
//   taken before the change, so it is under the first cwd; the unrelated file
//   is under the second;
// * long path through symlink-file (macOS and NetBSD, where a bind follows it): the
//   symlink points to the path made absolute against the second cwd, where
//   the bind is, and the real path kept is under the first, where the
//   unrelated file is.
// In each case the call fails, the socket file stays where the bind put it,
// and the unrelated file stays. The default bind keeps nothing, so it is not
// checked: the same change leaves its short relative bind as std's. Runs in a
// child process whose cwd is changed.
#[test]
fn bind_ex_of_a_relative_path_fails_when_the_cwd_changes_in_between() {
    let first = tmp();
    let second = tmp();
    run_child(
        "tests::child_cwd_changes_in_between",
        first.path(),
        &[("SOCKAT_CHILD_OTHER_CWD", second.path().as_os_str())],
    );
}

/// Change the cwd to `SOCKAT_CHILD_OTHER_CWD`, as another thread could in the
/// middle of a call ([`Config::between_cwd_lookups`]).
fn to_the_other_cwd() {
    std::env::set_current_dir(env_path("SOCKAT_CHILD_OTHER_CWD")).unwrap();
}

#[test]
fn child_cwd_changes_in_between() {
    if !in_child() {
        return;
    }
    let first = std::env::current_dir().unwrap();
    let second = env_path("SOCKAT_CHILD_OTHER_CWD");
    let place = tmp();
    let link_place = tmp();
    let hooked = |c: Config| Config {
        between_cwd_lookups: to_the_other_cwd,
        ..c
    };
    // A relative path longer than DIRECT_MAX, with its directories under both
    // cwds.
    let long = |name: &str| {
        let p = nest(Path::new("long"), 200, name);
        std::fs::create_dir_all(second.join(p.parent().unwrap())).unwrap();
        p
    };
    struct Case {
        what: &'static str,
        req: Request,
        rel: PathBuf,
        cfg: Config,
        /// The route expected to bind, when only one can.
        route: Option<&'static str>,
        /// Whether the bind is under the second cwd (else the first).
        bound_in_second: bool,
    }
    let case = |what, req, rel, cfg, route, bound_in_second| Case {
        what,
        req,
        rel,
        cfg: hooked(cfg),
        route,
        bound_in_second,
    };
    #[allow(unused_mut)]
    let mut cases = vec![
        case(
            "short",
            BIND,
            PathBuf::from("short.sock"),
            config(&[], &[]),
            Some("standard"),
            false,
        ),
        case(
            "long through D",
            BIND,
            long("d.sock"),
            config(&[at(place.path())], &[]),
            None,
            false,
        ),
        case(
            "long without D",
            BIND,
            long("r.sock"),
            config(&[], &[]),
            None,
            false,
        ),
        case(
            "long through symlink-dir",
            BIND_LAST,
            long("l.sock"),
            bind_nowhere(&[link_place.path()]),
            Some("symlink-dir"),
            false,
        ),
    ];
    #[cfg(any(target_os = "macos", target_os = "netbsd"))]
    cases.push(case(
        "long through symlink-file",
        BIND,
        long("f.sock"),
        with_symlink_file_bind(config(&[at(place.path())], &[link_place.path()])),
        Some("symlink-file"),
        true,
    ));
    let mut failures = Vec::new();
    for Case {
        what,
        req,
        rel,
        cfg: c,
        route: expected,
        bound_in_second,
    } in cases
    {
        std::env::set_current_dir(&first).unwrap();
        let (bound_in, unrelated_in) = match bound_in_second {
            true => (&second, &first),
            false => (&first, &second),
        };
        std::fs::write(unrelated_in.join(&rel), "unrelated").unwrap();
        let result = route::run_with(req, &rel, true, Some(&c));
        std::env::set_current_dir(&first).unwrap();
        match result {
            Ok(r) => {
                // What bind_ex would hand out, discarded as its user would.
                let b = r.bound.unwrap();
                drop(r.fd);
                let kept = b.path.clone();
                let discarded = crate::SocketFile {
                    path: b.path,
                    id: b.id,
                }
                .discard();
                failures.push(format!(
                    "{what}: bound by {} and kept {kept:?} (discard: {discarded:?})",
                    r.route.name()
                ));
            }
            Err(f) => {
                if f.route.is_none() || (expected.is_some() && f.route != expected) {
                    failures.push(format!("{what}: route {:?}, not {expected:?}", f.route));
                }
                if !f.error.to_string().contains("current directory changed") {
                    failures.push(format!("{what}: {:?}", f.error));
                }
            }
        }
        if !is_socket(&bound_in.join(&rel)) {
            failures.push(format!("{what}: no socket file left under {bound_in:?}"));
        }
        if std::fs::read(unrelated_in.join(&rel)).ok().as_deref() != Some(&b"unrelated"[..]) {
            failures.push(format!(
                "{what}: the unrelated file under {unrelated_in:?} was removed"
            ));
        }
    }
    assert_empty(place.path());
    assert_empty(link_place.path());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let c = hooked(config(&[], &[]));
    let (_l, r) = route::run_with(BIND, Path::new("default.sock"), false, Some(&c))
        .map(|r| (r.fd, r.route))
        .unwrap();
    std::env::set_current_dir(&first).unwrap();
    assert_eq!(r, Route::Standard);
    assert!(is_socket(&first.join("default.sock")));
}

// ----- the names of the routes and the routes passed over -----

// The routes carry the names of the SPEC ("経路の名前"), one name for each
// route of this platform, a bind's and a connect's symlink the same.
#[test]
fn the_routes_carry_the_names_of_the_spec() {
    let mut cases: Vec<(Route, &str)> = vec![
        (Route::Standard, "standard"),
        (Route::ForkCwdDirect { child: 1 }, "fork-cwd-direct"),
        (Route::SymlinkDir { shared: false }, "symlink-dir"),
        (Route::SymlinkDir { shared: true }, "symlink-dir"),
    ];
    #[cfg(not(target_os = "freebsd"))]
    cases.extend([
        (Route::ForkCwd { child: 1 }, "fork-cwd"),
        (Route::ForkCwdSockdir { child: 1 }, "fork-cwd-sockdir"),
    ]);
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    cases.extend([
        (Route::ThreadCwd, "thread-cwd"),
        (Route::ThreadCwdSockdir, "thread-cwd-sockdir"),
    ]);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    cases.extend([
        (Route::Proc, "proc"),
        (Route::ProcSockdir, "proc-sockdir"),
        (Route::ProcDirect, "proc-direct"),
        (Route::ProcHeld, "proc-held"),
    ]);
    #[cfg(not(target_os = "freebsd"))]
    cases.extend([
        (Route::Relative, "relative"),
        (Route::SymlinkFile, "symlink-file"),
    ]);
    #[cfg(target_vendor = "apple")]
    cases.extend([
        (Route::Widened, "widened"),
        (Route::WidenedRelative, "widened"),
    ]);
    #[cfg(target_os = "freebsd")]
    cases.extend([
        (Route::At, "at"),
        (Route::AtSockdir, "at-sockdir"),
        (Route::AtDirect, "at-direct"),
    ]);
    for (r, name) in cases {
        assert_eq!(r.name(), name, "{r:?}");
    }
}

// A path std takes is reported as the route `standard` with nothing passed
// over; a longer one has `standard` passed over first (its spelling, the path
// itself, does not fit), then the route that took it.
#[test]
fn standard_is_passed_over_only_for_a_long_path() {
    let t = tmp();
    let short = t.path().join("short.sock");
    let (l, _) = route::run(BIND, &short).unwrap();
    let _short = UnixListener::from(l);
    let reached = route::run_reporting(CONNECT, &short).unwrap();
    assert_eq!(reached.route.name(), "standard");
    assert_eq!(reached.skipped, []);
    let long = nest(t.path(), 400, "long.sock");
    let reached = route::run_reporting(BIND, &long).unwrap();
    assert_ne!(reached.route.name(), "standard");
    assert_eq!(
        reached.skipped.first().map(|s| s.name),
        Some("standard"),
        "{:?}",
        reached.skipped
    );
}

// A call reports the route that made it and the routes tried before it and
// passed over, in the order tried, each with a reason (SPEC "公開する操作",
// what the _ex functions return). With the thread-cwd means off (so the run does
// not depend on it) and the inputs chosen per platform, the names come in the
// SPEC's order: for a bind through D (thread-cwd, proc, fork), then beside
// the socket (the `-sockdir` ones) when D can be made nowhere, then
// symlink-dir (bind_ex's last resort); for a connect the order of the OS (SPEC "connect の
// 順": the routes that need neither writing nor a thread first), then
// fork-cwd-direct. Only the names are pinned; a reason just has to be there.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn the_routes_passed_over_are_reported_in_the_order_tried() {
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let linux = cfg!(any(target_os = "linux", target_os = "android"));
    let with_d = no_thread_cwd(config(&[at(place.path())], &[links.path()]));
    let nowhere = no_thread_cwd(config(&[], &[]));
    let bare = {
        #[allow(unused_mut)]
        let mut c = no_thread_cwd(config(&[], &[]));
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            c.proc_self_fd = PathBuf::from("/nonexistent/proc/self/fd");
        }
        c
    };
    let refused = Config {
        open_socket_dir: refuse_socket_dir,
        dir_link_places: vec![links.path().to_path_buf()],
        ..nowhere.clone()
    };
    // The name for a bind beside the socket: too long for proc through D (and
    // so for proc-sockdir) on Linux, within the 19 bytes of `.sockat.X.../`.
    let beside_name = if linux {
        "b".repeat(85)
    } else {
        "b".repeat(80)
    };

    type Case<'a> = (&'a str, Request, PathBuf, &'a Config, &'a str, Vec<&'a str>);
    // What each case gives up (SPEC "失うもの"): a bind through D or beside
    // the socket respelled, a fork forked too, a bind through symlink-dir in a
    // place only this user can write to respelled only; a connect nothing but
    // for fork-cwd-direct.
    let losses_of = |what: &str| -> Vec<crate::Loss> {
        use crate::Loss::{Forked, Respelled};
        match what {
            "bind through D" if linux => vec![Respelled],
            "bind through D" | "bind beside the socket" => vec![Respelled, Forked],
            "bind_ex through symlink-dir" => vec![Respelled],
            "connect_ex by fork-cwd-direct" => vec![Forked],
            _ => vec![],
        }
    };
    let mut cases: Vec<Case> = vec![
        (
            "bind through D",
            BIND,
            nest(&t.path().join("0"), 400, "s.sock"),
            &with_d,
            if linux { "proc" } else { "fork-cwd" },
            vec!["standard", "thread-cwd"],
        ),
        (
            "bind beside the socket",
            BIND,
            nest(&t.path().join("1"), 400, &beside_name),
            &nowhere,
            "fork-cwd-sockdir",
            if linux {
                vec![
                    "standard",
                    "thread-cwd",
                    "proc",
                    "fork-cwd",
                    "thread-cwd-sockdir",
                    "proc-sockdir",
                ]
            } else {
                vec!["standard", "thread-cwd", "fork-cwd", "thread-cwd-sockdir"]
            },
        ),
        (
            "bind_ex through symlink-dir",
            BIND_LAST,
            nest(&t.path().join("2"), 400, "s.sock"),
            &refused,
            "symlink-dir",
            if linux {
                vec![
                    "standard",
                    "thread-cwd",
                    "proc",
                    "fork-cwd",
                    "thread-cwd-sockdir",
                    "proc-sockdir",
                    "fork-cwd-sockdir",
                ]
            } else {
                vec![
                    "standard",
                    "thread-cwd",
                    "fork-cwd",
                    "thread-cwd-sockdir",
                    "fork-cwd-sockdir",
                ]
            },
        ),
        (
            "connect_ex by fork-cwd-direct",
            CONNECT_LAST,
            nest(&t.path().join("3"), 600, "s.sock"),
            &bare,
            "fork-cwd-direct",
            if linux {
                vec!["standard", "relative", "thread-cwd", "symlink-file"]
            } else {
                vec![
                    "standard",
                    "relative",
                    "widened",
                    "thread-cwd",
                    "symlink-file",
                ]
            },
        ),
    ];
    if linux {
        cases.push((
            "connect by proc-held",
            CONNECT,
            nest(&t.path().join("4"), 400, &"n".repeat(95)),
            &with_d,
            "proc-held",
            vec!["standard", "proc-direct", "thread-cwd", "proc"],
        ));
    } else {
        cases.push((
            "connect by symlink",
            CONNECT,
            nest(&t.path().join("4"), 600, "s.sock"),
            &with_d,
            "symlink-file",
            vec!["standard", "relative", "widened", "thread-cwd"],
        ));
    }
    for (what, req, p, c, route, skipped) in cases {
        let listener = match req {
            Request::Bind(_) => None,
            Request::Connect(_) => {
                let (l, _) = route::run_long(BIND, &p, &config(&[at(place.path())], &[])).unwrap();
                Some(UnixListener::from(l))
            }
        };
        let reached = route::run_long_with(req, &p, c, false).unwrap();
        assert_eq!(reached.route.name(), route, "{what}: {:?}", reached.skipped);
        assert_eq!(
            crate::losses(req.is_bind(), reached.route, &p),
            losses_of(what),
            "{what}"
        );
        let names: Vec<&str> = reached.skipped.iter().map(|s| s.name).collect();
        assert_eq!(names, skipped, "{what}: {:?}", reached.skipped);
        for s in &reached.skipped {
            assert!(!s.reason.is_empty(), "{what}: {s:?}");
        }
        if let Some(listener) = listener {
            ping_kept(&listener, reached.fd);
        }
    }
    assert_empty(place.path());
    assert_empty(links.path());
}

// skipped holds only the routes of the order this call tries on this platform
// in its state (SPEC "公開する操作": the order of "試す順番", for a connect with
// the one row of "connect の順" that the OS and the state, /proc usable or
// not, pick), every one of them that could not be used; a route of no such
// order is not reported. With the thread-cwd means off, /proc looked for where
// nothing is on Linux (a Linux without /proc), D made nowhere and no place for
// a connect's symlink:
//
// * a bind passes over standard, thread-cwd and fork through D, then
//   thread-cwd-sockdir, and is made by fork-cwd-sockdir. proc and proc-sockdir
//   are for a Linux with /proc only (SPEC "経路の名前"), so they are not there;
// * connect_ex with the last resort passes over standard and the row of a Linux without /proc
//   (relative, thread-cwd, symlink; no proc-direct, proc or proc-held) or of
//   Apple (relative, widened, thread-cwd, symlink), and is made by
//   fork-cwd-direct;
// * a connect that may not fork passes over the same routes and fails with
//   ENAMETOOLONG and no route: fork-cwd-direct is not in its order (the last row of
//   its column in "試す順番" is ENAMETOOLONG), so it is not reported either.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn only_the_routes_of_the_order_tried_are_reported() {
    let t = tmp();
    let place = tmp();
    let linux = cfg!(any(target_os = "linux", target_os = "android"));
    let bare = fork_means_only(config(&[], &[]));
    let names = |s: &[route::Skipped]| s.iter().map(|s| s.name).collect::<Vec<_>>();

    let p = nest(&t.path().join("bind"), 400, "s.sock");
    let reached = route::run_long_with(BIND, &p, &bare, false).unwrap();
    let _bound = UnixListener::from(reached.fd);
    assert_eq!(
        reached.route.name(),
        "fork-cwd-sockdir",
        "{:?}",
        reached.skipped
    );
    assert_eq!(
        names(&reached.skipped),
        ["standard", "thread-cwd", "fork-cwd", "thread-cwd-sockdir"]
    );
    assert!(is_socket(&p));

    let p = nest(&t.path().join("connect"), 600, "s.sock");
    let (l, _) = route::run_long(BIND, &p, &config(&[at(place.path())], &[])).unwrap();
    let listener = UnixListener::from(l);
    let order: &[&str] = if linux {
        &["standard", "relative", "thread-cwd", "symlink-file"]
    } else {
        &[
            "standard",
            "relative",
            "widened",
            "thread-cwd",
            "symlink-file",
        ]
    };
    let reached = route::run_long_with(CONNECT_LAST, &p, &bare, false).unwrap();
    assert_eq!(
        reached.route.name(),
        "fork-cwd-direct",
        "{:?}",
        reached.skipped
    );
    assert_eq!(names(&reached.skipped), order);
    ping_kept(&listener, reached.fd);

    let f = route::run_long_with(CONNECT, &p, &bare, false)
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG), "{f:?}");
    assert_eq!(f.route, None);
    assert_eq!(names(&f.skipped), order);
    assert_empty(place.path());
}

// The -sockdir routes are tried only when D was to be made and could be made
// nowhere (hub SPEC 445c1670): when no means through D can be used at all (here a file name too long for `R/<name>`), D is not made, so no
// -sockdir route is tried or reported, and the bind is ENAMETOOLONG with only
// the routes through D passed over. With no place for D at all, D being made
// nowhere must not be taken for a reason to try the -sockdir routes. The
// socket's directory is left untouched.
#[test]
fn no_sockdir_route_is_tried_when_d_is_not_made() {
    let t = tmp();
    let p = t.path().join("n".repeat(250));
    mark_untouched(t.path());
    // No place for D either, so that only the check on the means keeps the
    // -sockdir routes from being tried.
    let f = route::run_long_with(BIND, &p, &config(&[], &[]), false)
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG), "{f:?}");
    let names: Vec<_> = f.skipped.iter().map(|s| s.name).collect();
    assert!(names.iter().all(|n| !n.ends_with("-sockdir")), "{names:?}");
    assert!(names.contains(&"standard"), "{names:?}");
    assert_untouched(t.path());
}

// The spelling through D is `R/<name>`, 11 bytes more than the file name, in
// the plain sockaddr: a file name of DIRECT_MAX - 11 bytes (96 on
// Linux and illumos, 92 on Apple and the BSDs) binds through D and is reached;
// one byte more fits no bind spelling and is ENAMETOOLONG for the bind, with
// nothing made in the place (the length is checked before D is made). A
// connect to such a name still gets there by a route that keeps the file name
// out of the spelling (a socket bound under a short name and renamed).
#[test]
fn the_routes_through_d_take_a_file_name_up_to_sun_path_less_r() {
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let c = config(&[at(place.path())], &[links.path()]);
    let longest = DIRECT_MAX - 11;
    let fits = t.path().join("f".repeat(longest));
    let (l, r) = route::run_long(BIND, &fits, &c).unwrap();
    assert!(!matches!(r, Route::Standard), "{r:?}");
    let (cl, _) = route::run_long(CONNECT, &fits, &c).unwrap();
    ping(l, cl);

    let over = t.path().join("o".repeat(longest + 1));
    mark_untouched(place.path());
    let e = route::run_long(BIND, &over, &c).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
    assert_untouched(place.path());
    assert!(!is_socket(&over));
    let (l, _) = route::run_long(BIND, &t.path().join("s.sock"), &c).unwrap();
    std::fs::rename(t.path().join("s.sock"), &over).unwrap();
    let (cl, r) = route::run_long(CONNECT, &over, &c).unwrap();
    assert!(!r.forked(), "{r:?}");
    ping(l, cl);
}

// A bind / connect that a means through D made is the result: its error is
// returned, no later means is tried (the fork route, for a bind) and no later
// place, and a connect does not go on to the routes after it (the symlink).
// With two places for D and a place for a connect's symlink, and for the
// connects the routes before D off ([`no_direct`]): an existing file is
// EADDRINUSE, a missing socket ENOENT and a socket nobody listens on
// ECONNREFUSED; the second place for D and the place for the symlink stay
// untouched, the first is empty again. (Where the platform has no connect
// route through D, only the bind is checked here.)
//
// The same holds for a route before D: a connect whose first route (Linux
// proc-direct, FreeBSD at-direct, Apple widened for a path of at most 253
// bytes) fails for real returns that failure, and D is not made for the
// routes after it: both places for D stay untouched.
#[test]
fn a_failure_through_d_is_not_retried() {
    let t = tmp();
    let first = tmp();
    let second = tmp();
    let links = tmp();
    let c = config(&[at(first.path()), at(second.path())], &[links.path()]);
    let p = nest(t.path(), 600, "s.sock");
    let (_l, _) = route::run_long(BIND, &p, &c).unwrap();
    mark_untouched(second.path());
    mark_untouched(links.path());
    let e = route::run_long(BIND, &p, &c).unwrap_err();
    assert_eq!(errno(&e), Some(libc::EADDRINUSE), "{e}");
    let stale = p.with_file_name("stale.sock");
    stale_socket(&stale);
    if connects_through_d(thread_here()) {
        let through_d = no_direct(c.clone());
        for req in CONNECTS {
            let e = route::run_long(req, &p.with_file_name("x.sock"), &through_d).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENOENT), "{req:?}: {e}");
        }
        for req in CONNECTS {
            let e = route::run_long(req, &stale, &through_d).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ECONNREFUSED), "{req:?}: {e}");
        }
    }
    assert_untouched(second.path());
    assert_untouched(links.path());
    assert_empty(first.path());

    if cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd"
    )) {
        let near = nest(&t.path().join("near"), 200, "s.sock");
        let near_stale = near.with_file_name("stale.sock");
        stale_socket(&near_stale);
        mark_untouched(first.path());
        for req in CONNECTS {
            let f = route::run_long_with(req, &near.with_file_name("x.sock"), &c, false)
                .err()
                .unwrap();
            assert_eq!(errno(&f.error), Some(libc::ENOENT), "{req:?}: {f:?}");
            assert!(
                ["proc-direct", "at-direct", "widened"].contains(&f.route.unwrap_or_default()),
                "{req:?}: {f:?}"
            );
            let f = route::run_long_with(req, &near_stale, &c, false)
                .err()
                .unwrap();
            assert_eq!(errno(&f.error), Some(libc::ECONNREFUSED), "{req:?}: {f:?}");
        }
        assert_untouched(first.path());
        assert_untouched(second.path());
        assert_untouched(links.path());
    }
}

// ----- the connect order (SPEC "connect の順") -----

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
))]
thread_local! {
    /// How often a stand-in for opening D ([`Config::open_own_dir`]) was
    /// called on this thread: once for each D made in a place.
    static OWN_DIR_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How often D was made since the last call, by the counting stand-ins on this
/// thread ([`refuse_own_dir_counted`], the Linux `open_own_dir_high`). The
/// route order makes D on the calling thread, and each test runs on a thread of
/// its own, so the count is this test's.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
))]
fn own_dir_opens() -> usize {
    OWN_DIR_OPENS.with(|n| n.replace(0))
}

/// [`refuse_own_dir`], counted ([`own_dir_opens`]): D is made in the place,
/// cannot be opened, is removed again, and the place is passed over.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
))]
fn refuse_own_dir_counted(base: std::os::fd::BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    OWN_DIR_OPENS.with(|n| n.set(n.get() + 1));
    refuse_own_dir(base, name)
}

// A connect tries the routes that need neither writing nor a thread first
// (SPEC "connect の順", kawaz 裁定): when one of them reaches the socket, D is
// not made and no symlink either. With the thread-cwd means on and a place
// for D and one for the symlink, each connect reaches the socket by the
// platform's first route, and both places keep their mtime (nothing created
// in them or removed from them):
//
// * Linux with /proc: proc-direct (`/proc/self/fd/<the socket's directory's
//   fd>/<name>`);
// * Linux without /proc, NetBSD, OpenBSD, illumos: relative, for a path whose
//   spelling relative to the cwd fits;
// * Apple: relative the same way, and widened for a path of up to 253 bytes
//   that relative does not reach;
// * FreeBSD: at-direct (connectat with the file name on the socket's
//   directory's fd).
#[test]
fn a_connect_by_a_route_that_needs_no_writing_makes_nothing() {
    #[allow(unused_variables)]
    let t = tmp();
    let bind_place = tmp();
    let place = tmp();
    let links = tmp();
    #[cfg(not(target_os = "freebsd"))]
    let below = tmp_below_cwd();
    #[cfg(not(target_os = "freebsd"))]
    let near = {
        let cwd = std::env::current_dir().unwrap();
        let rel_base = below.path().strip_prefix(&cwd).unwrap().to_path_buf();
        cwd.join(nest(&rel_base, DIRECT_MAX, "rel.sock"))
    };
    let c = config(&[at(place.path())], &[links.path()]);
    #[allow(unused_mut)]
    let mut cases: Vec<(PathBuf, Config, Route)> = Vec::new();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        assert!(Path::new("/proc/self/fd").exists(), "the premise: /proc");
        cases.push((
            nest(t.path(), 400, "proc.sock"),
            c.clone(),
            Route::ProcDirect,
        ));
        let no_proc = Config {
            proc_self_fd: PathBuf::from("/nonexistent/proc/self/fd"),
            ..c.clone()
        };
        cases.push((near, no_proc, Route::Relative));
    }
    #[cfg(target_vendor = "apple")]
    {
        cases.push((near, c.clone(), Route::Relative));
        cases.push((nest(t.path(), 200, "wide.sock"), c.clone(), Route::Widened));
    }
    #[cfg(target_os = "freebsd")]
    cases.push((nest(t.path(), 400, "at.sock"), c.clone(), Route::AtDirect));
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple",
        target_os = "freebsd"
    )))]
    cases.push((near, c.clone(), Route::Relative));
    for (p, c, want) in cases {
        let (l, _) = route::run_long(BIND, &p, &config(&[at(bind_place.path())], &[])).unwrap();
        let listener = UnixListener::from(l);
        mark_untouched(place.path());
        mark_untouched(links.path());
        for req in CONNECTS {
            let (cl, r) = route::run_long(req, &p, &c).unwrap();
            assert_eq!(r, want, "{req:?}");
            ping_kept(&listener, cl);
        }
        assert_untouched(place.path());
        assert_untouched(links.path());
    }
}

// D is made at most once in a call (SPEC "D と R"): where it could be made
// nowhere, every route through D of the connect order is passed over for
// that (D is not tried again for the next one), and the connect goes on to
// the next route that needs no D. With the routes that need neither writing
// nor a thread unusable (the socket's directory cannot be opened, the widened
// sockaddr is off, the path too far from the cwd for relative), one place for
// D where D is made but cannot be opened (so it is removed and the place
// passed over) and a place for the symlink:
//
// * Linux with /proc: proc-direct, thread-cwd, proc and proc-held are passed
//   over, and the symlink reaches the socket;
// * Apple: relative, widened and thread-cwd, then the symlink;
// * FreeBSD: at-direct and at, then ENAMETOOLONG (FreeBSD has no symlink
//   route).
//
// D is made once (the place is touched and empty again), and every route
// through D after the one that tried has "no place for D" as its reason.
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "freebsd"
))]
#[test]
fn with_d_made_nowhere_every_route_through_d_is_passed_over_and_d_tried_once() {
    let t = tmp();
    let bind_place = tmp();
    let place = tmp();
    let links = tmp();
    #[allow(unused_mut)]
    let mut c = Config {
        open_socket_dir: refuse_socket_dir,
        open_own_dir: refuse_own_dir_counted,
        ..config(&[at(place.path())], &[links.path()])
    };
    #[cfg(target_vendor = "apple")]
    {
        c.wide = false;
    }
    let p = nest(t.path(), 400, "s.sock");
    let (l, _) = route::run_long(BIND, &p, &config(&[at(bind_place.path())], &[])).unwrap();
    let listener = UnixListener::from(l);
    mark_untouched(place.path());
    own_dir_opens();
    let result = route::run_long_with(CONNECT, &p, &c, false);
    let skipped = match &result {
        Ok(r) => &r.skipped,
        Err(f) => &f.skipped,
    };
    let names: Vec<&str> = skipped.iter().map(|s| s.name).collect();
    let (want, through_d): (&[&str], &[&str]) =
        if cfg!(any(target_os = "linux", target_os = "android")) {
            (
                &["standard", "proc-direct", "thread-cwd", "proc", "proc-held"],
                &["thread-cwd", "proc", "proc-held"],
            )
        } else if cfg!(target_vendor = "apple") {
            (
                &["standard", "relative", "widened", "thread-cwd"],
                &["thread-cwd"],
            )
        } else {
            (&["standard", "at-direct", "at"], &["at"])
        };
    assert_eq!(names, want, "{skipped:?}");
    assert_eq!(own_dir_opens(), 1, "{skipped:?}");
    assert_touched(place.path());
    assert_empty(place.path());
    let first_try = skipped
        .iter()
        .position(|s| through_d.contains(&s.name) && s.reason == route::names::NO_PLACE)
        .unwrap_or_else(|| panic!("no route through D found no place: {skipped:?}"));
    for s in &skipped[first_try..] {
        assert_eq!(s.reason, route::names::NO_PLACE, "{skipped:?}");
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        let r = result.unwrap();
        assert_eq!(r.route.name(), "symlink-file");
        ping_kept(&listener, r.fd);
        assert_empty(links.path());
    }
    #[cfg(target_os = "freebsd")]
    {
        let _ = &listener;
        let f = result.err().unwrap();
        assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG), "{f:?}");
    }
}

// The connect order's routes that need D now and later — on Linux with /proc
// the proc route, whose spelling holds R, and proc-held after it, whose
// spelling is `s` in D — share one D made once in the call (SPEC "D と R"):
// D is not removed and made again between them. In a child process (so the
// fds the route order opens get small numbers), D is opened by a stand-in
// that counts and gives D's fd a number of 4 digits; the file name, 79 bytes,
// lets `/proc/self/fd/<fd>/R/<name>` fit with the number the route order
// estimates before making D (at most 2 digits here) but not with D's own, so
// the proc route makes D and passes it over as too long once it sees D's fd,
// and proc-held reaches the socket through `s` in that same D. The
// thread-cwd means is off and the socket's directory cannot be opened (no
// proc-direct), so the order is proc-direct, thread-cwd, proc passed over,
// then proc-held; D was made once, and the place is empty again.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn d_is_made_once_for_the_routes_through_it() {
    let t = tmp();
    let place = tmp();
    let p = nest(t.path(), 400, &"m".repeat(79));
    let (l, _) = route::run_long(BIND, &p, &config(&[at(place.path())], &[])).unwrap();
    let listener = UnixListener::from(l);
    run_child(
        "tests::d_made_once_child",
        Path::new("/"),
        &[
            ("SOCKAT_CHILD_PATH", p.as_os_str()),
            ("SOCKAT_CHILD_PLACE", place.path().as_os_str()),
        ],
    );
    let (_s, _) = listener.accept().unwrap();
    assert_empty(place.path());
}

/// Open D as the route order does, with its fd moved to a number of 4 digits,
/// counting the calls ([`own_dir_opens`]).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn open_own_dir_high(base: std::os::fd::BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    use std::os::fd::AsFd;
    OWN_DIR_OPENS.with(|n| n.set(n.get() + 1));
    let fd = own::open_own_dir(base, name)?;
    sys::dup_at_least(fd.as_fd(), 1000)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn d_made_once_child() {
    if !in_child() {
        return;
    }
    let p = env_path("SOCKAT_CHILD_PATH");
    let place = env_path("SOCKAT_CHILD_PLACE");
    let c = Config {
        open_socket_dir: refuse_socket_dir,
        open_own_dir: open_own_dir_high,
        ..no_thread_cwd(config(&[at(&place)], &[]))
    };
    own_dir_opens();
    let reached = route::run_long_with(CONNECT, &p, &c, false).unwrap();
    assert_eq!(reached.route, Route::ProcHeld, "{:?}", reached.skipped);
    assert_eq!(own_dir_opens(), 1, "{:?}", reached.skipped);
    let names: Vec<&str> = reached.skipped.iter().map(|s| s.name).collect();
    assert_eq!(
        names,
        ["standard", "proc-direct", "thread-cwd", "proc"],
        "{:?}",
        reached.skipped
    );
    assert_eq!(reached.skipped[3].reason, route::names::TOO_LONG);
    assert_empty(&place);
}

// ----- a bind through symlink-file (hub DR-0016) -----

// A bind tries symlink-file right after standard where a bind may follow a
// symlink (the Apple OSes and NetBSD, measured to follow, and the OSes not
// measured): with the bind's symlink-file on and no place for its symlink, a
// long path's bind passes over standard, then symlink-file, and goes on
// through D. On the OSes measured not to follow (Linux / Android, FreeBSD,
// OpenBSD, illumos) the route is not in the bind's order: symlink-file is not
// reported and the bind goes through D after standard.
#[test]
fn a_bind_tries_symlink_file_right_after_standard_where_a_bind_may_follow_it() {
    let t = tmp();
    let place = tmp();
    let c = config(&[at(place.path())], &[]);
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "illumos"
    )))]
    let c = with_symlink_file_bind(c);
    let p = nest(t.path(), 400, "s.sock");
    let reached = route::run_long_with(BIND, &p, &c, false).unwrap();
    let names: Vec<&str> = reached.skipped.iter().map(|s| s.name).collect();
    if cfg!(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "illumos"
    ))) {
        assert_eq!(names[..2], ["standard", "symlink-file"], "{names:?}");
        assert_eq!(reached.skipped[1].reason, route::names::NO_SAFE_BASE);
    } else {
        assert_eq!(names.first(), Some(&"standard"), "{names:?}");
        assert!(!names.contains(&"symlink-file"), "{names:?}");
    }
    assert!(
        route_through_d(BIND, thread_here(), reached.route),
        "{:?}",
        reached.route
    );
    assert!(is_socket(&p));
}

// On macOS and NetBSD a bind follows a symlink in the last component, so the
// bind through symlink-file reaches the path whatever the file name length:
// the spelling, `<place>/sockat.XXXXXXXXXX`, holds no file name (hub
// DR-0016). File names of 92 bytes (the longest the routes through D take on
// both), 93 (one more), 200 and 254 (NAME_MAX - 1) each bind by symlink-file, with
// the socket at the path given and reached; what it gives up is `respelled`
// alone (the place is private here; a shared one gives up nothing more); the
// address the kernel reports is the symlink's spelling in the place, which
// names nothing once the call has returned (removing by it is ENOENT, and the
// socket file stays); what bind_ex keeps is the socket file at the path; the
// place for D is untouched (no D was made) and the place for the symlink is
// empty again.
#[cfg(any(target_os = "macos", target_os = "netbsd"))]
#[test]
fn a_bind_through_symlink_file_reaches_any_file_name_length() {
    use std::os::unix::fs::MetadataExt;
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let c = with_symlink_file_bind(config(&[at(place.path())], &[links.path()]));
    let links_real = std::fs::canonicalize(links.path()).unwrap();
    mark_untouched(place.path());
    for len in [92, 93, 200, 254] {
        let p = t.path().join("f".repeat(len));
        let r = route::run_long_with(BIND, &p, &c, true).unwrap();
        assert_eq!(r.route, Route::SymlinkFile, "{len}: {:?}", r.skipped);
        assert_eq!(
            crate::losses(true, r.route, &p),
            [crate::Loss::Respelled],
            "{len}"
        );
        assert!(is_socket(&p), "{len}");
        let bound = r.bound.unwrap();
        let m = std::fs::symlink_metadata(&p).unwrap();
        assert_eq!(bound.path, std::fs::canonicalize(&p).unwrap(), "{len}");
        assert_eq!(bound.id, (m.dev(), m.ino()), "{len}");
        let listener = UnixListener::from(r.fd);
        let shown = reported(&listener);
        let shown_dir = std::fs::canonicalize(shown.parent().unwrap()).unwrap();
        assert_eq!(shown_dir, links_real, "{len}: {shown:?}");
        assert!(
            is_drawn(shown.file_name().unwrap(), own::PREFIX),
            "{len}: {shown:?}"
        );
        let e = std::fs::remove_file(&shown).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{len}");
        assert!(is_socket(&p), "{len}");
        let (cl, _) = route::run_long(CONNECT, &p, &config(&[], &[links.path()])).unwrap();
        ping_kept(&listener, cl);
        assert_empty(links.path());
    }
    assert_untouched(place.path());
}

// A bind through symlink-file that fails with something at the path is the
// real failure (hub DR-0016, SPEC "経路の失敗の扱い"): with a file already at
// the path, the bind is EADDRINUSE by the route symlink-file, the file is
// left as it was, and no later route is tried (the place for D stays
// untouched) nor a later place for the symlink.
#[cfg(any(target_os = "macos", target_os = "netbsd"))]
#[test]
fn a_bind_through_symlink_file_onto_an_existing_file_is_the_failure() {
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let second_links = tmp();
    let c = with_symlink_file_bind(config(
        &[at(place.path())],
        &[links.path(), second_links.path()],
    ));
    let p = t.path().join("e".repeat(200));
    std::fs::write(&p, b"keep").unwrap();
    mark_untouched(place.path());
    mark_untouched(second_links.path());
    let f = route::run_long_with(BIND, &p, &c, false).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::EADDRINUSE), "{f:?}");
    assert_eq!(f.route, Some("symlink-file"), "{f:?}");
    assert_eq!(std::fs::read(&p).unwrap(), b"keep");
    assert_untouched(place.path());
    assert_untouched(second_links.path());
    assert_empty(links.path());
}

// A bind through symlink-file that fails with nothing at the path is taken
// for a kernel that did not follow the symlink (hub DR-0016): the route is
// passed over ("did not follow"), and the next route is tried; a real failure
// fails that one the same way and is returned there. A socket's directory
// without write permission (0500) makes the bind through the symlink fail
// with EACCES and nothing at the path, so the bind goes on to the next route
// (thread-cwd on macOS, fork-cwd on NetBSD), which fails with EACCES for
// real. Root bypasses permission checks, so as root the test cannot tell
// anything and says so.
#[cfg(any(target_os = "macos", target_os = "netbsd"))]
#[test]
fn a_bind_through_symlink_file_failing_with_nothing_at_the_path_goes_on() {
    use std::os::unix::fs::PermissionsExt;
    if sys::effective_uid() == 0 {
        eprintln!("skipped: running as root, which bypasses the permission under test");
        return;
    }
    let t = tmp();
    let place = tmp();
    let links = tmp();
    let c = with_symlink_file_bind(config(&[at(place.path())], &[links.path()]));
    let dir = t.path().join("ro");
    std::fs::create_dir(&dir).unwrap();
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    let _restore = Restore(dir.clone());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let p = dir.join("s".repeat(80));
    let f = route::run_long_with(BIND, &p, &c, false).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::EACCES), "{f:?}");
    let next = if cfg!(target_os = "macos") {
        "thread-cwd"
    } else {
        "fork-cwd"
    };
    assert_eq!(f.route, Some(next), "{f:?}");
    let skipped: Vec<_> = f.skipped.iter().map(|s| (s.name, s.reason)).collect();
    assert_eq!(
        skipped,
        [
            ("standard", route::names::TOO_LONG),
            ("symlink-file", route::names::SYMLINK_NOT_FOLLOWED),
        ]
    );
    assert!(!p.exists());
    assert_empty(links.path());
    assert_empty(place.path());
}

// The `TMPDIR` the public order reads is the first place for D, and it may be
// relative (it is opened once, against the cwd of the moment, and held by its
// fd; no spelling names it) or far too deep for any sockaddr (D is never in a
// spelling). Two children of this binary run with such a TMPDIR and the public
// order (Config::from_env): a call through D makes D in TMPDIR (its mtime
// changes, it is empty afterwards). On Linux, FreeBSD, OpenBSD and illumos the
// bind goes through D; on macOS the bind takes symlink-file first, and the
// connect (600 bytes, too long for relative and widened) goes through D. On
// NetBSD the bind takes symlink-file first and the connect order has no route
// through D (relative, symlink-file, fork-cwd-direct), so no call of the public
// order goes through D there: the child turns the bind's symlink-file off,
// and the bind goes through D with the places read from TMPDIR as before. A relative TMPDIR is left out of the places for a
// connect's symlink, a deep one is passed over there (the spelling would not
// fit); that is checked with the symlink route.
#[test]
fn tmpdir_may_be_relative_or_deep_for_d() {
    let t = tmp();
    let cwd = tmp();
    let rel = cwd.path().join("rel");
    std::fs::create_dir(&rel).unwrap();
    let deep_base = tmp();
    let deep = nest(deep_base.path(), 600, "tmpdir");
    std::fs::create_dir(&deep).unwrap();
    for (i, (tmpdir, place)) in [(Path::new("rel"), &rel), (deep.as_path(), &deep)]
        .into_iter()
        .enumerate()
    {
        let p = nest(&t.path().join(i.to_string()), 600, "s.sock");
        mark_untouched(place);
        run_child(
            "tests::child_bind_and_connect_from_env",
            cwd.path(),
            &[
                ("TMPDIR", tmpdir.as_os_str()),
                ("SOCKAT_CHILD_PATH", p.as_os_str()),
            ],
        );
        assert_touched(place);
        assert_empty(place);
    }
}

#[test]
fn child_bind_and_connect_from_env() {
    if !in_child() {
        return;
    }
    let p = env_path("SOCKAT_CHILD_PATH");
    #[allow(unused_mut)]
    let mut c = Config::from_env();
    #[cfg(target_os = "netbsd")]
    {
        c.symlink_file_bind = false;
    }
    let (l, r) = route::run_long(BIND, &p, &c).unwrap();
    println!("sockat-child: bind {r:?}");
    let (cl, r) = route::run_long(CONNECT_LAST, &p, &c).unwrap();
    println!("sockat-child: connect {r:?}");
    ping(l, cl);
}

// The places the public order reads: D goes in `${TMPDIR:-/tmp}` (an empty
// TMPDIR is unset; a relative one is used as it is), then /tmp unless that is
// the same spelling (written with or without a trailing slash), then the
// socket's directory. A connect's symlink goes in TMPDIR only when it is
// absolute, then /tmp, once when TMPDIR is /tmp. The symlink of bind_ex with the last resort goes
// in XDG_RUNTIME_DIR, TMPDIR (each only when absolute; empty is unset) and
// /tmp, each spelling once (SPEC "symlink の置き場と名前"). Outside the tests
// the widened sockaddr (Apple) and the thread-cwd means (Linux, Apple) are
// on, and /proc is looked for where it is (Linux).
#[test]
fn config_reads_tmpdir_then_tmp() {
    use std::ffi::OsString;
    let c = |tmpdir: Option<&str>| Config::from_vars(tmpdir.map(OsString::from), None);
    let paths = |ps: &[&str]| ps.iter().map(PathBuf::from).collect::<Vec<_>>();
    let both = |tmpdir: Option<&str>, runtime: Option<&str>| {
        Config::from_vars(tmpdir.map(OsString::from), runtime.map(OsString::from)).dir_link_places
    };
    assert_eq!(both(None, None), paths(&["/tmp"]));
    assert_eq!(both(Some(""), Some("")), paths(&["/tmp"]));
    assert_eq!(
        both(Some("/var/t"), Some("/run/user/1")),
        paths(&["/run/user/1", "/var/t", "/tmp"])
    );
    assert_eq!(both(Some("rel"), Some("run")), paths(&["/tmp"]));
    assert_eq!(both(Some("/tmp/"), Some("/tmp")), paths(&["/tmp"]));
    assert_eq!(both(Some("/x"), Some("/x/")), paths(&["/x", "/tmp"]));

    let places = |ps: &[&str]| {
        ps.iter()
            .map(|p| at(Path::new(p)))
            .chain([Place::SocketDir])
            .collect::<Vec<_>>()
    };
    assert_eq!(c(None).own_dir_places, places(&["/tmp"]));
    assert_eq!(c(Some("")).own_dir_places, places(&["/tmp"]));
    assert_eq!(
        c(Some("/var/x/")).own_dir_places,
        places(&["/var/x/", "/tmp"])
    );
    assert_eq!(c(Some("rel")).own_dir_places, places(&["rel", "/tmp"]));
    assert_eq!(c(Some("/tmp")).own_dir_places, places(&["/tmp"]));
    assert_eq!(c(Some("/tmp/")).own_dir_places, places(&["/tmp"]));
    #[cfg(not(target_os = "freebsd"))]
    {
        let links = |ps: &[&str]| ps.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(c(None).file_link_places, links(&["/tmp"]));
        assert_eq!(c(Some("")).file_link_places, links(&["/tmp"]));
        assert_eq!(
            c(Some("/var/x/")).file_link_places,
            links(&["/var/x/", "/tmp"])
        );
        assert_eq!(c(Some("rel")).file_link_places, links(&["/tmp"]));
        assert_eq!(c(Some("/tmp")).file_link_places, links(&["/tmp"]));
        assert_eq!(c(Some("/tmp/")).file_link_places, links(&["/tmp"]));
    }
    #[cfg(target_vendor = "apple")]
    assert!(c(None).wide);
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    assert!(c(None).thread_cwd);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    assert_eq!(
        c(None).proc_self_fd,
        Path::new(crate::route::linux::PROC_SELF_FD)
    );
}

// ----- sockat's own directory D, made and held (every platform) -----
//
// D is made with mkdirat in a place, then opened by name without following a
// symlink and checked: a directory, owned by the effective uid, no permission
// for group and others. Between the two steps anyone who can write to the
// place can put something else at the name; that is refused and left alone.

/// The flags-to-be of `hold`: where /proc is when this platform opens D
/// O_PATH (Linux with /proc), else `None`.
fn hold_proc() -> Option<&'static Path> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if Path::new(route::linux::PROC_SELF_FD).exists() {
        return Some(Path::new(route::linux::PROC_SELF_FD));
    }
    None
}

/// A directory `name` under `parent` that anyone may write to and that is
/// not sticky (mode 0777): a place where others can replace entries.
fn open_place(parent: &Path, name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let d = parent.join(name);
    std::fs::create_dir(&d).unwrap();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o777)).unwrap();
    d
}

// What is put at D's name between mkdirat and openat must be neither used nor
// removed. Each replacement is made by hand between the two steps:
//
// * the directory with group and others given read and search (0755);
// * a symlink to another directory (O_NOFOLLOW refuses it, and nothing
//   appears in that directory);
// * a plain file;
// * (as root only) a directory owned by another user.
//
// Unreplaced, the same steps hold D (mode 0700), the symlink made in it
// points to the target, and dropping the value removes both.
#[test]
fn a_directory_replaced_before_it_is_held_is_not_used() {
    use std::os::fd::AsFd;
    use std::os::unix::fs::PermissionsExt;
    let places = tmp();
    let place = open_place(places.path(), "place");
    let other = tmp();
    let target = other.path().join("target");
    let base = || route::open_dir(&place).unwrap();
    let name = || Ok(String::from("sockat.AAAAAAAAAA"));
    let made = || place.join(own::OwnDir::make(base().as_fd(), name).unwrap());
    let hold = |p: &Path| {
        own::OwnDir::hold(
            base(),
            p.file_name().unwrap().to_os_string(),
            hold_proc(),
            own::open_own_dir,
        )
    };

    let d = made();
    let mode_of = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode_of(&d), 0o700);
    let held = hold(&d).unwrap();
    held.with_r(&target).unwrap();
    let links: Vec<_> = std::fs::read_dir(&d)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(links.len(), 1);
    assert_eq!(std::fs::read_link(&links[0]).unwrap(), target);
    drop(held);
    assert!(std::fs::symlink_metadata(&d).is_err());

    let d = made();
    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    let e = hold(&d).err().unwrap();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    assert_empty(&d);
    std::fs::remove_dir(&d).unwrap();

    let d = made();
    std::fs::remove_dir(&d).unwrap();
    std::os::unix::fs::symlink(other.path(), &d).unwrap();
    let e = hold(&d).err().unwrap();
    // The errno of O_NOFOLLOW on a symlink differs per OS; any of them is the
    // refusal that matters here. POSIX says ELOOP; with O_DIRECTORY as well,
    // Linux answers ENOTDIR (observed in docker, Linux 7.2); FreeBSD answers
    // EMLINK and NetBSD EFTYPE (their open(2), observed in the CI's VMs).
    #[cfg(target_os = "freebsd")]
    let refusals = [libc::ENOTDIR, libc::ELOOP, libc::EMLINK];
    #[cfg(target_os = "netbsd")]
    let refusals = [libc::ENOTDIR, libc::ELOOP, libc::EFTYPE];
    #[cfg(not(any(target_os = "freebsd", target_os = "netbsd")))]
    let refusals = [libc::ENOTDIR, libc::ELOOP];
    assert!(errno(&e).is_some_and(|n| refusals.contains(&n)), "{e}");
    assert_empty(other.path());
    assert!(
        std::fs::symlink_metadata(&d)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    std::fs::remove_file(&d).unwrap();

    let d = made();
    std::fs::remove_dir(&d).unwrap();
    std::fs::write(&d, b"").unwrap();
    let e = hold(&d).err().unwrap();
    assert_eq!(errno(&e), Some(libc::ENOTDIR), "{e}");
    assert!(d.is_file());
    std::fs::remove_file(&d).unwrap();

    if sys::effective_uid() != 0 {
        eprintln!("skipped the other user's directory: only root can chown");
        return;
    }
    let d = made();
    std::os::unix::fs::chown(&d, Some(65534), None).unwrap();
    let e = hold(&d).err().unwrap();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
    assert_empty(&d);
    std::fs::remove_dir(&d).unwrap();
}

// What fails the checks is someone else's, and so is everything in it: it is
// neither used nor emptied. A replacement that already holds a symlink and a
// file of its own keeps them (only a link sockat's own symlinkat made is ever
// removed). Each replacement is a directory put at the name between the two
// steps, given away by group / other permission (0755) or (as root only) by
// another owner.
#[test]
fn a_replaced_directory_is_left_with_its_contents() {
    use std::os::fd::AsFd;
    use std::os::unix::fs::PermissionsExt;
    let places = tmp();
    let place = open_place(places.path(), "place");
    let other = tmp();
    let theirs = other.path().join("theirs");
    let base = || route::open_dir(&place).unwrap();
    let name = || Ok(String::from("sockat.BBBBBBBBBB"));
    let replaced = |give_away: &dyn Fn(&Path)| {
        let n = own::OwnDir::make(base().as_fd(), name).unwrap();
        let d = place.join(&n);
        give_away(&d);
        std::os::unix::fs::symlink(&theirs, d.join("XXXXXXXXXX")).unwrap();
        std::fs::write(d.join("note"), b"theirs").unwrap();
        let e = own::OwnDir::hold(base(), n, hold_proc(), own::open_own_dir)
            .err()
            .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        assert_eq!(std::fs::read_link(d.join("XXXXXXXXXX")).unwrap(), theirs);
        assert_eq!(std::fs::read(d.join("note")).unwrap(), b"theirs");
        std::fs::remove_file(d.join("XXXXXXXXXX")).unwrap();
        std::fs::remove_file(d.join("note")).unwrap();
        std::fs::remove_dir(&d).unwrap();
    };
    replaced(&|d| std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap());
    if sys::effective_uid() != 0 {
        eprintln!("skipped the other user's directory: only root can chown");
        return;
    }
    replaced(&|d| std::os::unix::fs::chown(d, Some(65534), None).unwrap());
}

// D is reached through its fd, not its name. Someone renaming it away and
// putting their own directory at its name changes nothing for the call: a bind
// with `R/<name>` relative to D's fd (here by the fork route, which every
// platform has) still lands in the socket's directory. And the clean-up removes
// R through the fd, but removes a directory by the name only while the name is
// still D (same st_dev / st_ino as the fd): the directory put in its place is
// left alone, and the renamed D, now empty, stays where it was moved to.
#[test]
fn d_renamed_away_is_still_used_and_its_replacement_is_not_removed() {
    use std::os::fd::AsFd;
    let t = tmp();
    let places = tmp();
    let place = open_place(places.path(), "place");
    let target = std::fs::canonicalize(t.path()).unwrap();
    let socket_dir = route::open_dir(t.path()).unwrap();
    let d = own::OwnDir::create(
        &at(&place),
        Some(socket_dir.as_fd()),
        hold_proc(),
        own::open_own_dir,
    )
    .unwrap();
    let anchor = d.with_r(&target).unwrap();
    let named = place.join(&d.name);
    let moved = place.join("moved");
    std::fs::rename(&named, &moved).unwrap();
    std::fs::create_dir(&named).unwrap();

    let addr = SockAddr::standard(Path::new(anchor.link).join("s.sock").as_os_str()).unwrap();
    let sock = sys::socket(false).unwrap();
    sys::op_in_dir_forked(sys::Op::Bind, sock.as_fd(), anchor.fd, &addr).unwrap();
    sys::listen(sock.as_fd()).unwrap();
    assert!(is_socket(&t.path().join("s.sock")));
    let c = UnixStream::connect(t.path().join("s.sock")).unwrap();
    ping(sock, OwnedFd::from(c));

    drop(d);
    assert!(named.is_dir(), "the directory put in its place was removed");
    assert_empty(&named);
    assert_empty(&moved);
}

// The 0700 asked for when D is made is what the umask leaves of it. A umask
// that takes some of the owner's bits must not make the routes through D fail
// on every call and leave a D behind each time:
//
// * 0277 makes D 0500 (no write): only the group and other bits are judged
//   (none, as a umask cannot add any), and the owner's are set to 0700 before
//   R is made in it: fchmod, or for an O_PATH fd (Linux) chmod through
//   /proc/self/fd/<fd>, or without /proc an fchmod of D opened again by name
//   O_RDONLY and checked to be the same directory;
// * 0477 makes D 0300 (no read either): D is opened O_SEARCH on Apple and
//   O_PATH on Linux, which need no read permission, so it is still checked.
//   On Linux with /proc it is fixed and used. On Linux without /proc the open
//   by name O_RDONLY fails, so D is removed again and the place passed over
//   (the bind then goes on beside the socket). Elsewhere D is opened
//   O_RDONLY, which needs the read bit: D cannot be opened at all;
// * 0177 makes D 0600 (no search): O_SEARCH (Apple) needs the search bit, so
//   there D cannot be opened at all; O_PATH and O_RDONLY open it.
//
// A D that cannot be opened is removed by its name, as it passes the checks
// there (D12-Q5), and the place passed over: the bind goes on beside the
// socket. So under every umask a bind and a connect reach their sockets and nothing
// is left in the place or beside the sockets. The connect goes to a listener
// bound here with the usual umask (a socket file bound under umask 0277 is
// 0500, which its owner cannot connect to, with or without sockat). Root
// ignores the permissions, so
// the missing bits only matter when this runs as another user (macOS, or
// setpriv in docker); as root it still checks the route and the clean-up. On
// Linux each umask runs with the real /proc and with /proc made to look
// absent. Runs in a child process because the umask is process-wide.
#[test]
fn a_umask_that_takes_the_owners_bits_leaves_nothing_behind() {
    let umasks: &[&str] = &["277", "477", "177"];
    let procs: &[&str] = if cfg!(any(target_os = "linux", target_os = "android")) {
        &["real", "none"]
    } else {
        &["real"]
    };
    for umask in umasks {
        for proc in procs {
            let t = tmp();
            let place = tmp();
            let p = nest(t.path(), 600, "s.sock");
            let q = p.with_file_name("q.sock");
            let (l, _) = route::run(BIND, &q).unwrap();
            let listener = UnixListener::from(l);
            listener.set_nonblocking(true).unwrap();
            mark_untouched(place.path());
            run_child(
                "tests::child_bind_and_connect_under_umask",
                Path::new("/"),
                &[
                    ("SOCKAT_CHILD_PATH", p.as_os_str()),
                    ("SOCKAT_CHILD_TO", q.as_os_str()),
                    ("SOCKAT_CHILD_OTHER", place.path().as_os_str()),
                    ("SOCKAT_CHILD_UMASK", OsStr::new(umask)),
                    ("SOCKAT_CHILD_PROC", OsStr::new(proc)),
                ],
            );
            assert!(is_socket(&p), "{umask} {proc}");
            // The child's connection is in the backlog (its end closed since).
            listener.accept().unwrap();
            assert_touched(place.path());
            assert_empty(place.path());
            assert_none_named(p.parent().unwrap(), own::HIDDEN_PREFIX);
        }
    }
}

#[test]
fn child_bind_and_connect_under_umask() {
    use std::os::fd::AsFd;
    use std::os::unix::fs::PermissionsExt;
    if !in_child() {
        return;
    }
    let p = env_path("SOCKAT_CHILD_PATH");
    let q = env_path("SOCKAT_CHILD_TO");
    let place = env_path("SOCKAT_CHILD_OTHER");
    let umask = std::env::var("SOCKAT_CHILD_UMASK").unwrap();
    let umask = u32::from_str_radix(&umask, 8).unwrap();
    sys::set_umask(umask);
    // The premise: D comes out with the owner's bits cut.
    let base = route::open_dir(&place).unwrap();
    let made = own::OwnDir::make(base.as_fd(), || Ok(String::from("sockat.AAAAAAAAAA"))).unwrap();
    let mode = std::fs::metadata(place.join(&made))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700 & !umask);
    std::fs::remove_dir(place.join(&made)).unwrap();

    #[allow(unused_mut)]
    let mut c = config(&[at(&place)], &[]);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if std::env::var("SOCKAT_CHILD_PROC").unwrap() == "none" {
        c.proc_self_fd = PathBuf::from("/nonexistent/proc/self/fd");
    }
    let (l, r) = route::run_long(BIND, &p, &c).unwrap();
    println!("sockat-child: bind {r:?}");
    assert_empty(&place);
    // Through D (R with no prefix in the spelling), except, as a user other
    // than root, where D cannot be opened (Apple without the owner's search
    // bit, the other targets but Linux without the read bit) or, on Linux
    // without /proc, cannot be fixed (without the read bit): there the bind
    // goes on with R beside the socket.
    let no_read = umask & 0o400 != 0;
    let without_proc = std::env::var("SOCKAT_CHILD_PROC").unwrap() == "none";
    let unopenable = if cfg!(any(target_os = "linux", target_os = "android")) {
        no_read && without_proc
    } else if cfg!(target_vendor = "apple") {
        umask & 0o100 != 0
    } else {
        no_read
    };
    let beside = unopenable && sys::effective_uid() != 0;
    #[cfg(not(target_os = "openbsd"))]
    {
        let shown = reported(&UnixListener::from(l));
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let shown = match shown.strip_prefix(route::linux::PROC_SELF_FD) {
            Ok(rest) => rest.iter().skip(1).collect(),
            Err(_) => shown,
        };
        let prefix = if beside { own::HIDDEN_PREFIX } else { "" };
        assert!(through_link(&shown, prefix, "s.sock"), "{shown:?}");
    }
    #[cfg(target_os = "openbsd")]
    let _ = (l, beside);
    let (_c, r) = route::run_long(CONNECT_LAST, &q, &c).unwrap();
    println!("sockat-child: connect {r:?}");
    assert_empty(&place);
}

// The names sockat draws come from the system's secure generator
// (arc4random_buf, getrandom(2) on Linux, /dev/urandom elsewhere). It must fill
// the whole buffer, and two draws must differ: a generator that left the
// buffer as it was, or returned the same bytes each time, would make every
// name collide (D, R and the symlink could then never be made). 32 bytes make
// an accidental match or an all-zero draw impossible in practice.
#[test]
fn the_random_bytes_fill_the_buffer_and_differ_between_draws() {
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    sys::random_bytes(&mut a).unwrap();
    sys::random_bytes(&mut b).unwrap();
    assert_ne!(a, [0u8; 32]);
    assert_ne!(a, b);
}

// A name that is already taken (by anything) is skipped for the next one, and
// the existing entry is left as it was: mkdirat / symlinkat fail with EEXIST
// rather than replacing it. When every name tried is taken, the attempt ends
// after own::ATTEMPTS (16) tries with AlreadyExists (D, R or the symlink then
// cannot be made, and the next place is tried); it does not loop forever. The
// names come from a stand-in generator so the collisions are certain.
#[test]
fn a_taken_name_is_skipped_and_the_tries_are_bounded() {
    use std::os::fd::AsFd;
    let place = tmp();
    let taken = format!("{}AAAAAAAAAA", own::PREFIX);
    let free = format!("{}BBBBBBBBBB", own::PREFIX);
    std::fs::write(place.path().join(&taken), b"keep").unwrap();
    let base = route::open_dir(place.path()).unwrap();
    let mut names = vec![taken.clone(), free.clone()].into_iter();
    let mut calls = 0;
    let made = own::OwnDir::make(base.as_fd(), || {
        calls += 1;
        Ok(names.next().unwrap())
    })
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(made, std::ffi::OsString::from(&free));
    assert!(place.path().join(&free).is_dir());
    assert_eq!(std::fs::read(place.path().join(&taken)).unwrap(), b"keep");

    let mut calls = 0;
    let e = own::OwnDir::make(base.as_fd(), || {
        calls += 1;
        Ok(taken.clone())
    })
    .err()
    .unwrap();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(calls, own::ATTEMPTS);
    assert_eq!(std::fs::read(place.path().join(&taken)).unwrap(), b"keep");
}

// ---------------------------------------------------------------------------
// FreeBSD (bindat / connectat)
// ---------------------------------------------------------------------------

#[cfg(target_os = "freebsd")]
mod freebsd {
    use super::*;

    // FreeBSD's bind goes through D by bindat with `R/<name>` on D's fd. A
    // connect takes at-direct first (connectat with the bare name on the
    // socket's directory's fd), which leaves the place for D untouched, and
    // at (connectat with `R/<name>` on D's fd) only when at-direct cannot be
    // used, here the socket's directory not opened ([`no_direct`]); none
    // forks. A file name too long for `R/<name>` (93 to 103 bytes) is still
    // connected to by at-direct; its bind is ENAMETOOLONG. With D made
    // nowhere, a bind takes at-sockdir (bindat with R beside the socket) and a
    // connect at-direct.
    #[test]
    fn at_and_at_direct_take_bindat_and_connectat() {
        let t = tmp();
        let place = tmp();
        let c = config(&[at(place.path())], &[]);
        let p = nest(t.path(), 600, "s.sock");
        let (l, r) = route::run_long(BIND, &p, &c).unwrap();
        assert_eq!(r, Route::At);
        let listener = UnixListener::from(l);
        mark_untouched(place.path());
        for req in CONNECTS {
            let (cl, r) = route::run_long(req, &p, &c).unwrap();
            assert_eq!(r, Route::AtDirect, "{req:?}");
            ping_kept(&listener, cl);
        }
        assert_untouched(place.path());
        for req in CONNECTS {
            let (cl, r) = route::run_long(req, &p, &no_direct(c.clone())).unwrap();
            assert_eq!(r, Route::At, "{req:?}");
            ping_kept(&listener, cl);
        }
        assert_empty(place.path());

        let long = t.path().join("n".repeat(100));
        let e = route::run_long(BIND, &long, &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
        let (l, _) = route::run_long(BIND, &t.path().join("x.sock"), &c).unwrap();
        std::fs::rename(t.path().join("x.sock"), &long).unwrap();
        mark_untouched(place.path());
        let (cl, r) = route::run_long(CONNECT, &long, &c).unwrap();
        assert_eq!(r, Route::AtDirect);
        ping(l, cl);
        assert_untouched(place.path());

        let nowhere = config(&[], &[]);
        let q = p.with_file_name("q.sock");
        let (l, r) = route::run_long(BIND, &q, &nowhere).unwrap();
        assert_eq!(r, Route::AtSockdir);
        let (cl, r) = route::run_long(CONNECT, &q, &nowhere).unwrap();
        assert_eq!(r, Route::AtDirect);
        ping(l, cl);
        assert_none_named(q.parent().unwrap(), own::HIDDEN_PREFIX);
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(any(target_os = "linux", target_os = "android"))]
mod linux {
    use super::*;

    // On Linux a path whose first byte is a NUL is a name in the abstract
    // namespace, not a file, so none of sockat's routes applies to it: it is
    // handed to std as is, short or long, and whatever std answers is the
    // answer. Rust's std takes an abstract name only through
    // SocketAddr::from_abstract_name and refuses a NUL anywhere in a Path with
    // InvalidInput, so through a path sockat answers exactly that: the same
    // kind and the same message as std's own bind / connect of the same path.
    // The message is what shows that std answered: sockat's own refusal of a
    // NUL, or a route trying the long path (opening "\0/..." as a directory),
    // words it differently. The nonblocking connect, which std cannot make,
    // builds the address std would and is refused there the same way
    // (InvalidInput).
    #[test]
    fn a_leading_nul_goes_to_std_at_any_length() {
        use std::os::unix::ffi::OsStrExt;
        let long = [b"\0/".as_slice(), "d/".repeat(150).as_bytes(), b"s.sock"].concat();
        for bytes in [&b"\0name"[..], &long] {
            let p = Path::new(OsStr::from_bytes(bytes));
            let std_bind = UnixListener::bind(p).unwrap_err();
            let std_connect = UnixStream::connect(p).unwrap_err();
            for (req, want) in [
                (BIND, &std_bind),
                (CONNECT, &std_connect),
                (CONNECT_LAST, &std_connect),
            ] {
                let e = route::run(req, p).unwrap_err();
                assert_eq!(e.kind(), want.kind(), "{} {req:?}", bytes.len());
                assert_eq!(e.to_string(), want.to_string(), "{} {req:?}", bytes.len());
            }
            let e = route::run(CONNECT_NB, p).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
        }
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

    /// `c` with `proc` as the place of `/proc/self/fd`.
    fn with_proc(proc: &Path, c: Config) -> Config {
        Config {
            proc_self_fd: proc.to_path_buf(),
            ..c
        }
    }

    // The routes that need /proc, by file name length, with the thread-cwd
    // means off (as under a seccomp filter) and one place for D:
    //
    // * a short name: `/proc/self/fd/<D's fd>/R/<name>` fits, so the bind
    //   takes proc; a connect takes proc-direct,
    //   `/proc/self/fd/<dir fd>/<name>`, which comes first in its order;
    // * 85 bytes: proc no longer fits (14 + digits + 1 + 11 + 85 > 107), so
    //   the bind forks in D (`R/<name>` fits); proc-direct still fits for a
    //   connect;
    // * 95 bytes: neither /proc spelling fits, so a connect takes the symlink
    //   in D, `/proc/self/fd/<D's fd>/s` (about 20 bytes whatever the name);
    //   the bind still forks in D.
    //
    // Each socket is at the path given and reached; the place is empty after
    // every call.
    #[test]
    fn proc_proc_direct_and_proc_held_by_file_name_length() {
        let t = tmp();
        let place = tmp();
        let c = no_thread_cwd(config(&[at(place.path())], &[]));
        let short: fn(Request, Route) -> bool = |req, r| match req {
            Request::Bind(_) => r == Route::Proc,
            Request::Connect(_) => r == Route::ProcDirect,
        };
        let fd_route: fn(Request, Route) -> bool = |req, r| match req {
            Request::Bind(_) => r.forked(),
            Request::Connect(_) => r == Route::ProcDirect,
        };
        let file_link: fn(Request, Route) -> bool = |req, r| match req {
            Request::Bind(_) => r.forked(),
            Request::Connect(_) => r == Route::ProcHeld,
        };
        for (name, want) in [
            ("s.sock".to_string(), short),
            ("e".repeat(85), fd_route),
            ("n".repeat(95), file_link),
        ] {
            let p = nest(t.path(), 400, &name);
            let (l, r) = route::run_long(BIND, &p, &c).unwrap();
            assert!(want(BIND, r), "{}: bind {r:?}", name.len());
            assert!(is_socket(&p));
            let listener = UnixListener::from(l);
            for req in CONNECTS {
                let (cl, r) = route::run_long(req, &p, &c).unwrap();
                assert!(want(req, r), "{}: {req:?} {r:?}", name.len());
                ping_kept(&listener, cl);
                assert_empty(place.path());
            }
        }
    }

    // The same lengths with the socket's directory's open refused (it is
    // opened O_PATH here, which a permission does not stop, so a stand-in
    // refuses it): only proc-direct, which needs that fd, is lost. Whether
    // /proc can be used is checked without the socket's directory, so a
    // connect of a short name goes on to proc, and the symlink in D
    // (`/proc/self/fd/<D's fd>/s`) still serves every connect of 85 and 95
    // bytes; the bind takes proc or forks in D as before. Only the connects
    // ask for the socket's directory, once each (proc-direct); D is made and
    // removed in the place every time.
    #[test]
    fn proc_and_proc_held_need_no_socket_directory_fd() {
        let t = tmp();
        let place = tmp();
        let mut c = no_thread_cwd(config(&[at(place.path())], &[]));
        c.open_socket_dir = refuse_socket_dir;
        socket_dir_opens();
        let proc: fn(Route) -> bool = |r| r == Route::Proc;
        let fork: fn(Route) -> bool = |r| r.forked();
        for (name, bind, connect, opens) in [
            ("s.sock".to_string(), proc, Route::Proc, 1),
            ("e".repeat(85), fork, Route::ProcHeld, 1),
            ("n".repeat(95), fork, Route::ProcHeld, 1),
        ] {
            let p = nest(t.path(), 400, &name);
            let (l, r) = route::run_long(BIND, &p, &c).unwrap();
            assert!(bind(r), "{}: bind {r:?}", name.len());
            assert!(is_socket(&p));
            assert_eq!(socket_dir_opens(), 0, "{}: bind", name.len());
            let listener = UnixListener::from(l);
            for req in CONNECTS {
                let (cl, r) = route::run_long(req, &p, &c).unwrap();
                assert_eq!(r, connect, "{}: {req:?}", name.len());
                assert_eq!(socket_dir_opens(), opens, "{}: {req:?}", name.len());
                ping_kept(&listener, cl);
                assert_empty(place.path());
            }
        }
    }

    // The symlink in D needs no safe place either (DR-0027): through a place
    // anyone may write to (0777, not sticky), every connect reaches a socket
    // whose name fits no /proc spelling, from this process (no fork; the
    // server sees this process's pid), and the place is empty again
    // afterwards, after a failing connect too: a missing socket is the
    // kernel's ENOENT and a socket nobody listens on ECONNREFUSED (the connect
    // through the link is the result, not ENAMETOOLONG).
    #[test]
    fn proc_held_reaches_a_socket_through_an_open_place() {
        let me = std::process::id() as libc::pid_t;
        let t = tmp();
        let places = tmp();
        let open = open_place(places.path(), "open");
        let c = no_thread_cwd(config(&[at(&open)], &[]));
        let p = t.path().join("n".repeat(95));
        let (l, _) = route::run_long(BIND, &p, &c).unwrap();
        let listener = UnixListener::from(l);
        for req in CONNECTS {
            let (cl, r) = route::run_long(req, &p, &c).unwrap();
            assert_eq!(r, Route::ProcHeld, "{req:?}");
            let mut cl = UnixStream::from(cl);
            let (mut s, _) = listener.accept().unwrap();
            assert_eq!(peer_pid(&s), me, "{req:?}");
            cl.write_all(b"ping").unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).unwrap();
            assert_empty(&open);
        }
        let e = route::run_long(CONNECT, &p.with_file_name("m".repeat(95)), &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");
        assert_empty(&open);
        let stale = p.with_file_name("s".repeat(95));
        stale_socket(&stale);
        let e = route::run_long(CONNECT, &stale, &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ECONNREFUSED), "{e}");
        assert_empty(&open);
    }

    // The thread-cwd means moves into D through an O_PATH fd and resolves R to
    // the socket's directory, which needs only search permission there: a
    // directory without read permission (mode 0300: write + search) is
    // enough, as it is for a plain bind / connect by path. Root bypasses
    // permission checks, so under root (as in `just test-linux`, docker) the
    // test cannot tell anything and says so; where the means is not used (a
    // seccomp filter, as docker's default) there is nothing to check either.
    #[test]
    fn thread_cwd_does_not_need_read_permission_on_the_directory() {
        use std::os::unix::fs::PermissionsExt;
        if sys::effective_uid() == 0 {
            eprintln!("skipped: running as root, which bypasses the permission under test");
            return;
        }
        if !thread_cwd_usable() {
            return;
        }
        let t = tmp();
        let place = tmp();
        let p = nest(t.path(), 400, "s.sock");
        let dir = p.parent().unwrap().to_path_buf();
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
        }
        let _restore = Restore(dir.clone());
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300)).unwrap();
        let e = std::fs::read_dir(&dir).unwrap_err();
        assert_eq!(
            errno(&e),
            Some(libc::EACCES),
            "the premise: no read permission"
        );

        let c = with_proc(&no_proc(t.path(), false), config(&[at(place.path())], &[]));
        let (l, r) = route::run_long(BIND, &p, &c).unwrap();
        assert_eq!(r, Route::ThreadCwd);
        assert!(is_socket(&p));
        let (cl, r) = route::run_long(CONNECT, &p, &c).unwrap();
        assert_eq!(r, Route::ThreadCwd);
        ping(l, cl);
    }

    // Without /proc (no /proc directory, or an empty one with nothing
    // mounted) and without the thread-cwd means, D is opened O_RDONLY and its
    // mode set by fchmod (an O_PATH fd takes no fchmod), and the bind forks in
    // D. A connect takes the routes of a Linux without /proc (thread-cwd
    // being off):
    //
    // * a path whose spelling relative to the cwd is at most DIRECT_MAX bytes
    //   takes the relative route;
    // * a path far from the cwd goes through a symlink under a safe place;
    // * with no usable place for the symlink (one that anyone may write to,
    //   0777 and not sticky, is passed over untouched), `connect` is
    //   ENAMETOOLONG and `connect_ex` with the last resort forks.
    //
    // The places for D and the symlink are empty afterwards. The real /proc is
    // untouched: only the place to look is changed.
    #[test]
    fn without_proc_the_bind_forks_in_d_and_a_connect_takes_relative_then_symlink() {
        for empty in [false, true] {
            let t = tmp();
            let below = tmp_below_cwd();
            let place = tmp();
            let links = tmp();
            let places = tmp();
            let open = open_place(places.path(), "open");
            let none = no_proc(t.path(), empty);
            let base = no_thread_cwd(with_proc(&none, config(&[at(place.path())], &[])));
            let cwd = std::env::current_dir().unwrap();
            let rel_base = below.path().strip_prefix(&cwd).unwrap();
            let near = cwd.join(nest(rel_base, DIRECT_MAX, "rel.sock"));
            let far = nest(t.path(), 600, "far.sock");

            let c = Config {
                file_link_places: vec![links.path().to_path_buf()],
                ..base.clone()
            };
            for (p, want) in [(&near, Route::Relative), (&far, Route::SymlinkFile)] {
                let (l, r) = route::run_long(BIND, p, &c).unwrap();
                assert!(r.forked(), "{empty}: bind {r:?}");
                assert!(is_socket(p));
                let (cl, r) = route::run_long(CONNECT, p, &c).unwrap();
                assert_eq!(r, want, "{empty}");
                ping(l, cl);
            }
            assert_empty(links.path());

            let c = Config {
                file_link_places: vec![open.clone()],
                ..base
            };
            let q = far.with_file_name("q.sock");
            let (l, _) = route::run_long(BIND, &q, &c).unwrap();
            mark_untouched(&open);
            let e = route::run_long(CONNECT, &q, &c).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
            let (cl, r) = route::run_long(CONNECT_LAST, &q, &c).unwrap();
            assert!(r.forked(), "{r:?}");
            ping(l, cl);
            assert_untouched(&open);
            assert_empty(place.path());
        }
    }

    // The side effect documented for connecting through the fork route: the
    // server's SO_PEERCRED is recorded at connect time and keeps the pid of
    // the child that connected, even after the child has exited and the real
    // client process has sent data. This is why `sockat::connect` never forks;
    // pinned here so the reason stays checkable. A connect of the public order
    // (no fork) names this process.
    #[test]
    fn so_peercred_keeps_the_pid_of_a_forked_connector() {
        use std::os::fd::AsFd;
        let t = tmp();
        let p = nest(t.path(), 300, "s.sock");
        let (dir, name) = path::split(&p).unwrap();
        let addr = SockAddr::standard(name).unwrap();
        let l = UnixListener::from(route::fork_op(BIND, dir, &addr).unwrap().0);
        let mut c = UnixStream::from(route::fork_op(CONNECT_LAST, dir, &addr).unwrap().0);
        let (mut s, _) = l.accept().unwrap();
        let me = std::process::id() as libc::pid_t;
        let before = sys::peer_cred_pid(s.as_fd()).unwrap();
        assert_ne!(before, me);
        c.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        s.read_exact(&mut b).unwrap();
        assert_eq!(sys::peer_cred_pid(s.as_fd()).unwrap(), before);

        let (c2, r) = route::run(CONNECT_LAST, &p).unwrap();
        assert!(!r.forked(), "{r:?}");
        let (s2, _) = l.accept().unwrap();
        assert_eq!(sys::peer_cred_pid(s2.as_fd()).unwrap(), me);
        drop(c2);
    }

    /// Places and directories one [`ways`] call works in, removed at the end
    /// of the test.
    struct Dirs {
        t: tempfile::TempDir,
        below_cwd: tempfile::TempDir,
        place: tempfile::TempDir,
        links: tempfile::TempDir,
    }

    impl Dirs {
        fn new() -> Self {
            Self {
                t: tmp(),
                below_cwd: tmp_below_cwd(),
                place: tmp(),
                links: tmp(),
            }
        }
    }

    /// The Linux routes, each a [`Way`] to its own socket path, with the
    /// route each request must take there:
    ///
    /// | way | path, inputs | bind | connects |
    /// |---|---|---|---|
    /// | Direct | DIRECT_MAX bytes | Direct | Direct |
    /// | ThreadCwd | 600 bytes (if usable here), [`no_direct`] | ThreadCwd | ThreadCwd |
    /// | ProcThroughLink | 600, thread cwd off, [`no_direct`] | Proc | Proc |
    /// | ProcFd | name of 85, thread cwd off | fork in D | ProcDirect |
    /// | ProcFileLink | name of 95, thread cwd off | fork in D | ProcHeld |
    /// | Relative | below the cwd, no /proc, thread cwd off | fork in D | Relative |
    /// | Symlink | 600, no /proc, thread cwd off, a link place | fork in D | Symlink |
    /// | Fork | 600, no /proc, thread cwd off, no link place | fork in D | fork (the last resort) |
    fn ways(d: &Dirs) -> Vec<(&'static str, Way, Want)> {
        let t = d.t.path();
        let cwd = std::env::current_dir().unwrap();
        let rel_base = d.below_cwd.path().strip_prefix(&cwd).unwrap();
        let none = no_proc(t, false);
        let base = config(&[at(d.place.path())], &[]);
        let off = no_thread_cwd(base.clone());
        let bare = no_thread_cwd(with_proc(&none, base.clone()));
        let direct = nest(t, DIRECT_MAX, "direct.sock");
        let bind_forks = |connected: Route| {
            move |req: Request, r: Route| match req {
                Request::Bind(_) => r.forked(),
                Request::Connect(_) => r == connected,
            }
        };
        let mut cases: Vec<(&'static str, PathBuf, Config, Want)> = vec![
            (
                "ProcThroughLink",
                nest(t, 600, "proc.sock"),
                no_direct(off.clone()),
                std::sync::Arc::new(|_, r| r == Route::Proc),
            ),
            (
                "ProcFd",
                nest(t, 400, &"e".repeat(85)),
                off.clone(),
                std::sync::Arc::new(bind_forks(Route::ProcDirect)),
            ),
            (
                "ProcFileLink",
                nest(t, 400, &"n".repeat(95)),
                off,
                std::sync::Arc::new(bind_forks(Route::ProcHeld)),
            ),
            (
                "Relative",
                cwd.join(nest(rel_base, DIRECT_MAX, "rel.sock")),
                bare.clone(),
                std::sync::Arc::new(bind_forks(Route::Relative)),
            ),
            (
                "Symlink",
                nest(t, 600, "sym.sock"),
                Config {
                    file_link_places: vec![d.links.path().to_path_buf()],
                    ..bare.clone()
                },
                std::sync::Arc::new(bind_forks(Route::SymlinkFile)),
            ),
            (
                "Fork",
                nest(t, 600, "fork.sock"),
                bare,
                std::sync::Arc::new(|_, r: Route| r.forked()),
            ),
        ];
        if thread_cwd_usable() {
            cases.push((
                "ThreadCwd",
                nest(t, 600, "thread.sock"),
                no_direct(base),
                std::sync::Arc::new(|_, r| r == Route::ThreadCwd),
            ));
        }
        let mut ways: Vec<(&'static str, Way, Want)> = vec![(
            "Direct",
            std::sync::Arc::new(move |req| route::run(req, &direct)),
            std::sync::Arc::new(|_, r| r == Route::Standard),
        )];
        ways.extend(cases.into_iter().map(|(what, p, c, want)| {
            let way: Way = std::sync::Arc::new(move |req| route::run_long(req, &p, &c));
            (what, way, want)
        }));
        ways
    }

    // A nonblocking connect must not wait on a full backlog on any route
    // (the blocking one does, on Linux: it sleeps until the listener
    // accepts). Each route makes its socket O_NONBLOCK before connecting: the
    // path itself, /proc, the relative spelling and the symlinks on sockat's
    // own socket rather than std's (std cannot connect a nonblocking socket),
    // the thread-cwd means on the calling thread before handing the socket to
    // the other thread, the fork route in the parent before the fork, so the
    // child's connect is nonblocking through the open file description they
    // share. See check_nonblocking_connect for what is checked on each.
    #[test]
    fn a_nonblocking_connect_fails_at_once_on_a_full_backlog_on_every_route() {
        let d = Dirs::new();
        for (what, way, want) in ways(&d) {
            check_nonblocking_connect(&way, &want, what);
        }
        assert_empty(d.place.path());
        assert_empty(d.links.path());
    }

    // DR-0010, route by route: only the fork route connects from another
    // process (the child, whose pid SO_PEERCRED keeps), so only there do the
    // connects that may fork report a fork and the ones that may not fail with
    // ENAMETOOLONG. Every other route connects from this process for all four,
    // the thread-cwd means included (its connect is made by another thread of
    // this process, and SO_PEERCRED names the process). See
    // check_fork_and_peer.
    #[test]
    fn only_fork_cwd_direct_reports_a_fork_or_needs_the_last_resort() {
        let d = Dirs::new();
        for (what, way, want) in ways(&d) {
            check_fork_and_peer(&way, &want, what == "Fork", what);
        }
        assert_empty(d.place.path());
        assert_empty(d.links.path());
    }
}

// ---------------------------------------------------------------------------
// the connect options, route by route (Linux and Apple, where the tests run)
// ---------------------------------------------------------------------------

/// What a nonblocking connect answers while the listener's backlog is full
/// (Linux: `EAGAIN`, where a blocking connect waits for room; macOS:
/// `ECONNREFUSED`, as a blocking connect does).
#[cfg(any(target_os = "linux", target_os = "android"))]
const FULL_BACKLOG: i32 = libc::EAGAIN;
#[cfg(target_vendor = "apple")]
const FULL_BACKLOG: i32 = libc::ECONNREFUSED;

/// One socket path reached one fixed way: the route order with its inputs
/// chosen so that one route is taken. Called with what to do there.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
type Way = std::sync::Arc<dyn Fn(Request) -> io::Result<(OwnedFd, Route)> + Send + Sync>;

/// Whether a route is the one a [`Way`] must take for a request (a bind and a
/// connect can differ: some routes are for connects only).
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
type Want = std::sync::Arc<dyn Fn(Request, Route) -> bool + Send + Sync>;

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

/// Check the nonblocking connect of `way`, whose routes must satisfy `want`.
/// `what` names the way in failure messages.
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
fn check_nonblocking_connect(way: &Way, want: &Want, what: &str) {
    use std::os::fd::AsFd;
    let (l, r) = way(BIND).unwrap();
    assert!(want(BIND, r), "{what}: bind {r:?}");
    let listener = UnixListener::from(l);

    let (c, r) = way(CONNECT_LAST).unwrap();
    assert!(want(CONNECT_LAST, r), "{what}: blocking {r:?}");
    assert!(
        !nonblocking(&c),
        "{what}: a blocking connect stays blocking"
    );
    drop(listener.accept().unwrap());

    let w = way.clone();
    let (c, r) = at_once(move || w(CONNECT_NB_LAST)).unwrap();
    assert!(want(CONNECT_NB_LAST, r), "{what}: nonblocking {r:?}");
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
        match at_once(move || w(CONNECT_NB_LAST)) {
            Ok((c, r)) => {
                assert!(want(CONNECT_NB_LAST, r), "{what}: {r:?}");
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
    let e = at_once(move || w(CONNECT_NB_LAST)).unwrap_err();
    assert_eq!(errno(&e), Some(FULL_BACKLOG), "{what}: {e}");
}

/// Check the four connects through `way` (each takes the same route there),
/// with `forks` telling whether the connects' route there is the fork route:
///
/// * The fork route: the connects that may take the last resort
///   (`connect_ex`, `connect_nonblocking_ex`) connect and report that they forked
///   ([`Route::forked`]), and the server's peer-credential query right after
///   the connect names the forked child, not this process. The ones that may
///   not (`connect`, `connect_nonblocking`) fail with `ENAMETOOLONG` and no
///   connection reaches the listener.
/// * Any other route: all four connect on it, report that they did not fork,
///   and the server names this process. The nonblocking ones return an
///   `O_NONBLOCK` socket, the others not.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn check_fork_and_peer(way: &Way, want: &Want, forks: bool, what: &str) {
    let me = std::process::id() as libc::pid_t;
    let (l, _) = way(BIND).unwrap();
    let listener = UnixListener::from(l);
    // Nonblocking, so "nothing got in" is a WouldBlock rather than a hang. A
    // Unix-domain connection is queued before connect returns, so a connect
    // that succeeded is always there to accept.
    listener.set_nonblocking(true).unwrap();
    for req in [CONNECT_LAST, CONNECT_NB_LAST, CONNECT, CONNECT_NB] {
        let Request::Connect(o) = req else {
            unreachable!()
        };
        if forks && !o.last_resort {
            let e = way(req).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{what} {req:?}: {e}");
            let e = listener.accept().unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{what} {req:?}");
            continue;
        }
        let (c, r) = way(req).unwrap();
        assert!(want(req, r), "{what} {req:?}: {r:?}");
        assert_eq!(nonblocking(&c), o.nonblocking, "{what} {req:?}");
        let (s, _) = listener.accept().unwrap();
        let seen = peer_pid(&s);
        assert_eq!(r.forked(), forks, "{what} {req:?}: {r:?}");
        if forks {
            assert!(
                matches!(r, Route::ForkCwdDirect { child } if child == seen),
                "{what} {req:?}: {r:?}, server sees {seen}"
            );
            assert_ne!(seen, me, "{what} {req:?}");
        } else {
            assert_eq!(seen, me, "{what} {req:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// The POSIX routes of a connect: the relative spelling and the symlink in a
// safe place (every target but FreeBSD), and the thread-cwd means forced
// (Linux, Apple)
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "freebsd"))]
mod posix {
    use super::*;
    use crate::route::links::{self, PlaceKind, TempLink};
    use crate::route::posix::Relative;
    use std::ffi::OsString;
    use std::path::Component;

    /// The route order's inputs with no place for D (so a connect passes over
    /// the routes through D and a bind goes beside the socket), `links` as
    /// the places for the symlink, and on Linux /proc made to look absent, so
    /// the order of a Linux without /proc (relative, thread-cwd, symlink)
    /// runs there. On Apple the widened sockaddr is on.
    fn cfg(links: &[&Path]) -> Config {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        return Config {
            proc_self_fd: PathBuf::from("/nonexistent/proc/self/fd"),
            ..config(&[], links)
        };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        return config(&[], links);
    }

    fn has_dotdot(p: &Path) -> bool {
        p.components().any(|c| c == Component::ParentDir)
    }

    // A path over DIRECT_MAX bytes whose spelling relative to the cwd is at
    // most DIRECT_MAX bytes: a connect takes the relative route first
    // (through std, the plain sockaddr), before anything else is considered
    // (on Apple, the widened form). The bind
    // (beside the socket here, no place for D) put the socket at the given
    // absolute path.
    #[test]
    fn a_path_short_relative_to_the_cwd_takes_relative() {
        let t = tmp_below_cwd();
        let cwd = std::env::current_dir().unwrap();
        // Relative spelling of DIRECT_MAX bytes, so the absolute path (cwd +
        // "/" + that) is over DIRECT_MAX however short the cwd is.
        let rel_base = t.path().strip_prefix(&cwd).unwrap();
        let p = cwd.join(nest(rel_base, DIRECT_MAX, "s.sock"));
        assert!(path::len(&p) > DIRECT_MAX);
        let c = cfg(&[Path::new("/nonexistent")]);
        let (l, _) = route::run_long(BIND, &p, &c).unwrap();
        assert!(is_socket(&p));
        let (cl, r) = route::run_long(CONNECT, &p, &c).unwrap();
        assert_eq!(r, Route::Relative);
        ping(l, cl);
    }

    // A path far from the cwd (600 bytes under /tmp, so no relative spelling
    // fits, nor on Apple the widened one) is connected to through a symlink
    // `<place>/sockat.XXXXXXXXXX` to the socket file, by all four connects
    // (none forks). The place is empty afterwards: the link is removed once
    // the call returns.
    #[test]
    fn a_path_far_from_the_cwd_is_connected_to_through_a_temporary_symlink() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let links = tmp();
        let c = cfg(&[links.path()]);
        let (l, _) = route::run_long(BIND, &p, &c).unwrap();
        let listener = UnixListener::from(l);
        for req in CONNECTS {
            let (cl, r) = route::run_long(req, &p, &c).unwrap();
            assert_eq!(r, Route::SymlinkFile, "{req:?}");
            ping_kept(&listener, cl);
        }
        assert_empty(links.path());
    }

    // The link holds the path as the caller wrote it, so the kernel resolves
    // it as it resolves the path: a `..` right after a symlink climbs out of
    // the symlink's target, not out of the directory the symlink sits in.
    // With `<t>/link -> <t>/real/sub`, `<t>/link/../x/...` names
    // `<t>/real/x/...`; a socket at `<t>/x/...` (where a lexical `..` would
    // lead) is a decoy that must not be reached. The bind's R points to the
    // real path of the socket's directory, resolved the same way: the socket
    // lands in `<t>/real/x/...` too.
    #[test]
    fn the_paths_resolve_as_the_kernel_does() {
        let t = tmp();
        let links = tmp();
        let place = tmp();
        let real = t.path().join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::os::unix::fs::symlink(real.join("sub"), t.path().join("link")).unwrap();
        let at_real = nest_tail(&real.join("x"), 500, "s.sock");
        let below = at_real.strip_prefix(&real).unwrap();
        let spelled = t.path().join("link/..").join(below);
        let decoy = t.path().join(below);
        std::fs::create_dir_all(decoy.parent().unwrap()).unwrap();

        let (l, _) = route::run_long(BIND, &spelled, &config(&[at(place.path())], &[])).unwrap();
        assert!(is_socket(&at_real));
        assert!(!is_socket(&decoy));
        let (decoy_l, _) = route::run(BIND, &decoy).unwrap();
        let decoy_l = UnixListener::from(decoy_l);
        decoy_l.set_nonblocking(true).unwrap();

        let (cl, r) = route::run_long(CONNECT, &spelled, &cfg(&[links.path()])).unwrap();
        assert_eq!(r, Route::SymlinkFile);
        ping(l, cl);
        let e = decoy_l.accept().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        assert_empty(links.path());
        assert_empty(place.path());
    }

    // The symlink route checks the length through the link before creating
    // anything, in the plain sockaddr only (on Apple too: the widened form is
    // for the path itself and its relative spelling). A place so long that
    // `<place>/sockat.XXXXXXXXXX` does not fit (here 150 bytes) is skipped
    // without touching it, as is a place that is missing: with nothing else
    // left, `connect` is ENAMETOOLONG and `connect_ex` with the last resort forks. A short place is
    // used.
    #[test]
    fn a_place_too_long_for_the_symlink_is_passed_over() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let b = tmp();
        let long = nest(b.path(), 150, "base");
        std::fs::create_dir(&long).unwrap();
        let short = tmp();
        let (l, _) = route::run_long(BIND, &p, &cfg(&[])).unwrap();
        let listener = UnixListener::from(l);
        mark_untouched(&long);
        for links in [&[long.as_path()][..], &[Path::new("/nonexistent")], &[]] {
            let c = cfg(links);
            let e = route::run_long(CONNECT, &p, &c).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{links:?}: {e}");
            let (cl, r) = route::run_long(CONNECT_LAST, &p, &c).unwrap();
            assert!(r.forked(), "{links:?}: {r:?}");
            ping_kept(&listener, cl);
        }
        assert_untouched(&long);
        let (cl, r) = route::run_long(CONNECT, &p, &cfg(&[&long, short.path()])).unwrap();
        assert_eq!(r, Route::SymlinkFile);
        ping_kept(&listener, cl);
        assert_untouched(&long);
        assert_empty(short.path());

        // The boundary: `<place>/sockat.XXXXXXXXXX` is the place and 18 bytes,
        // so a place of DIRECT_MAX - 18 bytes is used and one of a byte more
        // is passed over untouched.
        let e = tmp();
        let exact = nest(e.path(), DIRECT_MAX - 18, "x");
        std::fs::create_dir(&exact).unwrap();
        let over = nest(e.path(), DIRECT_MAX - 17, "yy");
        std::fs::create_dir(&over).unwrap();
        mark_untouched(&over);
        let (cl, r) = route::run_long(CONNECT, &p, &cfg(&[&over, &exact])).unwrap();
        assert_eq!(r, Route::SymlinkFile);
        ping_kept(&listener, cl);
        assert_untouched(&over);
        assert_empty(&exact);
    }

    // A failure through the symlink is the kernel's error for the target
    // (ENOENT for a missing socket or directory, ECONNREFUSED for a socket
    // nobody listens on), it is not retried under the next place (which stays
    // untouched) nor by the fork route, and the link is removed in the failure
    // case too.
    #[test]
    fn a_failure_through_the_symlink_is_reported_and_not_retried() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let first = tmp();
        let second = tmp();
        let c = cfg(&[first.path(), second.path()]);
        let (_l, _) = route::run_long(BIND, &p, &c).unwrap();
        mark_untouched(second.path());
        let missing = p.parent().unwrap().join("nope/s.sock");
        let e = route::run_long(CONNECT_LAST, &p.with_file_name("x.sock"), &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");
        let e = route::run_long(CONNECT_LAST, &missing, &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOENT), "{e}");
        let stale = p.with_file_name("stale.sock");
        stale_socket(&stale);
        let e = route::run_long(CONNECT_LAST, &stale, &c).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ECONNREFUSED), "{e}");
        assert_untouched(second.path());
        assert_empty(first.path());
    }

    // The symlink name comes from a fresh random draw each time and symlink(2)
    // never replaces an entry, so two links made back to back are distinct, an
    // existing file under the place is never touched, and dropping the guard
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
            assert!(is_drawn(l.file_name().unwrap(), own::PREFIX), "{l:?}");
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
    // symlink(2) fails with EEXIST rather than replacing it. When every name
    // is taken the attempt ends after own::ATTEMPTS tries with AlreadyExists
    // (the place then counts as unusable). The names come from a stand-in
    // generator here so the collision is certain.
    #[test]
    fn a_taken_symlink_name_is_skipped_and_the_tries_are_bounded() {
        let base = tmp();
        let target = tmp();
        let taken = format!("{}AAAAAAAAAA", own::PREFIX);
        let free = format!("{}BBBBBBBBBB", own::PREFIX);
        std::fs::write(base.path().join(&taken), b"keep").unwrap();
        let mut names = vec![taken.clone(), free.clone()].into_iter();
        let mut calls = 0;
        let link = TempLink::create_named(base.path(), target.path(), || {
            calls += 1;
            Ok(names.next().unwrap())
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(link.path, base.path().join(&free));
        assert_eq!(std::fs::read_link(&link.path).unwrap(), target.path());
        assert_eq!(std::fs::read(base.path().join(&taken)).unwrap(), b"keep");
        drop(link);
        assert!(!base.path().join(&free).exists());

        let mut calls = 0;
        let e = TempLink::create_named(base.path(), target.path(), || {
            calls += 1;
            Ok(taken.clone())
        })
        .err()
        .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(calls, own::ATTEMPTS);
        assert_eq!(std::fs::read(base.path().join(&taken)).unwrap(), b"keep");
    }

    // A place the link cannot be created in (missing, or a file) makes the
    // symlink route unusable there rather than an error: passed over as no
    // safe place, nothing made.
    #[test]
    fn an_unusable_place_skips_the_symlink_route() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let file = t.path().join("file");
        std::fs::write(&file, b"").unwrap();
        for base in [Path::new("/nonexistent"), file.as_path()] {
            let out = links::symlink_file_at(CONNECT, base, &p);
            assert_eq!(out.err(), Some(route::names::NO_SAFE_BASE), "{base:?}");
        }
        assert_eq!(std::fs::read(&file).unwrap(), b"");
    }

    // kawaz's ruling: a place for the symlink is used only where no other
    // user can replace the symlink between its creation and the call through
    // it (SPEC "symlink の置き場と名前"). The rule on a directory's mode and
    // owner, row by row (owner and user as uids; 0 is root):
    //
    // * the user's own, without group / other write (0700, 0755, also when
    //   sticky): only the user can write there (OwnerOnly). With either
    //   (0775, 0757, 0777) and not sticky: not a place;
    // * sticky and owned by root (as /tmp is) or by the user, writable by
    //   others: anyone can create an entry, only its owner remove it
    //   (Shared). Sticky and owned by another user: not a place, the
    //   directory's owner can remove or rename any entry in it;
    // * another user's without group / other write: not a place (it is
    //   theirs to change); root's, not sticky: not a place (only for root
    //   itself, where it is OwnerOnly).
    //
    // symlink-dir tries the OwnerOnly places first and tells a bind through a
    // Shared one apart, so the kind matters, not only whether it is a place.
    #[test]
    fn a_place_for_the_symlink_must_be_sticky_or_the_users_own() {
        use PlaceKind::{OwnerOnly, Shared};
        let me = 1000;
        let cases: &[(u32, u32, Option<PlaceKind>)] = &[
            (0o1777, 0, Some(Shared)),
            (0o1777, me, Some(Shared)),
            (0o1700, me, Some(OwnerOnly)),
            (0o1777, 2000, None),
            (0o700, me, Some(OwnerOnly)),
            (0o755, me, Some(OwnerOnly)),
            (0o775, me, None),
            (0o757, me, None),
            (0o777, me, None),
            (0o700, 2000, None),
            (0o700, 0, None),
        ];
        for &(mode, owner, want) in cases {
            assert_eq!(
                links::place_kind(mode, owner, me),
                want,
                "{mode:o} owned by {owner}"
            );
        }
        assert_eq!(
            links::place_kind(0o700, 0, 0),
            Some(OwnerOnly),
            "root's own, for root"
        );
    }

    // A safe place must be a directory, judged through symlinks as the
    // kernel would resolve a path through it. A file of ours with mode 0600
    // or 0700 has the owner and mode a private TMPDIR has, yet nothing can be
    // created in it, so it is not safe; a symlink to such a file neither. A
    // symlink to a safe directory is (macOS's /tmp is one to /private/tmp).
    #[test]
    fn a_place_for_the_symlink_must_be_a_directory() {
        use std::os::unix::fs::PermissionsExt;
        let t = tmp();
        for mode in [0o600, 0o700] {
            let file = t.path().join(format!("file{mode:o}"));
            std::fs::write(&file, b"").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(links::place_kind_of(&file), None, "{mode:o}");
            let link = t.path().join(format!("link{mode:o}"));
            std::os::unix::fs::symlink(&file, &link).unwrap();
            assert_eq!(links::place_kind_of(&link), None, "{mode:o}");
        }
        let dir = place(t.path(), "dir", 0o700);
        assert_eq!(links::place_kind_of(&dir), Some(PlaceKind::OwnerOnly));
        let link = t.path().join("linkdir");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert_eq!(links::place_kind_of(&link), Some(PlaceKind::OwnerOnly));
    }

    /// A directory `name` under `parent` with exactly `mode` (set after
    /// creating it, so the umask does not interfere).
    fn place(parent: &Path, name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let d = parent.join(name);
        std::fs::create_dir(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).unwrap();
        d
    }

    // The same rule on real directories, through the route order. A place
    // anyone may write to and that is not sticky (0777, 0775, 0757) is passed
    // over without creating anything in it (its mtime is unchanged) and the
    // next place is used; a sticky place (1777) and the user's own (0700,
    // 0755) are used. Run as root (docker), a place owned by another user is
    // constructed too (chown) and must be passed over, sticky or not; other
    // users cannot chown, so that part is skipped then.
    #[test]
    fn an_unsafe_place_for_the_symlink_is_passed_over() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let places = tmp();
        let good = tmp();
        let (l, _) = route::run_long(BIND, &p, &cfg(&[])).unwrap();
        let listener = UnixListener::from(l);
        for mode in [0o777, 0o775, 0o757] {
            let bad = place(places.path(), &format!("bad{mode:o}"), mode);
            mark_untouched(&bad);
            let (cl, r) = route::run_long(CONNECT, &p, &cfg(&[&bad, good.path()])).unwrap();
            assert_eq!(r, Route::SymlinkFile, "{mode:o}");
            ping_kept(&listener, cl);
            assert_untouched(&bad);
            assert_empty(good.path());
        }
        for mode in [0o1777, 0o700, 0o755] {
            let ok = place(places.path(), &format!("ok{mode:o}"), mode);
            let (cl, r) = route::run_long(CONNECT, &p, &cfg(&[&ok])).unwrap();
            assert_eq!(r, Route::SymlinkFile, "{mode:o}");
            ping_kept(&listener, cl);
            assert_empty(&ok);
        }
        if sys::effective_uid() != 0 {
            eprintln!("skipped the other user's places: only root can chown");
            return;
        }
        for mode in [0o1777, 0o700] {
            let theirs = place(places.path(), &format!("theirs{mode:o}"), mode);
            std::os::unix::fs::chown(&theirs, Some(65534), None).unwrap();
            mark_untouched(&theirs);
            let (cl, r) = route::run_long(CONNECT, &p, &cfg(&[&theirs, good.path()])).unwrap();
            assert_eq!(r, Route::SymlinkFile, "{mode:o}");
            ping_kept(&listener, cl);
            assert_untouched(&theirs);
        }
    }

    // The same through the real environment (Config::from_env, as the public
    // API reads it, with D made nowhere and on Linux /proc made to look
    // absent, so a connect gets to the symlink route): with TMPDIR set to a
    // directory too long for the symlink, a 600-byte path is still connected
    // to through a symlink, under /tmp, and nothing is created in TMPDIR. Runs
    // in a child process because it sets TMPDIR (the cwd is / so the relative
    // spelling is long too).
    #[test]
    fn a_tmpdir_too_long_for_the_symlink_falls_back_to_tmp() {
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let (l, _) = route::run(BIND, &p).unwrap();
        let listener = UnixListener::from(l);
        listener.set_nonblocking(true).unwrap();
        let b = tmp();
        let long_tmpdir = nest(b.path(), 250, "tmpdir");
        std::fs::create_dir(&long_tmpdir).unwrap();
        mark_untouched(&long_tmpdir);
        let out = run_child(
            "tests::posix::child_connect_from_env",
            Path::new("/"),
            &[
                ("TMPDIR", long_tmpdir.as_os_str()),
                ("SOCKAT_CHILD_PATH", p.as_os_str()),
            ],
        );
        assert!(out.contains("sockat-child: Symlink"), "{out}");
        // The child's connection is in the backlog (its end closed since).
        listener.accept().unwrap();
        assert_untouched(&long_tmpdir);
    }

    #[test]
    fn child_connect_from_env() {
        if !in_child() {
            return;
        }
        let p = env_path("SOCKAT_CHILD_PATH");
        let mut c = Config::from_env();
        c.own_dir_places.clear();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            c.proc_self_fd = PathBuf::from("/nonexistent/proc/self/fd");
        }
        let (_c, route) = route::run_long(CONNECT, &p, &c).unwrap();
        println!("sockat-child: {route:?}");
    }

    // ----- the thread-cwd means forced (Linux, Apple) -----
    //
    // Whether the means can be used depends on the system as well as on the
    // target (Linux does not use it under a seccomp filter, as docker's
    // default profile installs one), so these ask thread_cwd_usable which way
    // the order must go.

    // The thread-cwd means forced (the means' own function, with the
    // directory opened as the order opens it): the socket is bound at the
    // path given, in the directory the thread moved into, and accepts a
    // connection made the same way; data flows both ways. The connection is
    // made by this process: the server's peer-credential query (SO_PEERCRED
    // on Linux, LOCAL_PEERPID on macOS) names this process right after the
    // connect, before anything is sent (the fork route names its exited child
    // there). The calling thread's cwd is the one it had.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn thread_cwd_binds_and_connects_from_this_process() {
        if !thread_cwd_usable() {
            return;
        }
        let me = std::process::id() as libc::pid_t;
        let cwd = std::env::current_dir().unwrap();
        let t = tmp();
        let p = nest(t.path(), 600, "s.sock");
        let (dir, name) = path::split(&p).unwrap();
        let addr = SockAddr::standard(name).unwrap();
        let done = |o: sys::OnThread<OwnedFd>| match o {
            sys::OnThread::Done(done) => done.unwrap(),
            sys::OnThread::Unusable(e) => panic!("unusable: {e}"),
        };
        let listener = UnixListener::from(done(route::thread_cwd_op(BIND, dir, &addr)));
        assert!(is_socket(&p));
        let mut c = UnixStream::from(done(route::thread_cwd_op(CONNECT, dir, &addr)));
        let (mut s, _) = listener.accept().unwrap();
        assert_eq!(peer_pid(&s), me);
        c.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        s.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        s.write_all(b"pong").unwrap();
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"pong");
        assert_eq!(std::env::current_dir().unwrap(), cwd);
    }

    // No thread of the process but the one started for the call sees its
    // working directory move: on Linux that thread detaches its own copy
    // (unshare(CLONE_FS)) before the fchdir, on Apple pthread_fchdir_np sets
    // that thread's alone, and the thread ends with the call. Watcher threads
    // keep reading getcwd and resolving a relative path (Cargo.toml, at the
    // package root the tests run in) while workers bind and connect through
    // the means in directories of their own, each worker also checking its
    // own cwd after every call. Had the process's cwd moved into one of those
    // directories even once, a watcher would see it.
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn thread_cwd_never_moves_another_threads_cwd() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        if !thread_cwd_usable() {
            return;
        }
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
            .map(|_| {
                let cwd = cwd.clone();
                std::thread::spawn(move || {
                    let t = tmp();
                    for round in 0..20 {
                        let p = nest(t.path(), 300, &format!("{round}.sock"));
                        let (dir, name) = path::split(&p).unwrap();
                        let addr = SockAddr::standard(name).unwrap();
                        let sys::OnThread::Done(l) = route::thread_cwd_op(BIND, dir, &addr) else {
                            panic!("unusable");
                        };
                        assert_eq!(std::env::current_dir().unwrap(), cwd);
                        let sys::OnThread::Done(c) = route::thread_cwd_op(CONNECT, dir, &addr)
                        else {
                            panic!("unusable");
                        };
                        assert_eq!(std::env::current_dir().unwrap(), cwd);
                        assert!(is_socket(&p));
                        ping(l.unwrap(), c.unwrap());
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

    // Where the thread-cwd means sits: first of a bind's means, and in a
    // connect's order right after the routes that need neither writing nor a
    // thread (relative, and widened on Apple), for every request (no side
    // effect a server or another thread can see); when it cannot be used the
    // order goes on to the next route. A 600-byte path far from the cwd (so
    // relative and widened cannot reach it), one place for D, on Linux /proc
    // made to look absent:
    //
    // * the means on: the bind and the four connects take it (where this
    //   system allows it; under a seccomp filter the order must go on just
    //   the same);
    // * the means off: the bind forks in D, and the connects go on to the
    //   symlink under a short place (`connect` would be ENAMETOOLONG without
    //   it).
    //
    // A bind / connect it made is the result: a missing socket is ENOENT and
    // a socket nobody listens on ECONNREFUSED for the connects that may not
    // fork too (not the ENAMETOOLONG that going on would end in for them).
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    #[test]
    fn thread_cwd_comes_before_fork_and_symlink_and_its_failures_are_final() {
        let usable = thread_cwd_usable();
        let t = tmp();
        let place = tmp();
        let links = tmp();
        let on = Config {
            own_dir_places: vec![at(place.path())],
            ..cfg(&[links.path()])
        };
        let off = no_thread_cwd(on.clone());
        let p = nest(t.path(), 600, "s.sock");
        for (i, (c, thread)) in [(&on, usable), (&off, false)].into_iter().enumerate() {
            let q = p.with_file_name(format!("{i}.sock"));
            let (l, r) = route::run_long(BIND, &q, c).unwrap();
            if thread {
                assert_eq!(r, Route::ThreadCwd);
            } else {
                assert!(r.forked(), "{r:?}");
            }
            let listener = UnixListener::from(l);
            for req in CONNECTS {
                let (cl, r) = route::run_long(req, &q, c).unwrap();
                assert_eq!(
                    r,
                    if thread {
                        Route::ThreadCwd
                    } else {
                        Route::SymlinkFile
                    }
                );
                ping_kept(&listener, cl);
            }
        }
        if usable {
            mark_untouched(links.path());
            for req in [CONNECT, CONNECT_NB] {
                let e = route::run_long(req, &p.with_file_name("x.sock"), &on).unwrap_err();
                assert_eq!(errno(&e), Some(libc::ENOENT), "{req:?}: {e}");
            }
            let stale = p.with_file_name("stale.sock");
            stale_socket(&stale);
            for req in [CONNECT, CONNECT_NB] {
                let e = route::run_long(req, &stale, &on).unwrap_err();
                assert_eq!(errno(&e), Some(libc::ECONNREFUSED), "{req:?}: {e}");
            }
            assert_untouched(links.path());
        }
        assert_empty(place.path());
        assert_empty(links.path());
    }

    // ----- the relative spelling -----
    //
    // These run in a child process so the cwd (and $PWD) can be chosen. This
    // process binds the socket (through D), the child connects through the
    // order with no place for D (without /proc on Linux), prints the
    // route and the spelling, and this process checks which listener got the
    // connection.

    /// Connect to `path` in a child whose cwd is `cwd` and whose `$PWD` is
    /// `pwd`; return (route, relative spelling or "-") as the child printed
    /// them.
    fn connect_in_child(cwd: &Path, pwd: &Path, path: &Path) -> (String, String) {
        let out = run_child(
            "tests::posix::child_connect",
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
    fn child_connect() {
        if !in_child() {
            return;
        }
        let p = env_path("SOCKAT_CHILD_PATH");
        let (dir, _) = path::split(&p).unwrap();
        let rel = Relative::new(&p, dir)
            .map(|r| r.path.display().to_string())
            .unwrap_or_else(|| "-".into());
        let (_c, route) = route::run_long(CONNECT, &p, &cfg(&[])).unwrap();
        println!("sockat-child: {route:?} {rel}");
    }

    /// A listener at `p` (bound through D) that does not
    /// block on accept, so "nothing came" is a WouldBlock.
    fn listen_at(p: &Path) -> UnixListener {
        let place = tmp();
        let (l, _) = route::run_long(BIND, p, &config(&[at(place.path())], &[])).unwrap();
        let l = UnixListener::from(l);
        l.set_nonblocking(true).unwrap();
        l
    }

    /// Whether a connection came to `l`.
    fn came(l: &UnixListener) -> bool {
        match l.accept() {
            Ok(_) => true,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            Err(e) => panic!("{e}"),
        }
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
    // matched: it is the real path (/private/tmp/.../real). The connection
    // reaches the listener in the real directory.
    #[test]
    fn a_path_below_pwd_through_a_symlink_is_cut_at_pwd() {
        let t = tmp();
        let (_real, link) = real_and_link(t.path());
        // /tmp itself is a symlink to /private/tmp on macOS, so getcwd differs
        // from "<t>/link" in two places.
        let p = nest_tail(&link, 95, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let l = listen_at(&p);
        let rel_want = p.strip_prefix(&link).unwrap().to_path_buf();
        let (route, rel) = connect_in_child(&link, &link, &p);
        assert_eq!(route, "Relative");
        assert_eq!(Path::new(&rel), rel_want);
        assert!(came(&l));
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
        let (_real, link) = real_and_link(t.path());
        let p = nest_tail(&link, 95, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let l = listen_at(&p);
        let rel_want = p.strip_prefix(&link).unwrap().to_path_buf();
        let (route, rel) = connect_in_child(&link, Path::new("/"), &p);
        assert_eq!(route, "Relative");
        assert_eq!(Path::new(&rel), rel_want);
        assert!(came(&l));
    }

    // A symlink in the middle of the path, below the cwd: `<cwd>/link/...`
    // where link -> real. Cutting the cwd off gives "link/..." (descends
    // through the link); resolving gives "real/..." (same length here);
    // either names the socket in the real directory. The point is the place,
    // not the spelling.
    #[test]
    fn a_symlink_in_the_middle_of_the_path_still_lands_in_the_real_directory() {
        let t = tmp();
        let (_real, link) = real_and_link(t.path());
        let p = nest_tail(&link, 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let l = listen_at(&p);
        let (route, rel) = connect_in_child(t.path(), t.path(), &p);
        assert_eq!(route, "Relative");
        assert!(!has_dotdot(Path::new(&rel)), "{rel}");
        assert!(came(&l));
    }

    // The path is in a sibling of the cwd, so the spelling climbs with `..`:
    // cwd <t>/a/b, path <t>/c/... gives "../../c/...". Both sides are real
    // paths, so the climb goes out of the real cwd.
    #[test]
    fn a_path_beside_the_cwd_is_spelled_with_dotdot() {
        let t = tmp();
        let cwd = t.path().join("a/b");
        std::fs::create_dir_all(&cwd).unwrap();
        let p = nest_tail(&t.path().join("c"), 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let l = listen_at(&p);
        let (route, rel) = connect_in_child(&cwd, &cwd, &p);
        assert_eq!(route, "Relative");
        assert!(rel.starts_with("../../c/"), "{rel}");
        assert!(came(&l));
    }

    // A $PWD that names another directory than the cwd must not be used: cut
    // off the path, it would spell the right subpath relative to the wrong
    // directory. Both directories have the same subtree and a listener at the
    // same place in it, so using $PWD would silently connect to the cwd's
    // copy. The connection must reach the one in the $PWD directory (where the
    // path points), by climbing out of the real cwd.
    #[test]
    fn a_pwd_that_is_not_the_cwd_is_not_used() {
        let t = tmp();
        let cwd = t.path().join("one");
        let pwd = t.path().join("two");
        let p = nest_tail(&pwd, 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let sub = p.strip_prefix(&pwd).unwrap().to_path_buf();
        std::fs::create_dir_all(cwd.join(sub.parent().unwrap())).unwrap();
        let wanted = listen_at(&p);
        let decoy = listen_at(&cwd.join(&sub));
        let (route, rel) = connect_in_child(&cwd, &pwd, &p);
        assert_eq!(route, "Relative");
        assert!(rel.starts_with("../two/"), "{rel}");
        assert!(came(&wanted));
        assert!(!came(&decoy));
    }

    // A cwd whose name is a byte prefix of the path's next component (cwd
    // <t>/ab, path <t>/abc/...) is not a prefix of the path: cutting bytes
    // would spell "c/..." and name <t>/ab/c/.... The spelling climbs instead.
    #[test]
    fn a_cwd_that_is_only_a_byte_prefix_is_not_cut_off() {
        let t = tmp();
        let cwd = t.path().join("ab");
        std::fs::create_dir(&cwd).unwrap();
        let p = nest_tail(&t.path().join("abc"), 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let l = listen_at(&p);
        let (route, rel) = connect_in_child(&cwd, &cwd, &p);
        assert_eq!(route, "Relative");
        assert!(rel.starts_with("../abc/"), "{rel}");
        assert!(came(&l));
        assert!(!cwd.join("c").exists());
    }

    // The relative spelling is resolved against the cwd at the moment of the
    // connect, and the cwd is shared by every thread, so another thread that
    // changes it in between sends the connect to another directory. When the
    // cwd is still changed afterwards, Relative::run must notice and turn the
    // result into an error even though the connect itself succeeded: a stream
    // to the wrong server must not reach the caller as if it were at the path
    // they gave. The stream that did connect to a listener in the other
    // directory is closed, which that listener sees as end of file.
    //
    // The change is made from inside the call (the closure chdirs before it
    // connects), standing in for another thread. Runs in a child process
    // because it changes the cwd.
    #[test]
    fn a_cwd_changed_during_a_relative_connect_discards_the_result() {
        let t = tmp();
        let one = t.path().join("one");
        let two = t.path().join("two");
        let p = nest_tail(&one, 90, "s.sock");
        assert!(path::len(&p) > DIRECT_MAX);
        let sub = p.strip_prefix(&one).unwrap().to_path_buf();
        std::fs::create_dir_all(two.join(sub.parent().unwrap())).unwrap();
        run_child(
            "tests::posix::child_relative_with_the_cwd_changed_midway",
            &one,
            &[
                ("PWD", one.as_os_str()),
                ("SOCKAT_CHILD_PATH", p.as_os_str()),
                ("SOCKAT_CHILD_OTHER", two.as_os_str()),
            ],
        );
    }

    #[test]
    fn child_relative_with_the_cwd_changed_midway() {
        if !in_child() {
            return;
        }
        let p = env_path("SOCKAT_CHILD_PATH");
        let two = env_path("SOCKAT_CHILD_OTHER");
        let (dir, _) = path::split(&p).unwrap();
        let rel = Relative::new(&p, dir).unwrap();
        assert!(!has_dotdot(&rel.path), "{:?}", rel.path);
        // A listener in the other directory, at the same relative spelling.
        let place = tmp();
        let (listener, _) = route::run_long(
            BIND,
            &two.join(&rel.path),
            &config(&[at(place.path())], &[]),
        )
        .unwrap();
        let listener = UnixListener::from(listener);

        let e = rel
            .run(|q| {
                std::env::set_current_dir(&two).unwrap();
                route::plain_op(CONNECT_LAST, q)
            })
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Other, "{e}");
        assert_eq!(errno(&e), None, "{e}");
        let (mut server, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(server.read(&mut buf).unwrap(), 0, "the client is closed");
    }
}

// ---------------------------------------------------------------------------
// Apple (the widened sockaddr, O_SEARCH, LOCAL_PEERPID)
// ---------------------------------------------------------------------------

#[cfg(target_vendor = "apple")]
mod apple {
    use super::*;
    use crate::route::apple;
    use crate::sys::{Op, WIDE_PATH_MAX};
    use std::os::fd::AsFd;
    use std::os::unix::ffi::OsStrExt;

    /// The route order's inputs with no place for D and the thread-cwd means
    /// off (a connect has no route through D left; a bind forks beside the
    /// socket), `links` as the places for the symlink and the
    /// widened sockaddr on or off (`wide: false` stands in for a kernel that
    /// refuses it).
    fn cfg(links: &[&Path], wide: bool) -> Config {
        Config {
            wide,
            ..no_thread_cwd(config(&[], links))
        }
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

    /// The options of a blocking connect that may fork, for
    /// [`apple::wide_connect`].
    const CONNECT_OPTIONS: ConnectOptions = ConnectOptions {
        nonblocking: false,
        last_resort: true,
    };

    // XNU takes a socket address of up to SOCK_MAXADDRLEN (255) bytes, so the
    // widened form carries 255 - 2 = 253 bytes of path. Pinned against the
    // running kernel for a connect, the only call that widens: a socket bound
    // at a 253-byte path (through the route order, which never widens a bind)
    // is reached by a connect in the widened form with that path, and
    // SockAddr::wide refuses 254 before the kernel is asked.
    #[test]
    fn the_widened_sockaddr_carries_253_bytes() {
        assert_eq!(WIDE_PATH_MAX, 253);
        let t = tmp();
        let p = nest(t.path(), 253, "s.sock");
        let (l, _) = route::run(BIND, &p).unwrap();
        assert!(is_socket(&p));
        let c = apple::wide_connect(CONNECT_OPTIONS, &p).unwrap();
        ping(l, c);

        let t = tmp();
        let p = nest(t.path(), 254, "s.sock");
        let e = apple::wide_connect(CONNECT_OPTIONS, &p).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG));
    }

    /// The length of the address `getsockname(2)` reports for `sock`, asked
    /// with room for the widest address there is.
    fn reported_len(sock: &impl std::os::fd::AsFd) -> usize {
        sys::local_address_len(sock.as_fd()).unwrap()
    }

    /// The length of a `struct sockaddr_un`: what readers of a socket's
    /// address (std, CPython, Go) make room for.
    const SOCKADDR_UN_LEN: usize = std::mem::size_of::<libc::sockaddr_un>();

    // Why a bind never takes the widened form, pinned against the running
    // kernel: the address of a socket bound so does not fit a sockaddr_un.
    // getsockname reports the full widened length when the buffer has room,
    // and XNU fills a plain sockaddr_un (as std passes) to the brim with no
    // NUL, so std shows a path cut short. A reader that takes the reported
    // length at its word reads past a sockaddr_un (CPython then appends
    // unrelated bytes). Should a future XNU stop doing this, the decision can
    // be looked at again. Made with the system calls directly: sockat itself
    // has no way to bind so.
    #[test]
    fn the_address_of_a_widened_bind_does_not_fit_a_sockaddr_un() {
        let t = tmp();
        let p = nest(t.path(), 200, "s.sock");
        let sock = sys::socket(false).unwrap();
        sys::bind(sock.as_fd(), &SockAddr::wide(p.as_os_str()).unwrap()).unwrap();
        sys::listen(sock.as_fd()).unwrap();
        assert!(is_socket(&p));
        let reported = reported_len(&sock);
        assert!(reported > SOCKADDR_UN_LEN, "{reported}");
        let l = UnixListener::from(sock);
        let shown = super::reported(&l);
        assert_ne!(shown, p.as_path());
        eprintln!(
            "a widened bind: getsockname reports {reported} bytes, std shows {} bytes {shown:?}",
            path::len(&shown)
        );
        assert!(
            p.as_os_str()
                .as_bytes()
                .starts_with(shown.as_os_str().as_bytes())
        );
    }

    // A bind never takes the widened form, whatever the path, for each of the
    // spellings a connect widens: the path itself (200 bytes under /tmp), the
    // relative spelling (240 bytes below the cwd), and the file name of the
    // fork route (100 bytes, over `R/<name>`'s 92 but in the widened form).
    // The first two bind through D (thread-cwd, the default), and the
    // address the socket ends up with fits a sockaddr_un (getsockname reports
    // at most its length, and std shows the whole spelling); a connect to the
    // same path with no place for D widens where it can. A file name over 92
    // bytes fits no bind spelling: ENAMETOOLONG, with nothing created, while a
    // connect still reaches it widened (by the fork route, for connect_ex with the last resort).
    #[test]
    fn a_bind_never_widens_so_its_address_fits_a_sockaddr_un() {
        let t = tmp();
        let below = tmp_below_cwd();
        let place = tmp();
        let cwd = std::env::current_dir().unwrap();
        let rel_base = below.path().strip_prefix(&cwd).unwrap();
        let bind_cfg = config(&[at(place.path())], &[]);
        let connect_cfg = cfg(&[], true);
        for (what, p, connect_route) in [
            ("the path", nest(t.path(), 200, "wide.sock"), Route::Widened),
            (
                "the relative spelling",
                cwd.join(nest(rel_base, 240, "wrel.sock")),
                Route::WidenedRelative,
            ),
        ] {
            let (l, r) = route::run_long(BIND, &p, &bind_cfg).unwrap();
            assert_eq!(r, Route::ThreadCwd, "{what}");
            assert!(is_socket(&p), "{what}");
            let reported = reported_len(&l);
            assert!(reported <= SOCKADDR_UN_LEN, "{what}: {reported}");
            let listener = UnixListener::from(l);
            let shown = super::reported(&listener);
            assert!(path::len(&shown) <= DIRECT_MAX, "{what}: {shown:?}");
            let (cl, r) = route::run_long(CONNECT_LAST, &p, &connect_cfg).unwrap();
            assert_eq!(r, connect_route, "{what}");
            ping(OwnedFd::from(listener), cl);
        }

        let p = t.path().join("n".repeat(100));
        for c in [&bind_cfg, &no_thread_cwd(bind_cfg.clone()), &connect_cfg] {
            let e = route::run_long(BIND, &p, c).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
            assert!(!is_socket(&p));
        }
        assert_empty(place.path());
    }

    // The widened route of a connect: over DIRECT_MAX bytes, no relative spelling of at most DIRECT_MAX, and at most
    // 253 bytes, the path itself goes in the widened form. Under /tmp the
    // relative spelling from the package root climbs all the way up, so it is
    // long.
    #[test]
    fn a_path_up_to_253_bytes_takes_the_widened_route() {
        for total in [DIRECT_MAX + 1, 200, 253] {
            let t = tmp();
            let p = nest(t.path(), total, "s.sock");
            let c = cfg(&[Path::new("/nonexistent")], true);
            let (l, _) = route::run(BIND, &p).unwrap();
            let (cl, r) = route::run_long(CONNECT_LAST, &p, &c).unwrap();
            assert_eq!(r, Route::Widened, "{total}");
            ping(l, cl);
        }
    }

    // The path is over 253 bytes but its spelling relative to the cwd is at
    // most 253, so a connect widens that.
    #[test]
    fn a_path_over_253_bytes_short_relative_to_the_cwd_takes_the_widened_relative_route() {
        let t = tmp_below_cwd();
        let cwd = std::env::current_dir().unwrap();
        let rel_base = t.path().strip_prefix(&cwd).unwrap();
        let p = cwd.join(nest(rel_base, 240, "s.sock"));
        assert!(path::len(&p) > WIDE_PATH_MAX);
        let c = cfg(&[Path::new("/nonexistent")], true);
        let (l, _) = route::run(BIND, &p).unwrap();
        let (cl, r) = route::run_long(CONNECT_LAST, &p, &c).unwrap();
        assert_eq!(r, Route::WidenedRelative);
        ping(l, cl);
    }

    // The fork route of the connects that may fork puts the file name in the
    // plain sockaddr, and only a name too long for that (over 103 bytes) in
    // the widened one, when the kernel takes it. With the widened form refused
    // (`wide: false`) such a name is ENAMETOOLONG for connect_ex with the last resort too, and so
    // it is for a bind with or without it, so the socket is bound under a
    // short name and renamed to the long one.
    #[test]
    fn the_last_fork_widens_only_a_connects_name_too_long_for_the_plain_sockaddr() {
        let t = tmp();
        let short = nest(t.path(), 600, "s.sock");
        let p = short.with_file_name("n".repeat(150));
        let (wide, narrow) = (cfg(&[], true), cfg(&[], false));
        for c in [&wide, &narrow] {
            let e = route::run_long(BIND, &p, c).unwrap_err();
            assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
        }
        let (l, _) = route::run(BIND, &short).unwrap();
        std::fs::rename(&short, &p).unwrap();
        let (cl, r) = route::run_long(CONNECT_LAST, &p, &wide).unwrap();
        assert!(r.forked(), "{r:?}");
        ping(l, cl);
        let e = route::run_long(CONNECT, &p, &wide).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
        let e = route::run_long(CONNECT_LAST, &p, &narrow).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENAMETOOLONG), "{e}");
    }

    // The routes that work through a directory fd use the socket's directory
    // opened O_SEARCH on Apple, which needs only search permission, like a
    // plain bind / connect by path: pthread_fchdir_np and fchdir take such an
    // fd, and R in D resolves to the directory with search permission alone. A
    // directory of mode 0300 (write + search, no read) cannot be opened
    // O_RDONLY, yet the bind through D (thread cwd, and the fork in D with the
    // means off) binds in it, connect_ex with the last resort connects (thread cwd, or its last
    // fork with the means off and no place for the symlink), and so does the
    // last fork with no place for D. Without O_SEARCH the long path would fail where a short one
    // in the same directory works.
    #[test]
    fn the_routes_do_not_need_read_permission_on_the_directory() {
        let place = tmp();
        let with_d = config(&[at(place.path())], &[]);
        for (c, thread) in [(with_d.clone(), true), (no_thread_cwd(with_d), false)] {
            let t = tmp();
            let p = nest(t.path(), 400, "s.sock");
            let dir = p.parent().unwrap().to_path_buf();
            let _restore = unreadable(&dir);

            let (l, r) = route::run_long(BIND, &p, &c).unwrap();
            assert_eq!(r == Route::ThreadCwd, thread, "{r:?}");
            assert!(is_socket(&p));
            let listener = UnixListener::from(l);
            let (cl, _) = route::run_long(CONNECT_LAST, &p, &c).unwrap();
            ping_kept(&listener, cl);
            let (cl, r) = route::run_long(CONNECT_LAST, &p, &cfg(&[], true)).unwrap();
            assert!(r.forked(), "{r:?}");
            ping_kept(&listener, cl);
        }
        assert_empty(place.path());
    }

    // A kernel that does not take O_SEARCH on a directory answers EISDIR (as
    // macOS does for O_EXEC alone) or EINVAL. The fork route must then still
    // work, by opening the directory O_RDONLY instead, rather than fail on a
    // directory it could use. The running kernel takes O_SEARCH, so its
    // refusal is stood in for by handing that answer to search_or_read_only,
    // and the fd it returns is used the way the fork route uses it. Checked:
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

    // The fork route's connect is made by the child, so the server's
    // LOCAL_PEERPID names the child at first. XNU updates it when the real
    // client sends, after which it names the client process. Documented in
    // the crate docs; pinned so a change in XNU's behaviour is noticed. The
    // widened route connects from this process directly.
    #[test]
    fn local_peerpid_of_a_forked_connect_names_the_child_until_the_client_sends() {
        let me = std::process::id() as libc::pid_t;
        let t = tmp();
        let p = nest(t.path(), 400, "s.sock");
        let (dir, name) = path::split(&p).unwrap();
        let addr = SockAddr::standard(name).unwrap();
        let l = UnixListener::from(route::fork_op(BIND, dir, &addr).unwrap().0);
        let mut c = UnixStream::from(route::fork_op(CONNECT_LAST, dir, &addr).unwrap().0);
        let (mut s, _) = l.accept().unwrap();
        let before = sys::local_peer_pid(s.as_fd()).unwrap();
        assert_ne!(before, me);
        c.write_all(b"x").unwrap();
        let mut b = [0u8; 1];
        s.read_exact(&mut b).unwrap();
        assert_eq!(sys::local_peer_pid(s.as_fd()).unwrap(), me);

        let wp = nest(t.path(), 200, "w.sock");
        let wl = UnixListener::from(route::run(BIND, &wp).unwrap().0);
        let _wc = apple::wide_connect(CONNECT_OPTIONS, &wp).unwrap();
        let (ws, _) = wl.accept().unwrap();
        assert_eq!(sys::local_peer_pid(ws.as_fd()).unwrap(), me);
    }

    // ----- connect options, route by route -----

    /// Places and directories one [`ways`] call works in, removed at the end of
    /// the test.
    struct Dirs {
        t: tempfile::TempDir,
        below_cwd: tempfile::TempDir,
        place: tempfile::TempDir,
        links: tempfile::TempDir,
    }

    impl Dirs {
        fn new() -> Self {
            Self {
                t: tmp(),
                below_cwd: tmp_below_cwd(),
                place: tmp(),
                links: tmp(),
            }
        }
    }

    /// The Apple routes, each a [`Way`] to its own socket path, with the
    /// route each request must take there:
    ///
    /// | way | path, inputs | bind | connects |
    /// |---|---|---|---|
    /// | Direct | DIRECT_MAX bytes | Direct | Direct |
    /// | ThreadCwd | 600 bytes, one place for D | ThreadCwd | ThreadCwd |
    /// | ForkInD | 600, thread cwd off, one place | fork in D | fork (the last resort) |
    /// | Relative | below the cwd, no place for D | fork beside | Relative |
    /// | Wide | 200 bytes, no place | fork beside | Wide |
    /// | WideRelative | 240 relative, no place | fork beside | WideRelative |
    /// | Symlink | 600, no place, a link place | fork beside | Symlink |
    /// | Fork | 600, no place, no link place | fork beside | fork (the last resort) |
    fn ways(d: &Dirs) -> Vec<(&'static str, Way, Want)> {
        let t = d.t.path();
        let cwd = std::env::current_dir().unwrap();
        let rel_base = d.below_cwd.path().strip_prefix(&cwd).unwrap();
        let place = config(&[at(d.place.path())], &[]);
        let none = Path::new("/nonexistent");
        let direct = nest(t, DIRECT_MAX, "direct.sock");
        let bind_forks = |connected: Route| {
            move |req: Request, r: Route| match req {
                Request::Bind(_) => r.forked(),
                Request::Connect(_) => r == connected,
            }
        };
        let cases: Vec<(&'static str, PathBuf, Config, Want)> = vec![
            (
                "ThreadCwd",
                nest(t, 600, "thread.sock"),
                place.clone(),
                std::sync::Arc::new(|_, r| r == Route::ThreadCwd),
            ),
            (
                "ForkInD",
                nest(t, 600, "forkd.sock"),
                no_thread_cwd(place),
                std::sync::Arc::new(|_, r: Route| r.forked()),
            ),
            (
                "Relative",
                cwd.join(nest(rel_base, DIRECT_MAX, "rel.sock")),
                cfg(&[none], true),
                std::sync::Arc::new(bind_forks(Route::Relative)),
            ),
            (
                "Wide",
                nest(t, 200, "wide.sock"),
                cfg(&[none], true),
                std::sync::Arc::new(bind_forks(Route::Widened)),
            ),
            (
                "WideRelative",
                cwd.join(nest(rel_base, 240, "wrel.sock")),
                cfg(&[none], true),
                std::sync::Arc::new(bind_forks(Route::WidenedRelative)),
            ),
            (
                "Symlink",
                nest(t, 600, "sym.sock"),
                cfg(&[d.links.path()], true),
                std::sync::Arc::new(bind_forks(Route::SymlinkFile)),
            ),
            (
                "Fork",
                nest(t, 600, "fork.sock"),
                cfg(&[], true),
                std::sync::Arc::new(|_, r: Route| r.forked()),
            ),
        ];
        let mut ways: Vec<(&'static str, Way, Want)> = vec![(
            "Direct",
            std::sync::Arc::new(move |req| route::run(req, &direct)),
            std::sync::Arc::new(|_, r| r == Route::Standard),
        )];
        ways.extend(cases.into_iter().map(|(what, p, c, want)| {
            let way: Way = std::sync::Arc::new(move |req| route::run_long(req, &p, &c));
            (what, way, want)
        }));
        ways
    }

    // A nonblocking connect must come back at once and stay O_NONBLOCK on
    // every route. On macOS a full backlog already fails at once with
    // ECONNREFUSED for a blocking connect too, so what differs is the socket
    // that comes back; the routes that hand a plain sockaddr to std (the path
    // itself, the relative spelling, the symlink) must use sockat's own
    // socket for it (std cannot connect a nonblocking socket), the thread-cwd
    // means must make the socket O_NONBLOCK before handing it to the other
    // thread, and the fork route must make the parent's socket O_NONBLOCK
    // before the fork. See check_nonblocking_connect for what is checked on
    // each.
    #[test]
    fn a_nonblocking_connect_fails_at_once_on_a_full_backlog_on_every_route() {
        let d = Dirs::new();
        for (what, way, want) in ways(&d) {
            check_nonblocking_connect(&way, &want, what);
        }
        assert_empty(d.place.path());
        assert_empty(d.links.path());
    }

    // DR-0010, route by route: only the fork route connects from another
    // process (the child; LOCAL_PEERPID names it until this process sends),
    // so only there do the connects that may fork report a fork and the ones
    // that may not fail with ENAMETOOLONG. Every other route connects from
    // this process for all four, the thread-cwd means included (its connect
    // is made by another thread of this process, and LOCAL_PEERPID names the
    // process). See check_fork_and_peer.
    #[test]
    fn only_fork_cwd_direct_reports_a_fork_or_needs_the_last_resort() {
        let d = Dirs::new();
        for (what, way, want) in ways(&d) {
            check_fork_and_peer(&way, &want, matches!(what, "Fork" | "ForkInD"), what);
        }
        assert_empty(d.place.path());
        assert_empty(d.links.path());
    }
}
