//! Route-level tests: each way of reaching a long path is forced and checked
//! for where the socket file ends up, that data flows, what the spelling is,
//! what is left in B and D, and how failures come out. The public API is
//! tested separately in `tests/`.
//!
//! What every test here leans on:
//!
//! * The socket must appear at the path the caller gave (checked with
//!   `symlink_metadata(...).file_type().is_socket()`), not at whatever
//!   spelling was handed to the kernel. A route that "works" but creates the
//!   socket elsewhere is the failure these tests exist to catch.
//! * Which route is taken is forced through the route order's inputs
//!   ([`Config`]: the places for B, where `/proc` is, whether the thread-cwd
//!   means, the widened sockaddr and the bind's link-file may be used, how
//!   the socket's directory is opened), never through the process
//!   environment. Each test has a place of its own (a fresh temporary
//!   directory), so its B is its own although the registry of Bs is
//!   process-wide.
//! * Temporary directories have random names and are removed when the test
//!   ends (`tempfile::TempDir`).
//! * What needs a process of its own (one that ends, so its D is dead; a
//!   umask; a cwd) runs in a child process of this test binary
//!   ([`run_child`]): the test runner is multi-threaded, and those are
//!   process-wide.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::base::{self, Place};
use crate::path;
use crate::route::{self, Config, Connect, DIRECT_MAX, Follows, Reached, Request, names};
use crate::sys::{self, SockType};
use crate::{Options, SocketFile, Spelling, Until};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A fresh, randomly named directory under `/tmp` (short, so the tests decide
/// the path lengths), removed at the end of the test. Mode 0700 whatever the
/// umask, so whether another uid can search the way to a B in it is known
/// (not).
fn tmp() -> tempfile::TempDir {
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    t
}

/// A test's directory and its own place for B (`<dir>/p`).
fn setup() -> (tempfile::TempDir, PathBuf) {
    let t = tmp();
    let place = t.path().join("p");
    std::fs::create_dir(&place).unwrap();
    (t, place)
}

/// Create directories under `base` so that `<dirs>/<name>` is exactly `total`
/// bytes long, and return that socket path.
fn nest(base: &Path, total: usize, name: &str) -> PathBuf {
    let fixed = path::len(base) + 1 + name.len();
    assert!(total >= fixed, "{total} < {fixed}");
    let mut remaining = total - fixed;
    let mut dir = base.to_path_buf();
    while remaining > 0 {
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

fn is_socket(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_socket())
}

/// Exchange a message in each direction between a client and the stream the
/// listener accepts for it.
fn ping(listener: &UnixListener, client: OwnedFd) {
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

/// A listener at `p` (any length), made by std at a short path in the same
/// file system and renamed there, so a connect test does not lean on the
/// bind routes.
fn listen_at(p: &Path) -> UnixListener {
    let short = std::env::temp_dir().join(format!("sockat-l-{}", base::random_name().unwrap()));
    let short = if path::len(&short) <= DIRECT_MAX {
        short
    } else {
        PathBuf::from(format!("/tmp/sockat-l-{}", base::random_name().unwrap()))
    };
    let l = UnixListener::bind(&short).unwrap();
    std::fs::rename(&short, p).unwrap();
    l
}

const BIND: Request<'static> = Request::Bind(SockType::Stream);
const CONNECT: Request<'static> = Request::Connect(Connect::Stream { nonblocking: false });

/// The route order's inputs as outside the tests, except that the only place
/// for B is `place` (its B is `<real path of place>/sockat-<uid>`), and the B
/// of the places is held as by a process of its own (clones of the config
/// share it).
fn config(place: &Path) -> Config {
    let mut c = Config::from_vars(None, None);
    c.places = vec![(Place::Tmp, place.to_path_buf())];
    c.held = Default::default();
    c
}

/// `c` with `/proc` looked for where there is nothing, standing in for a
/// system without it.
fn no_proc(mut c: Config) -> Config {
    c.proc = PathBuf::from("/nonexistent/proc");
    c
}

/// `c` with the thread-cwd means off, standing in for a system where it
/// cannot be used (a seccomp filter, no `pthread_fchdir_np`).
fn no_thread(c: Config) -> Config {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    return Config {
        thread_cwd: false,
        ..c
    };
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    c
}

/// `c` with the bind's link-file out of the order.
fn no_link_file(c: Config) -> Config {
    Config {
        follows: Follows::No,
        ..c
    }
}

/// `c` with the connect routes that need neither writing nor a thread
/// unusable: the socket's directory cannot be opened (no proc-direct,
/// at-direct, thread-cwd-direct) and the widened sockaddr off (Apple).
/// relative is left to the path: one hundreds of bytes long under `/tmp` is
/// too far from the cwd for it.
fn no_direct(c: Config) -> Config {
    #[allow(unused_mut)]
    let mut c = Config {
        open_socket_dir: |_| Err(io::Error::from(io::ErrorKind::PermissionDenied)),
        ..c
    };
    #[cfg(target_vendor = "apple")]
    {
        c.wide = false;
    }
    c
}

/// The B link-dir uses on every target: B's real path, no link-file.
fn real_link_dir(place: &Path) -> Config {
    no_link_file(no_proc(config(place)))
}

fn opts() -> Options {
    Options::default()
}

/// Bind `p` with `cfg`, taking the socket file as `bind_ex` does.
fn bind(p: &Path, options: &Options, cfg: &Config) -> Result<Reached, route::Failed> {
    route::run(BIND, p, options, true, Some(cfg))
}

fn connect(p: &Path, cfg: &Config) -> Result<Reached, route::Failed> {
    route::run(CONNECT, p, &opts(), false, Some(cfg))
}

fn listener(r: &mut Reached) -> UnixListener {
    UnixListener::from(r.socket.take().unwrap())
}

/// B of a test's only place.
fn b_of(place: &Path) -> PathBuf {
    std::fs::canonicalize(place)
        .unwrap()
        .join(format!("sockat-{}", sys::effective_uid()))
}

/// The names in `dir`, sorted (none when it cannot be read).
fn entries(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

/// What each D in B holds, as `D/E`, and `D/` for an empty D.
fn in_b(b: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for d in entries(b) {
        let inside = entries(&b.join(&d));
        if inside.is_empty() {
            out.push(format!("{d}/"));
        }
        for e in inside {
            out.push(format!("{d}/{e}"));
        }
    }
    out
}

/// No E in any D of `b` (the D themselves may be there).
fn assert_no_e(b: &Path) {
    let left = in_b(b);
    assert!(
        left.iter().all(|e| e.ends_with('/')),
        "E left in B: {left:?}"
    );
}

fn errno(e: &io::Error) -> Option<i32> {
    e.raw_os_error()
}

fn skipped_names(s: &[route::Skipped]) -> Vec<&'static str> {
    s.iter().map(|s| s.name).collect()
}

/// Whether `/proc` can be used here (Linux).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn proc_here() -> bool {
    cfg!(any(target_os = "linux", target_os = "android")) && Path::new("/proc/self/fd").exists()
}

/// Whether the thread-cwd means can be used here: Linux without a seccomp
/// filter (docker's default profile installs one), Apple with
/// `pthread_fchdir_np`.
fn thread_here() -> bool {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    return sys::thread_cwd_available();
    #[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
    false
}

/// What `listener`'s `local_addr()` reports: the spelling the kernel keeps.
fn reported(listener: &UnixListener) -> PathBuf {
    listener
        .local_addr()
        .unwrap()
        .as_pathname()
        .unwrap()
        .to_path_buf()
}

/// Run one test of this test binary in a child process with `env` added, and
/// require it to pass; its stdout is returned. The child tests return early
/// (and pass) when `SOCKAT_CHILD` is not set, which is how they look in a
/// normal run.
fn run_child(test: &str, env: &[(&str, &OsStr)]) -> String {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
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
    assert!(
        stdout.contains("1 passed"),
        "child {test} did not run\n{stdout}"
    );
    stdout
}

fn in_child() -> bool {
    std::env::var_os("SOCKAT_CHILD").is_some()
}

fn env_path(key: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("{key} not set")))
}

/// Bind `path` (with B in `place`) in a child process that then exits, and
/// return the spelling it bound by. Its D is then a dead process's D, and
/// the socket file is one nobody listens on.
fn bind_in_child(path: &Path, place: &Path) -> String {
    let out = run_child(
        "tests::child_bind_and_exit",
        &[
            ("SOCKAT_CHILD_PATH", path.as_os_str()),
            ("SOCKAT_CHILD_PLACE", place.as_os_str()),
        ],
    );
    // libtest prints `test <name> ... ` before the test's own output.
    let at = out.find("SPELLING=").unwrap() + "SPELLING=".len();
    out[at..].lines().next().unwrap().to_owned()
}

#[test]
fn child_bind_and_exit() {
    if !in_child() {
        return;
    }
    let p = env_path("SOCKAT_CHILD_PATH");
    let cfg = no_link_file(config(&env_path("SOCKAT_CHILD_PLACE")));
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    let l = listener(&mut r);
    println!("SPELLING={}", reported(&l).display());
}

// ---------------------------------------------------------------------------
// short paths
// ---------------------------------------------------------------------------

// A path std accepts goes to std unchanged: the longest one (sun_path less
// the NUL: 107 on Linux / illumos, 103 on Apple and the BSDs) is bound by
// std (route standard, its own address, nothing made in the place), and one
// byte more is not (a route through B, with standard passed over). The
// boundary is the one std itself draws, so every path std can take behaves
// as with std.
#[test]
fn the_longest_path_std_accepts_goes_to_std_and_one_byte_more_does_not() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let at_max = nest(t.path(), DIRECT_MAX, "a.sock");
    let mut r = bind(&at_max, &opts(), &cfg).unwrap();
    assert_eq!(r.route, names::STANDARD);
    assert_eq!(r.spelling, Some(Spelling::Original));
    assert_eq!(r.base, None);
    assert!(r.skipped.is_empty());
    assert_eq!(reported(&listener(&mut r)), at_max);
    assert!(entries(&place).is_empty(), "a short path made B");

    let over = nest(t.path(), DIRECT_MAX + 1, "b.sock");
    // The boundary is std's own: std refuses one byte more (and took
    // DIRECT_MAX above).
    let refused = UnixListener::bind(&over).unwrap_err();
    assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
    let r = bind(&over, &opts(), &cfg).unwrap();
    assert_eq!(r.route, names::LINK_DIR);
    assert_eq!(skipped_names(&r.skipped), vec![names::STANDARD]);
    assert!(is_socket(&over));
}

// A relative path std accepts goes to std as it is: its spelling is
// CwdRelative (after the cwd changes it names another place, a hazard std
// has on its own, reported all the same), an absolute one Original; a
// failure there is std's own, from the route standard.
#[test]
fn a_short_path_is_original_or_cwd_relative() {
    let rel = route::resolve(route::Use::Bind, Path::new("s.sock"), &opts(), None).unwrap();
    assert_eq!(rel.spelling, Spelling::CwdRelative);
    assert_eq!(rel.path, Path::new("s.sock"));
    let abs = route::resolve(route::Use::Bind, Path::new("/x/s.sock"), &opts(), None).unwrap();
    assert_eq!(abs.spelling, Spelling::Original);
    let f = route::run(
        BIND,
        Path::new("target/sockat-no-such-directory/s.sock"),
        &opts(),
        true,
        None,
    )
    .err()
    .unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENOENT));
    assert_eq!(f.route, Some(names::STANDARD));
}

// ---------------------------------------------------------------------------
// bind: link-dir
// ---------------------------------------------------------------------------

// link-dir through B's real path: the spelling is `<B>/<D>/<E>/<name>`, B
// being `<place's real path>/sockat-<uid>`, D 10 random letters and digits,
// E the base-36 counter of D (`0` first). The socket is at the path and
// reached through the spelling. The spelling is Linked: removable (unlinking
// it goes through E, a symlink to the directory), followed until cleanup,
// not by another uid here (the temporary place is 0700), and not by a peer
// with a /tmp of its own (B is under /tmp). The diagnostic base names the
// place. E stays after the bind (the kernel keeps the spelling); cleanup
// removes it once (a second cleanup does nothing), and D stays.
#[test]
fn link_dir_through_bs_real_path() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    assert_eq!(r.route, names::LINK_DIR);
    assert_eq!(r.base, Some("tmp"));
    assert_eq!(
        r.spelling,
        Some(Spelling::Linked {
            removable: true,
            until: Until::Cleanup,
            usable_cross_uid: false,
            usable_with_private_tmp: false,
        })
    );
    assert_eq!(skipped_names(&r.skipped), vec![names::STANDARD]);
    let b = b_of(&place);
    let l = listener(&mut r);
    let spelling = reported(&l);
    let rest = spelling.strip_prefix(&b).unwrap();
    let parts: Vec<_> = rest
        .iter()
        .map(|c| c.to_string_lossy().into_owned())
        .collect();
    assert_eq!(parts.len(), 3, "{spelling:?}");
    assert_eq!(parts[0].len(), base::RANDOM_LEN);
    assert!(parts[0].bytes().all(|c| c.is_ascii_alphanumeric()));
    assert_eq!(parts[1], "0");
    assert_eq!(parts[2], "s.sock");
    assert!(is_socket(&p));
    ping(&l, UnixStream::connect(&spelling).unwrap().into());
    assert_eq!(in_b(&b), vec![format!("{}/0", parts[0])]);
    r.link.as_ref().unwrap().cleanup().unwrap();
    assert_eq!(in_b(&b), vec![format!("{}/", parts[0])]);
    r.link.as_ref().unwrap().cleanup().unwrap();
}

// B and D are made with mode 0711, so another uid can follow a spelling
// through them where the way there allows it; D carries a shared flock this
// process holds, so another open of it cannot take an exclusive one (that is
// how sweep and systemd-tmpfiles tell a live D).
#[test]
fn b_and_d_are_0711_and_d_is_locked() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    let _r = bind(&p, &opts(), &cfg).unwrap();
    let b = b_of(&place);
    assert_eq!(std::fs::metadata(&b).unwrap().mode() & 0o7777, 0o711);
    let d = b.join(entries(&b).pop().unwrap());
    assert_eq!(std::fs::metadata(&d).unwrap().mode() & 0o7777, 0o711);
    let f = std::fs::File::open(&d).unwrap();
    assert!(!sys::lock(f.as_fd(), sys::Lock::Exclusive).unwrap());
}

// One D per (process, B): many binds and connects at once on many threads
// share this process's D (made once), each bind gets its own E (the counter
// never hands out a name twice), and every socket is at its own path.
#[test]
fn concurrent_binds_share_one_d_and_get_their_own_e() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let paths: Vec<PathBuf> = (0..16)
        .map(|i| nest(&t.path().join(format!("t{i:02}")), 200, "s.sock"))
        .collect();
    std::thread::scope(|s| {
        for p in &paths {
            let cfg = &cfg;
            s.spawn(move || {
                let mut r = bind(p, &opts(), cfg).unwrap();
                assert_eq!(r.route, names::LINK_DIR);
                let l = listener(&mut r);
                ping(&l, connect(p, cfg).unwrap().socket.unwrap());
            });
        }
    });
    let b = b_of(&place);
    let ds = entries(&b);
    assert_eq!(ds.len(), 1, "{ds:?}");
    assert_eq!(entries(&b.join(&ds[0])).len(), paths.len());
    for p in &paths {
        assert!(is_socket(p));
    }
}

// The spelling names nothing once cleaned up: unlinking it then finds
// nothing (ENOENT), and the files of the socket's directory (the socket, an
// unrelated file) are untouched. That is why E sits in a D of this process
// with a random name, rather than the path being spelled by an fd number or
// the file name alone, which a later lookup could resolve to an unrelated
// file.
#[test]
fn a_spelling_cleaned_up_names_nothing() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    let other = p.with_file_name("other");
    std::fs::write(&other, b"keep").unwrap();
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    let spelling = reported(&listener(&mut r));
    r.link.as_ref().unwrap().cleanup().unwrap();
    let e = std::fs::remove_file(&spelling).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOENT));
    assert!(is_socket(&p));
    assert_eq!(std::fs::read(&other).unwrap(), b"keep");
}

// A bind through link-dir that fails for real (EADDRINUSE: a file is at the
// path) is that failure, from link-dir, and the E made for it is removed
// before the error is returned: a failed bind leaves no E. The routes after
// it are not tried (they would fail the same way), so nothing but standard
// is in skipped.
#[test]
fn a_failed_bind_through_link_dir_removes_its_e_and_is_not_retried() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    std::fs::write(&p, b"").unwrap();
    let f = bind(&p, &opts(), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::EADDRINUSE));
    assert_eq!(f.route, Some(names::LINK_DIR));
    assert_eq!(skipped_names(&f.skipped), vec![names::STANDARD]);
    assert_no_e(&b_of(&place));
}

// link-dir through /proc/<pid>/fd/<B's fd> (Linux with /proc), tried before
// B's real path: the spelling goes through this process's fd of B by this
// process's pid (not `self`, which another process would read as its own).
// It is followed while this process lives (until Process), by the same uid
// only, by a peer with a /tmp of its own too (the spelling goes through no
// /tmp), and the diagnostic base is `proc`.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn link_dir_through_proc_on_linux() {
    if !proc_here() {
        eprintln!("/proc cannot be used here");
        return;
    }
    let (t, place) = setup();
    let cfg = config(&place);
    let p = nest(t.path(), 200, "s.sock");
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    assert_eq!(r.route, names::LINK_DIR);
    assert_eq!(r.base, Some("proc"));
    assert_eq!(
        r.spelling,
        Some(Spelling::Linked {
            removable: true,
            until: Until::Process,
            usable_cross_uid: false,
            usable_with_private_tmp: true,
        })
    );
    let l = listener(&mut r);
    let spelling = reported(&l);
    let prefix = format!("/proc/{}/fd/", std::process::id());
    assert!(spelling.starts_with(&prefix), "{spelling:?}");
    ping(&l, UnixStream::connect(&spelling).unwrap().into());
}

// The forms of B's spelling are tried in order, procB first, then B's real
// path, and they are one route (link-dir) in skipped, with the last reason.
// /proc is stood in for by a directory where `<it>/<pid>/fd/<B's fd>` is a
// symlink to B (what Linux's /proc/<pid>/fd/<fd> resolves to), so this runs
// on every OS: the procB form is checked to name B (the same `(st_dev,
// st_ino)` as the fd) before it is used. A file name that fits through
// procB takes it (base `proc`, until Process, private tmp true); one too long
// for both forms leaves a single link-dir entry, too long.
#[test]
fn the_forms_of_b_are_tried_in_order_as_one_route() {
    let (t, place) = setup();
    let fake = t.path().join("proc");
    let mut cfg = real_link_dir(&place);
    cfg.proc = fake.clone();
    // B made by a first bind (through B's real path: nothing at the fake yet).
    let first = bind(&nest(&t.path().join("a"), 200, "s.sock"), &opts(), &cfg).unwrap();
    assert_eq!(first.base, Some("tmp"));
    let b = b_of(&place);
    let fd_dir = fake.join(std::process::id().to_string()).join("fd");
    std::fs::create_dir_all(&fd_dir).unwrap();
    let fd = base::registered_fd(&b).unwrap();
    std::os::unix::fs::symlink(&b, fd_dir.join(fd.to_string())).unwrap();

    let r = bind(&nest(&t.path().join("b"), 200, "s.sock"), &opts(), &cfg).unwrap();
    assert_eq!(r.base, Some("proc"));
    assert_eq!(
        r.spelling,
        Some(Spelling::Linked {
            removable: true,
            until: Until::Process,
            usable_cross_uid: false,
            usable_with_private_tmp: true,
        })
    );

    let long = "n".repeat(DIRECT_MAX - path::len(&b) - 5);
    let r = bind(&nest(&t.path().join("c"), 300, &long), &opts(), &cfg);
    let skipped = match r {
        Ok(r) => r.skipped,
        Err(f) => f.skipped,
    };
    let link_dir: Vec<_> = skipped
        .iter()
        .filter(|s| s.name == names::LINK_DIR)
        .collect();
    assert_eq!(link_dir.len(), 1, "{skipped:?}");
    assert_eq!(link_dir[0].reason, names::TOO_LONG);
}

// A procB spelling names nothing once the process that bound it is gone.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_proc_spelling_names_nothing_after_the_process_ends() {
    if !proc_here() {
        eprintln!("/proc cannot be used here");
        return;
    }
    let (t, place) = setup();
    let p = nest(t.path(), 200, "s.sock");
    let spelling = bind_in_child(&p, &place);
    assert!(spelling.starts_with("/proc/"), "{spelling}");
    assert!(is_socket(&p));
    assert!(std::fs::symlink_metadata(&spelling).is_err());
}

// ---------------------------------------------------------------------------
// bind: thread-cwd / at, link-file
// ---------------------------------------------------------------------------

/// A file name too long for `<B>/<D>/<E>/<name>` through a test's B, short
/// enough for `E/<name>` (thread-cwd, at).
fn name_past_link_dir() -> String {
    "n".repeat(DIRECT_MAX - base::RANDOM_LEN - 1 - 5)
}

// A file name too long for link-dir (here without /proc and link-file) goes
// on to thread-cwd (Linux, Apple) or at (FreeBSD), with `E/<name>` relative
// to D; elsewhere (NetBSD, OpenBSD, illumos, or where the thread cwd cannot
// be used) it is ENAMETOOLONG with no route. link-dir is passed over as one
// entry, too long. The spelling is Dangling (E is removed right after the
// bind) and goes through no B, so base is None; D is left, without E.
#[test]
fn a_file_name_too_long_for_link_dir_goes_on() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 250, &name_past_link_dir());
    let r = bind(&p, &opts(), &cfg);
    let has_route = cfg!(target_os = "freebsd")
        || (cfg!(any(
            target_os = "linux",
            target_os = "android",
            target_vendor = "apple"
        )) && thread_here());
    if !has_route {
        let f = r.err().unwrap();
        assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG));
        assert_eq!(f.route, None);
        return;
    }
    let mut r = r.unwrap();
    let want = if cfg!(target_os = "freebsd") {
        names::AT
    } else {
        names::THREAD_CWD
    };
    assert_eq!(r.route, want);
    assert_eq!(r.spelling, Some(Spelling::Dangling));
    assert_eq!(r.base, None);
    assert_eq!(
        r.skipped
            .iter()
            .map(|s| (s.name, s.reason))
            .collect::<Vec<_>>(),
        vec![
            (names::STANDARD, names::TOO_LONG),
            (names::LINK_DIR, names::TOO_LONG)
        ]
    );
    assert!(is_socket(&p));
    let l = listener(&mut r);
    ping(&l, connect(&p, &cfg).unwrap().socket.unwrap());
    assert_no_e(&b_of(&place));
    let spelling = reported(&l);
    assert_eq!(spelling.iter().count(), 2);
    assert_eq!(spelling.iter().next().unwrap().len(), base::RANDOM_LEN);
}

// thread-cwd moves the working directory of a thread of its own only: the
// process's cwd is as it was after many binds and connects through it.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn thread_cwd_leaves_the_process_cwd_alone() {
    if !thread_here() {
        eprintln!("thread-cwd cannot be used here");
        return;
    }
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let before = std::env::current_dir().unwrap();
    for i in 0..8 {
        let p = nest(&t.path().join(format!("{i}")), 250, &name_past_link_dir());
        let mut r = bind(&p, &opts(), &cfg).unwrap();
        assert_eq!(r.route, names::THREAD_CWD);
        let c = connect(&p, &no_direct(cfg.clone())).unwrap();
        ping(&listener(&mut r), c.socket.unwrap());
        assert_eq!(std::env::current_dir().unwrap(), before);
    }
}

// thread-cwd and thread-cwd-direct move the working directory of a thread
// sockat starts for the call, never the caller's: a caller thread that keeps
// a working directory of its own (`pthread_fchdir_np` on Apple,
// `unshare(CLONE_FS)` + chdir on Linux) still has it after a bind and a
// connect through them (moving the caller's thread and back with
// `pthread_fchdir_np(-1)` would drop the caller's own setting), and the
// process's cwd is unchanged.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn the_callers_own_thread_cwd_is_left_as_it_is() {
    if !thread_here() {
        eprintln!("thread-cwd cannot be used here");
        return;
    }
    let (t, place) = setup();
    let x = tmp();
    let cwd = std::env::current_dir().unwrap();
    let p = nest(t.path(), 250, &name_past_link_dir());
    let x_real = std::fs::canonicalize(x.path()).unwrap();
    let x_path = x.path().to_path_buf();
    let seen = std::thread::spawn(move || {
        #[cfg(target_vendor = "apple")]
        {
            let x = base::open_dir(&x_path).unwrap();
            sys::set_thread_cwd(x.as_fd()).unwrap();
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            sys::unshare_fs().unwrap();
            std::env::set_current_dir(&x_path).unwrap();
        }
        let before = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let cfg = real_link_dir(&place);
        let mut r = bind(&p, &opts(), &cfg).unwrap();
        assert_eq!(r.route, names::THREAD_CWD);
        let l = listener(&mut r);
        let c = connect(&p, &no_direct(cfg.clone())).unwrap();
        ping(&l, c.socket.unwrap());
        #[allow(unused_mut)]
        let mut cfg = no_proc(cfg);
        #[cfg(target_vendor = "apple")]
        {
            cfg.wide = false;
        }
        let c = connect(&p, &cfg).unwrap();
        assert_eq!(c.route, names::THREAD_CWD_DIRECT);
        ping(&l, c.socket.unwrap());
        let after = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        (before, after)
    })
    .join()
    .unwrap();
    assert_eq!(seen.0, x_real, "the premise: the thread's own cwd is X");
    assert_eq!(seen.1, x_real);
    assert_eq!(std::env::current_dir().unwrap(), cwd);
}

// sockat's own sockets are set up like std's: where std sets SO_NOSIGPIPE on
// its Unix sockets (Apple, FreeBSD, NetBSD, DragonFly), so does sockat, so a
// write to a closed peer returns EPIPE instead of raising SIGPIPE whatever
// the route. Compared with std's own socket on the same system, so the
// promise is "the same as std".
#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
#[test]
fn sockets_carry_so_nosigpipe_where_std_sets_it() {
    let (t, place) = setup();
    let short = t.path().join("std.sock");
    let std_listener = UnixListener::bind(&short).unwrap();
    let std_client = UnixStream::connect(&short).unwrap();
    let std_says = sys::so_nosigpipe(std_client.as_fd()).unwrap();
    assert_eq!(sys::so_nosigpipe(std_listener.as_fd()).unwrap(), std_says);
    let cfg = real_link_dir(&place);
    let long = nest(t.path(), 250, &name_past_link_dir());
    let mut r = bind(&long, &opts(), &cfg).unwrap();
    let l = listener(&mut r);
    assert_eq!(sys::so_nosigpipe(l.as_fd()).unwrap(), std_says);
    let nb = Request::Connect(Connect::Stream { nonblocking: true });
    let c = route::run(nb, &long, &opts(), false, Some(&cfg)).unwrap();
    assert_eq!(
        sys::so_nosigpipe(c.socket.unwrap().as_fd()).unwrap(),
        std_says
    );
}

// link-file (a bind that follows a symlink in the last component: macOS,
// NetBSD): a file name too long for link-dir is bound through `<B>/<D>/<E>`,
// E a symlink to the socket file, so the spelling holds no file name and any
// length is reached. Unlinking the spelling would remove E, not the file
// (removable false).
#[cfg(any(target_vendor = "apple", target_os = "netbsd"))]
#[test]
fn link_file_reaches_any_file_name_length() {
    let (t, place) = setup();
    let cfg = no_proc(config(&place));
    let p = nest(t.path(), 400, &"n".repeat(200));
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    assert_eq!(r.route, names::LINK_FILE);
    assert!(matches!(
        r.spelling,
        Some(Spelling::Linked {
            removable: false,
            until: Until::Cleanup,
            ..
        })
    ));
    assert_eq!(
        skipped_names(&r.skipped),
        vec![names::STANDARD, names::LINK_DIR]
    );
    assert!(is_socket(&p));
    let l = listener(&mut r);
    ping(&l, connect(&p, &cfg).unwrap().socket.unwrap());
}

// Where a bind follows the symlink, a link-file bind onto a path with a file
// at it fails for real (EADDRINUSE): that is the result, E is removed, and
// the next route is not tried.
#[cfg(any(target_vendor = "apple", target_os = "netbsd"))]
#[test]
fn a_failed_link_file_bind_is_the_failure() {
    let (t, place) = setup();
    let cfg = no_proc(config(&place));
    let p = nest(t.path(), 400, &"n".repeat(200));
    std::fs::write(&p, b"").unwrap();
    let f = bind(&p, &opts(), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::EADDRINUSE));
    assert_eq!(f.route, Some(names::LINK_FILE));
    assert_no_e(&b_of(&place));
}

// Where whether a bind follows the symlink is not known (told at run time),
// a failed link-file bind with nothing at the path means the kernel did not
// follow it: link-file is passed over, its E removed, and the next route
// tried. With something at the path the same failure is the result. Forced
// here on a kernel that does not follow (it binds the symlink's own name,
// which exists: EADDRINUSE).
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "illumos"
))]
#[test]
fn link_file_told_at_run_time_is_passed_over_when_not_followed() {
    let (t, place) = setup();
    let cfg = Config {
        follows: Follows::Unknown,
        ..no_proc(config(&place))
    };
    let p = nest(t.path(), 250, &name_past_link_dir());
    let skipped = match bind(&p, &opts(), &cfg) {
        Ok(r) => r.skipped,
        Err(f) => f.skipped,
    };
    let lf = skipped.iter().find(|s| s.name == names::LINK_FILE).unwrap();
    assert_eq!(lf.reason, names::NOT_FOLLOWED);
    assert_no_e(&b_of(&place));

    let q = nest(&t.path().join("q"), 250, &name_past_link_dir());
    std::fs::write(&q, b"").unwrap();
    let f = bind(&q, &opts(), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::EADDRINUSE));
    assert_eq!(f.route, Some(names::LINK_FILE));
    assert_no_e(&b_of(&place));
}

// ---------------------------------------------------------------------------
// B: the places and Options.base
// ---------------------------------------------------------------------------

// A place reached through a directory another user could change (one
// writable by its group, not sticky) is not used: B goes in the next place.
// Another user could otherwise replace a directory on the way, and B with
// it.
#[test]
fn a_place_another_user_could_change_is_passed_over() {
    let (t, good) = setup();
    let open = t.path().join("g");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o770)).unwrap();
    let bad = open.join("p");
    std::fs::create_dir(&bad).unwrap();
    let mut cfg = real_link_dir(&good);
    cfg.places = vec![(Place::Tmp, bad.clone()), (Place::Tmpdir, good.clone())];
    let p = nest(t.path(), 200, "s.sock");
    let r = bind(&p, &opts(), &cfg).unwrap();
    assert_eq!(r.base, Some("tmpdir"));
    assert!(entries(&bad).is_empty());
    assert!(b_of(&good).is_dir());
}

// A B already at the default name that is not fit to use (writable by its
// group) is left as it is, and B is made under `sockat-<uid>.<10 random>`
// instead. A B already there that is fit is used with its mode as it is
// (0700 here).
#[test]
fn an_existing_b_is_used_as_it_is_or_replaced_by_a_random_name() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let b = b_of(&place);
    std::fs::create_dir(&b).unwrap();
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o770)).unwrap();
    let p = nest(t.path(), 200, "s.sock");
    let r = bind(&p, &opts(), &cfg).unwrap();
    assert_eq!(r.route, names::LINK_DIR);
    assert!(entries(&b).is_empty(), "the unfit B was used");
    assert_eq!(std::fs::metadata(&b).unwrap().mode() & 0o777, 0o770);
    let prefix = format!("sockat-{}.", sys::effective_uid());
    let fallback: Vec<_> = entries(&place)
        .into_iter()
        .filter(|n| n.starts_with(&prefix))
        .collect();
    assert_eq!(fallback.len(), 1);
    assert_eq!(fallback[0].len(), prefix.len() + base::RANDOM_LEN);

    let (t2, place2) = setup();
    let b2 = b_of(&place2);
    std::fs::create_dir(&b2).unwrap();
    std::fs::set_permissions(&b2, std::fs::Permissions::from_mode(0o700)).unwrap();
    let p2 = nest(t2.path(), 200, "s.sock");
    let r2 = bind(&p2, &opts(), &real_link_dir(&place2)).unwrap();
    assert_eq!(r2.route, names::LINK_DIR);
    assert_eq!(std::fs::metadata(&b2).unwrap().mode() & 0o777, 0o700);
    assert_eq!(entries(&b2).len(), 1);
}

// An Options.base of the wrong form (relative, or not ending with a name) is
// an argument error whatever the length of the path, a short one included,
// like a NUL in the path: no route, nothing skipped, nothing made.
#[test]
fn an_options_base_of_the_wrong_form_is_invalid_input_at_any_length() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let short = t.path().join("s.sock");
    let long = nest(t.path(), 200, "s.sock");
    for base in [
        PathBuf::from("relative/b"),
        PathBuf::from(format!("{}/", t.path().join("b").display())),
        t.path().join(".."),
        PathBuf::from("/"),
    ] {
        let options = Options {
            base: Some(base.clone()),
            ..opts()
        };
        for p in [&short, &long] {
            for req in [BIND, CONNECT] {
                let f = route::run(req, p, &options, true, Some(&cfg))
                    .err()
                    .unwrap();
                assert_eq!(
                    f.error.kind(),
                    io::ErrorKind::InvalidInput,
                    "{base:?} {p:?}"
                );
                assert_eq!(f.route, None);
                assert!(f.skipped.is_empty());
            }
            let f = route::resolve(route::Use::Bind, p, &options, Some(&cfg))
                .err()
                .unwrap();
            assert_eq!(f.error.kind(), io::ErrorKind::InvalidInput);
        }
    }
    assert!(!short.exists());
    assert!(entries(&place).is_empty());
}

// Options.base is checked before any route, for a bind and a connect alike,
// without making anything: a missing directory on the way is ENOENT, a base
// that is neither a directory nor a symlink ENOTDIR, a symlink (not a real
// directory) or one writable by group / others EPERM, and
// a relative base or one not ending with a name an argument error. The
// failure has no route and no skipped (not even standard: no route was
// tried), and nothing is made, at the base or in another place: an
// application that names its own place never gets another one.
#[test]
fn options_base_that_fails_its_check_fails_before_any_route() {
    let (t, auto) = setup();
    let cfg = real_link_dir(&auto);
    let p = nest(t.path(), 200, "s.sock");
    let file = t.path().join("f");
    std::fs::write(&file, b"").unwrap();
    let writable = t.path().join("w");
    std::fs::create_dir(&writable).unwrap();
    std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o770)).unwrap();
    // A symlink to a directory of this user's is not a real directory.
    let real = t.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = t.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let cases: Vec<(PathBuf, Option<i32>)> = vec![
        (t.path().join("none/b"), Some(libc::ENOENT)),
        (file.clone(), Some(libc::ENOTDIR)),
        (writable.clone(), Some(libc::EPERM)),
        (link.clone(), Some(libc::EPERM)),
        (PathBuf::from("relative/b"), None),
        (t.path().join("b/"), None),
        (t.path().join(".."), None),
    ];
    for (base, want) in cases {
        let options = Options {
            base: Some(base.clone()),
            ..opts()
        };
        for req in [BIND, CONNECT] {
            let f = route::run(req, &p, &options, true, Some(&cfg))
                .err()
                .unwrap();
            match want {
                Some(want) => assert_eq!(errno(&f.error), Some(want), "{base:?}"),
                None => assert_eq!(f.error.kind(), io::ErrorKind::InvalidInput, "{base:?}"),
            }
            assert_eq!(f.route, None);
            assert!(f.skipped.is_empty());
        }
    }
    assert!(entries(&auto).is_empty());
    assert!(!t.path().join("none").exists());
    assert!(!t.path().join("b").exists());
}

// With Options.base, B is the given directory (made when missing, mode
// 0711), this process's D goes in it, the automatic place is left alone,
// and the diagnostic base is `given` whatever the form of the spelling. A
// connect reached by a route that needs no B (relative, proc-direct,
// thread-cwd-direct, ...) does not make B.
#[test]
fn options_base_is_b_and_is_made_only_when_used() {
    let (t, auto) = setup();
    let base = t.path().join("b");
    let options = Options {
        base: Some(base.clone()),
        ..opts()
    };
    let p = nest(t.path(), 200, "s.sock");
    for cfg in [no_link_file(config(&auto)), real_link_dir(&auto)] {
        let q = nest(&t.path().join(base::random_name().unwrap()), 200, "s.sock");
        let r = bind(&q, &options, &cfg).unwrap();
        assert_eq!(r.route, names::LINK_DIR);
        assert_eq!(r.base, Some("given"));
    }
    assert_eq!(std::fs::metadata(&base).unwrap().mode() & 0o7777, 0o711);
    assert_eq!(entries(&base).len(), 1);
    assert!(entries(&auto).is_empty());

    let l = listen_at(&p);
    let base2 = t.path().join("b2");
    let options2 = Options {
        base: Some(base2.clone()),
        ..opts()
    };
    let c = route::run(CONNECT, &p, &options2, false, Some(&config(&auto))).unwrap();
    if c.route != names::LINK_FILE {
        assert!(!base2.exists(), "a connect reached without B made B");
    }
    ping(&l, c.socket.unwrap());
}

// usable_cross_uid on a bind takes only a B every directory on the way to
// which, and B itself, let others search, and never the procB spelling: the
// place made 0711 here (its parents in /tmp are searchable) gives a spelling
// through B's real path, Linked with usable_cross_uid. A 0700 place does
// not: then no spelling another uid can follow is given, and the bind goes
// on to thread-cwd / at (Dangling, its D in the usual B) or fails, rather
// than give one another uid cannot follow.
#[test]
fn usable_cross_uid_takes_only_a_b_others_can_search() {
    let (t, place) = setup();
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
    std::fs::set_permissions(&place, std::fs::Permissions::from_mode(0o711)).unwrap();
    let cross = Options {
        usable_cross_uid: true,
        ..opts()
    };
    let p = nest(t.path(), 200, "s.sock");
    let r = bind(&p, &cross, &no_link_file(config(&place))).unwrap();
    assert_eq!(r.route, names::LINK_DIR);
    assert_eq!(r.base, Some("tmp"));
    assert!(matches!(
        r.spelling,
        Some(Spelling::Linked {
            until: Until::Cleanup,
            usable_cross_uid: true,
            ..
        })
    ));

    let (t2, closed) = setup();
    let q = nest(t2.path(), 200, "s.sock");
    match bind(&q, &cross, &no_link_file(config(&closed))) {
        Ok(r) => {
            assert_eq!(r.spelling, Some(Spelling::Dangling));
            let s = r
                .skipped
                .iter()
                .find(|s| s.name == names::LINK_DIR)
                .unwrap();
            assert_eq!(s.reason, names::NO_CROSS_UID_BASE);
        }
        Err(f) => assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG)),
    }
}

// usable_cross_uid with an Options.base the way to which others cannot
// search is EPERM for a bind before any route (never another place); a
// connect does not look at usable_cross_uid and connects.
#[test]
fn usable_cross_uid_with_a_base_others_cannot_search() {
    let (t, auto) = setup();
    let cfg = config(&auto);
    let options = Options {
        usable_cross_uid: true,
        base: Some(t.path().join("b")),
    };
    let p = nest(t.path(), 200, "s.sock");
    let f = bind(&p, &options, &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::EPERM));
    assert!(f.skipped.is_empty());
    assert!(!t.path().join("b").exists());
    let l = listen_at(&p);
    let c = route::run(CONNECT, &p, &options, false, Some(&cfg)).unwrap();
    ping(&l, c.socket.unwrap());
}

// ---------------------------------------------------------------------------
// B: which one the process uses
// ---------------------------------------------------------------------------

/// `c` with the places 1-3 `places`, in order.
fn with_places(c: Config, places: &[(Place, &Path)]) -> Config {
    Config {
        places: places.iter().map(|(k, p)| (*k, p.to_path_buf())).collect(),
        ..c
    }
}

/// A file name one byte too long for link-dir through B's real path
/// `<real path of place>/sockat-<uid>` (`B/D/E/<name>` with a one-letter E).
fn name_just_past(place: &Path) -> String {
    let b = path::len(&b_of(place));
    "n".repeat(DIRECT_MAX + 1 - (b + 1 + base::RANDOM_LEN + 1 + 1 + 1))
}

/// The skipped of a call, whether it reached the path or not.
fn skipped_of(r: &Result<Reached, route::Failed>) -> Vec<(&'static str, &'static str)> {
    let s = match r {
        Ok(r) => &r.skipped,
        Err(f) => &f.skipped,
    };
    s.iter().map(|s| (s.name, s.reason)).collect()
}

// The process's B is the first place in the order where B can be made,
// whatever the path's length. When the spelling through that place is known
// not to fit (from the place's real path), B is not made there and the bind
// does not move on to a later place where it would fit: the routes through B
// are passed over (link-dir, too long), and the later place is left empty.
// Otherwise the process's B would depend on the length of the path of each
// call (hub SPEC-routes "実体の置き場").
#[test]
fn a_spelling_too_long_through_the_first_place_does_not_move_to_the_next() {
    let t = tmp();
    let first = t.path().join("l".repeat(40));
    let later = t.path().join("p");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&later).unwrap();
    let cfg = with_places(
        no_link_file(no_proc(config(&first))),
        &[(Place::Tmp, &first), (Place::Tmpdir, &later)],
    );
    let p = nest(t.path(), 250, &name_just_past(&first));
    let r = bind(&p, &opts(), &cfg);
    assert!(
        skipped_of(&r).contains(&(names::LINK_DIR, names::TOO_LONG)),
        "{:?}",
        r.as_ref().map(|r| (r.route, r.base))
    );
    assert!(entries(&later).is_empty(), "B made in the later place");
}

// A B the process holds (the places 1-3) is the B for the rest of the
// process: a later call whose spelling would fit only through a later place
// still goes through it (and passes over the routes through B, too long),
// and nothing is made in the later place. A place where B could not be made
// is not remembered: once B can be made there, the next call makes it.
#[test]
fn a_b_held_is_kept_and_a_place_that_failed_is_tried_again() {
    let t = tmp();
    let first = t.path().join("l".repeat(40));
    let later = t.path().join("p");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&later).unwrap();
    let cfg = with_places(
        no_link_file(no_proc(config(&first))),
        &[(Place::Tmp, &first), (Place::Tmpdir, &later)],
    );
    let r = bind(&nest(&t.path().join("a"), 200, "s"), &opts(), &cfg).unwrap();
    assert_eq!(r.base, Some("tmp"));
    let r = bind(
        &nest(&t.path().join("b"), 250, &name_just_past(&first)),
        &opts(),
        &cfg,
    );
    assert!(skipped_of(&r).contains(&(names::LINK_DIR, names::TOO_LONG)));
    assert!(entries(&later).is_empty(), "B made in the later place");

    let (t2, missing) = setup();
    std::fs::remove_dir(&missing).unwrap();
    let cfg2 = no_thread(real_link_dir(&missing));
    let _ = bind(&nest(&t2.path().join("a"), 200, "s.sock"), &opts(), &cfg2);
    std::fs::create_dir(&missing).unwrap();
    let r = bind(&nest(&t2.path().join("b"), 200, "s.sock"), &opts(), &cfg2).unwrap();
    assert_eq!(r.base, Some("tmp"));
}

// The usual B and the B for usable_cross_uid are held apart: once the usual
// B is in a later place (the first could not be made then), a usable_cross_uid
// bind that makes its B in the first place does not change the usual B, and
// the next plain bind still goes through the later place (one process uses a
// second B of the places 1-3 only for usable_cross_uid).
#[test]
fn the_usual_b_and_the_b_for_another_uid_are_held_apart() {
    let t = tmp();
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
    let first = t.path().join("p1");
    let closed = t.path().join("q");
    std::fs::create_dir(&closed).unwrap();
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).unwrap();
    let later = closed.join("p2");
    std::fs::create_dir(&later).unwrap();
    let cfg = with_places(
        no_link_file(no_proc(config(&first))),
        &[(Place::Tmp, &first), (Place::Tmpdir, &later)],
    );
    let cross = Options {
        usable_cross_uid: true,
        ..opts()
    };
    let r = bind(&nest(&t.path().join("a"), 200, "s"), &opts(), &cfg).unwrap();
    assert_eq!(r.base, Some("tmpdir"));
    std::fs::create_dir(&first).unwrap();
    std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o711)).unwrap();
    let r = bind(&nest(&t.path().join("b"), 200, "s"), &cross, &cfg).unwrap();
    assert_eq!(r.base, Some("tmp"));
    assert!(matches!(
        r.spelling,
        Some(Spelling::Linked {
            usable_cross_uid: true,
            ..
        })
    ));
    let r = bind(&nest(&t.path().join("c"), 200, "s"), &opts(), &cfg).unwrap();
    assert_eq!(r.base, Some("tmpdir"));
}

// Whether another uid can search the way to a B (the directories on the way
// and B itself) is judged when the process first takes B, and kept: a B
// that was 0700 then is not taken for usable_cross_uid after it is made 0711
// (link-dir passes it over, no second D is made in it), and a B that was 0711
// then still gives a spelling marked usable_cross_uid after it is made 0700.
// The same holds for an Options.base: one taken while 0700 is EPERM for
// usable_cross_uid before any route even once it is 0711 (hub SPEC-routes
// "実体の置き場").
#[test]
fn usable_cross_uid_is_judged_when_b_is_taken() {
    let t = tmp();
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
    let cross = Options {
        usable_cross_uid: true,
        ..opts()
    };
    let mode = |p: &Path, m: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    };

    let shut = t.path().join("shut");
    std::fs::create_dir(&shut).unwrap();
    mode(&shut, 0o711);
    let b = b_of(&shut);
    std::fs::create_dir(&b).unwrap();
    mode(&b, 0o700);
    let cfg = no_link_file(no_proc(config(&shut)));
    let r = bind(&nest(&t.path().join("a"), 200, "s"), &opts(), &cfg).unwrap();
    assert_eq!(r.base, Some("tmp"));
    mode(&b, 0o711);
    let r = bind(&nest(&t.path().join("b"), 200, "s"), &cross, &cfg);
    assert!(
        skipped_of(&r).contains(&(names::LINK_DIR, names::NO_CROSS_UID_BASE)),
        "{:?}",
        r.as_ref().map(|r| (r.route, r.spelling))
    );
    assert_eq!(entries(&b).len(), 1, "a second D in B");

    let open = t.path().join("open");
    std::fs::create_dir(&open).unwrap();
    mode(&open, 0o711);
    let cfg = no_link_file(no_proc(config(&open)));
    bind(&nest(&t.path().join("c"), 200, "s"), &opts(), &cfg).unwrap();
    mode(&b_of(&open), 0o700);
    let r = bind(&nest(&t.path().join("d"), 200, "s"), &cross, &cfg).unwrap();
    assert!(matches!(
        r.spelling,
        Some(Spelling::Linked {
            usable_cross_uid: true,
            ..
        })
    ));

    let given = t.path().join("given");
    std::fs::create_dir(&given).unwrap();
    mode(&given, 0o700);
    let at = |cross: bool| Options {
        usable_cross_uid: cross,
        base: Some(given.clone()),
    };
    let r = bind(&nest(&t.path().join("e"), 200, "s"), &at(false), &cfg).unwrap();
    assert_eq!(r.base, Some("given"));
    mode(&given, 0o711);
    let f = bind(&nest(&t.path().join("f"), 200, "s"), &at(true), &cfg)
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::EPERM));
    assert_eq!(f.route, None);
    assert!(f.skipped.is_empty());
    assert_eq!(entries(&given).len(), 1, "a second D in the given B");

    let made = t.path().join("made");
    let at_made = |cross: bool| Options {
        usable_cross_uid: cross,
        base: Some(made.clone()),
    };
    bind(&nest(&t.path().join("g"), 200, "s"), &at_made(false), &cfg).unwrap();
    mode(&made, 0o700);
    let r = bind(&nest(&t.path().join("h"), 200, "s"), &at_made(true), &cfg).unwrap();
    assert_eq!(r.base, Some("given"));
    assert!(matches!(
        r.spelling,
        Some(Spelling::Linked {
            usable_cross_uid: true,
            ..
        })
    ));
}

// When D cannot be made ready in B (made, but its mode cannot be set or it
// cannot be opened), the empty D is removed again, the routes that use D
// cannot be used in that call (link-dir, link-file, thread-cwd / at: each "D
// not made"), D is not tried again for each of them, and no other B is
// looked for (no B in the later place, none in the socket's directory): the
// process's B stays what it is (hub SPEC-routes "D の作り方" 2). The open is
// made to fail by the test hook (no real failure there is open to a test).
#[test]
fn a_d_that_cannot_be_made_is_not_tried_again_nor_another_b_looked_for() {
    let t = tmp();
    let first = t.path().join("p1");
    let later = t.path().join("p2");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&later).unwrap();
    let cfg = with_places(
        no_proc(config(&first)),
        &[(Place::Tmp, &first), (Place::Tmpdir, &later)],
    );
    let p = nest(&t.path().join("s"), 200, "s.sock");
    base::FAIL_D.set(Some(0));
    let r = bind(&p, &opts(), &cfg);
    let tries = base::FAIL_D.replace(None);
    let f = r.err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG));
    assert!(
        f.skipped
            .iter()
            .any(|s| s.name == names::LINK_DIR && s.reason == names::NO_D),
        "{:?}",
        f.skipped
    );
    for s in &f.skipped {
        if [names::LINK_FILE, names::THREAD_CWD, names::AT].contains(&s.name) {
            assert!(
                [names::NO_D, names::SECCOMP, names::NOT_FOLLOWED].contains(&s.reason),
                "{s:?}"
            );
        }
    }
    assert_eq!(tries, Some(1), "D tried more than once in one call");
    assert!(entries(&later).is_empty());
    assert!(b_of(&first).is_dir());
    assert!(entries(&b_of(&first)).is_empty(), "the empty D left in B");
    assert!(
        entries(p.parent().unwrap())
            .iter()
            .all(|n| !n.starts_with(base::SOCKDIR_PREFIX))
    );
}

/// A directory where `mkdir` fails for everyone (root included) although
/// the check of `Options.base` passes (owned by root, nobody can write to
/// it): `/proc` on Linux, `/dev` elsewhere.
fn unmakeable_parent() -> &'static Path {
    Path::new(if cfg!(any(target_os = "linux", target_os = "android")) {
        "/proc"
    } else {
        "/dev"
    })
}

// An Options.base that passes its check but cannot be made (mkdir fails) is
// that error, with no route (no route made the call) and the routes passed
// over before it in skipped, for a bind, a connect (whose link-file is the
// route that needs B) and resolve_for_bind; nothing is made in another place.
#[test]
fn an_options_base_that_cannot_be_made_after_its_check_is_that_error() {
    let (t, auto) = setup();
    let given = unmakeable_parent().join(format!("sockat-test-{}", base::random_name().unwrap()));
    let want = std::fs::create_dir(&given).unwrap_err().raw_os_error();
    let options = Options {
        base: Some(given.clone()),
        ..opts()
    };
    let p = nest(t.path(), 200, "s.sock");
    let f = bind(&p, &options, &real_link_dir(&auto)).err().unwrap();
    assert_eq!(errno(&f.error), want);
    assert_eq!(f.route, None);
    assert_eq!(skipped_names(&f.skipped), vec![names::STANDARD]);

    let f = route::resolve(route::Use::Bind, &p, &options, Some(&real_link_dir(&auto)))
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), want);
    assert_eq!(f.route, None);
    assert_eq!(skipped_names(&f.skipped), vec![names::STANDARD]);

    let _l = listen_at(&p);
    let cfg = no_thread(no_direct(no_proc(config(&auto))));
    let f = route::run(CONNECT, &p, &options, false, Some(&cfg))
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), want);
    assert_eq!(f.route, None);
    assert_eq!(f.skipped.first().map(|s| s.name), Some(names::STANDARD));
    assert!(!skipped_names(&f.skipped).contains(&names::LINK_FILE));
    assert!(entries(&auto).is_empty());
}

// The B in the socket's directory (made when no place 1-3 can be) is used
// only through the procB spelling: its real path is the socket's directory
// and more, so the spelling through it reaches a socket file only as far as
// the path does, and a B made in the user's own directory for a spelling is
// what the procB form is for. Here /proc is stood in for by a directory with
// `self/fd` (so /proc looks usable) where the procB form of B names nothing,
// and the socket's directory is reached by a long symlink to a short real
// one (so B's real path would fit): the bind takes no Linked spelling
// through that B (thread-cwd / at with Dangling, or ENAMETOOLONG).
#[test]
fn a_b_in_the_socket_directory_is_spelled_only_through_proc() {
    let t = tmp();
    let real = t.path().join("x");
    std::fs::create_dir(&real).unwrap();
    let long = t.path().join("l".repeat(150));
    std::os::unix::fs::symlink(&real, &long).unwrap();
    let fake = t.path().join("proc");
    std::fs::create_dir_all(fake.join("self/fd")).unwrap();
    let mut cfg = config(&t.path().join("none"));
    cfg.proc = fake;
    let p = long.join("s.sock");
    assert!(path::len(&p) > DIRECT_MAX);
    match bind(&p, &opts(), &cfg) {
        Ok(r) => {
            assert_eq!(
                (r.route, r.base, r.spelling),
                (
                    if cfg!(target_os = "freebsd") {
                        names::AT
                    } else {
                        names::THREAD_CWD
                    },
                    None,
                    Some(Spelling::Dangling)
                )
            );
            assert!(is_socket(&real.join("s.sock")));
        }
        Err(f) => assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG)),
    }
}

// A B or D the process holds that has been removed (by the OS's clean-up of
// temporary directories, or by hand) is let go and made again by the next
// call: after D is removed the next bind makes a new D in the same B, and
// after B is removed (with its D) the next bind makes B again at the same
// place. Both binds reach the path through link-dir.
#[test]
fn a_b_or_d_removed_is_made_again_by_the_next_call() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let b = b_of(&place);
    bind(&nest(&t.path().join("a"), 200, "s.sock"), &opts(), &cfg).unwrap();
    let d = entries(&b);
    std::fs::remove_dir_all(b.join(&d[0])).unwrap();
    let p = nest(&t.path().join("b"), 200, "s.sock");
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    assert_eq!(r.route, names::LINK_DIR);
    let now = entries(&b);
    assert_eq!(now.len(), 1);
    assert_ne!(now, d);
    ping(
        &listener(&mut r),
        connect(&p, &cfg).unwrap().socket.unwrap(),
    );

    std::fs::remove_dir_all(&b).unwrap();
    let p = nest(&t.path().join("c"), 200, "s.sock");
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    assert_eq!((r.route, r.base), (names::LINK_DIR, Some("tmp")));
    assert_eq!(entries(&b).len(), 1);
    ping(
        &listener(&mut r),
        connect(&p, &cfg).unwrap().socket.unwrap(),
    );
}

// A directory held by an fd counts as removed when the fd shows no links,
// even when its name now gives the same (st_dev, st_ino): a file system may
// give the number of a removed directory to a new one at the same name, and
// then only the link count tells. Here the reuse is stood in for by passing
// the new directory's number as the one held. Where a removed directory
// keeps its link count (macOS's APFS) there is nothing to tell this way, and
// the test says so.
#[test]
fn a_directory_held_counts_as_removed_when_it_has_no_links() {
    let t = tmp();
    let old = t.path().join("old");
    std::fs::create_dir(&old).unwrap();
    let held = std::fs::File::open(&old).unwrap();
    std::fs::remove_dir(&old).unwrap();
    if held.metadata().unwrap().nlink() != 0 {
        eprintln!("a removed directory keeps its link count here");
        return;
    }
    std::fs::create_dir(&old).unwrap();
    let m = std::fs::symlink_metadata(&old).unwrap();
    let id = (m.dev(), m.ino());
    assert!(!base::still_named(held.as_fd(), id, Ok(id)));
    let live = std::fs::File::open(&old).unwrap();
    assert!(base::still_named(live.as_fd(), id, Ok(id)));
}

// "Myself" is the effective uid: B's name is `sockat-<effective uid>`, and B
// is taken as this user's when its owner is the effective uid. Run in a
// child that root turns into effective uid 1000 (the real uid stays 0); its
// B is `sockat-1000`, owned by 1000. Only root can run it.
#[test]
fn the_uid_is_the_effective_one() {
    if sys::effective_uid() != 0 {
        eprintln!("only root can change its effective uid");
        return;
    }
    let t = tmp();
    let place = t.path().join("p");
    std::fs::create_dir(&place).unwrap();
    let p = nest(t.path(), 200, "s.sock");
    let mut dir = p.parent().unwrap();
    while dir != Path::new("/tmp") {
        std::os::unix::fs::chown(dir, Some(1000), Some(1000)).unwrap();
        dir = dir.parent().unwrap();
    }
    std::os::unix::fs::chown(&place, Some(1000), Some(1000)).unwrap();
    run_child(
        "tests::child_effective_uid",
        &[
            ("SOCKAT_CHILD_PATH", p.as_os_str()),
            ("SOCKAT_CHILD_PLACE", place.as_os_str()),
        ],
    );
    let b = std::fs::canonicalize(&place).unwrap().join("sockat-1000");
    assert_eq!(std::fs::metadata(&b).unwrap().uid(), 1000);
    assert_eq!(entries(&place), vec!["sockat-1000"]);
}

#[test]
fn child_effective_uid() {
    if !in_child() {
        return;
    }
    sys::set_effective_uid(1000).unwrap();
    assert_eq!(sys::real_uid(), 0);
    let p = env_path("SOCKAT_CHILD_PATH");
    let r = bind(&p, &opts(), &real_link_dir(&env_path("SOCKAT_CHILD_PLACE"))).unwrap();
    assert_eq!((r.route, r.base), (names::LINK_DIR, Some("tmp")));
}

// skipped names only the routes of this OS's order (hub SPEC "結果"): a route
// the OS has no means for (thread-cwd on NetBSD / OpenBSD / illumos, the
// bind's link-file where a bind does not follow a symlink, proc-direct
// without /proc) is not in it. With every route unusable here (no place for
// B, no /proc, no thread-cwd, the socket's directory unopenable, a path too
// far from the cwd for relative), a bind and a connect fail with
// ENAMETOOLONG after passing over exactly the routes of this OS's row.
#[test]
fn skipped_holds_only_the_routes_of_this_os() {
    let t = tmp();
    let cfg = no_thread(no_direct(no_proc(config(&t.path().join("none")))));
    let p = nest(t.path(), 200, "s.sock");
    let r = bind(&p, &opts(), &cfg);
    let bind_row: &[&str] = if cfg!(any(target_os = "linux", target_os = "android")) {
        &[names::STANDARD, names::LINK_DIR, names::THREAD_CWD]
    } else if cfg!(target_vendor = "apple") {
        &[
            names::STANDARD,
            names::LINK_DIR,
            names::LINK_FILE,
            names::THREAD_CWD,
        ]
    } else if cfg!(target_os = "freebsd") {
        &[names::STANDARD, names::LINK_DIR, names::AT]
    } else if cfg!(any(target_os = "openbsd", target_os = "illumos")) {
        &[names::STANDARD, names::LINK_DIR]
    } else {
        &[names::STANDARD, names::LINK_DIR, names::LINK_FILE]
    };
    match r {
        // FreeBSD's at works in a B in the socket's directory, which can be
        // made: the routes before it are passed over.
        Ok(r) => {
            assert_eq!(r.route, names::AT);
            assert_eq!(skipped_names(&r.skipped), &bind_row[..2]);
            std::fs::remove_file(&p).unwrap();
        }
        Err(f) => {
            assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG));
            assert_eq!(skipped_names(&f.skipped), bind_row);
        }
    }

    let _l = listen_at(&p);
    let f = connect(&p, &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG));
    let connect_row: &[&str] = if cfg!(any(target_os = "linux", target_os = "android")) {
        &[
            names::STANDARD,
            names::RELATIVE,
            names::THREAD_CWD_DIRECT,
            names::LINK_FILE,
        ]
    } else if cfg!(target_vendor = "apple") {
        &[
            names::STANDARD,
            names::RELATIVE,
            names::WIDENED,
            names::THREAD_CWD_DIRECT,
            names::LINK_FILE,
        ]
    } else if cfg!(target_os = "freebsd") {
        &[names::STANDARD, names::AT_DIRECT, names::LINK_FILE]
    } else {
        &[names::STANDARD, names::RELATIVE, names::LINK_FILE]
    };
    assert_eq!(skipped_names(&f.skipped), connect_row);
}

// ForBind's socket_file follows the spelling after the bind and fails when
// what it reaches is not a socket (ENOTSOCK): a file put at the path instead
// of the socket is not taken, so its remove cannot remove that file.
#[test]
fn socket_file_of_what_is_not_a_socket_is_an_error() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    let r = route::resolve(route::Use::Bind, &p, &opts(), Some(&cfg)).unwrap();
    std::fs::write(&p, b"not a socket").unwrap();
    let e = route::remember_resolved(&r.path, r.cwd).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOTSOCK));
    assert_eq!(std::fs::read(&p).unwrap(), b"not a socket");
}

// ---------------------------------------------------------------------------
// sweep
// ---------------------------------------------------------------------------

// sweep removes the D of a process that ended (no lock held on it), with its
// E, and leaves: a live process's D (a child that binds and waits), this
// process's own D, a D holding something but symlinks (not one sockat left),
// an E directly in B (a connect's), and B itself.
#[test]
fn sweep_removes_only_dead_processes_d() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let b = b_of(&place);
    bind_in_child(&nest(&t.path().join("dead"), 200, "s.sock"), &place);
    let live = nest(&t.path().join("live"), 200, "s.sock");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::child_bind_and_wait",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SOCKAT_CHILD", "1")
        .env("SOCKAT_CHILD_PATH", &live)
        .env("SOCKAT_CHILD_PLACE", &place)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while !seen.ends_with(b"READY\n") {
        assert_eq!(out.read(&mut byte).unwrap(), 1, "child ended early");
        seen.push(byte[0]);
    }
    let _own = bind(&nest(&t.path().join("own"), 200, "s.sock"), &opts(), &cfg).unwrap();
    let foreign = b.join("notsockatD");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("file"), b"").unwrap();
    std::os::unix::fs::symlink("/", b.join("connectE00")).unwrap();
    assert_eq!(entries(&b).len(), 5);

    base::sweep(&cfg.places).unwrap();
    assert_eq!(entries(&b).len(), 4, "{:?}", in_b(&b));
    assert!(foreign.join("file").exists());
    assert!(b.join("connectE00").is_symlink());
    drop(child.stdin.take());
    child.wait().unwrap();
    base::sweep(&cfg.places).unwrap();
    assert_eq!(entries(&b).len(), 3, "{:?}", in_b(&b));
}

#[test]
fn child_bind_and_wait() {
    if !in_child() {
        return;
    }
    let p = env_path("SOCKAT_CHILD_PATH");
    let cfg = real_link_dir(&env_path("SOCKAT_CHILD_PLACE"));
    let _r = bind(&p, &opts(), &cfg).unwrap();
    println!("READY");
    io::stdout().flush().unwrap();
    let mut rest = Vec::new();
    io::stdin().read_to_end(&mut rest).unwrap();
}

// sweep_base does the same in an Options.base, and fails (making nothing)
// for one that fails its check.
#[test]
fn sweep_base_sweeps_an_options_base() {
    let t = tmp();
    let base = t.path().join("b");
    std::fs::create_dir(&base).unwrap();
    let dead_d = base.join("deadDDDDDD");
    std::fs::create_dir(&dead_d).unwrap();
    std::os::unix::fs::symlink("/", dead_d.join("0")).unwrap();
    base::sweep_base(&base).unwrap();
    assert!(entries(&base).is_empty());
    let e = base::sweep_base(&t.path().join("none/b")).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOENT));
    assert!(!t.path().join("none").exists());
}

// ---------------------------------------------------------------------------
// connect
// ---------------------------------------------------------------------------

// The connect's link-file (the routes before it unusable): E is made
// directly in B and removed right after the connect, so B holds nothing
// afterwards and no D is made; the connection works. The connect's result
// has no base and no spelling.
#[test]
fn a_connect_through_link_file_leaves_nothing() {
    let (t, place) = setup();
    let cfg = no_thread(no_direct(no_proc(config(&place))));
    let p = nest(t.path(), 300, &"n".repeat(100));
    let l = listen_at(&p);
    let c = connect(&p, &cfg).unwrap();
    assert_eq!(c.route, names::LINK_FILE);
    assert_eq!(c.base, None);
    assert_eq!(c.spelling, None);
    assert!(b_of(&place).is_dir());
    assert!(entries(&b_of(&place)).is_empty());
    ping(&l, c.socket.unwrap());
}

// thread-cwd-direct (Linux, Apple): when the routes that need no writing
// cannot be used, a thread of its own moves its working directory into the
// socket's directory and connects by the file name; nothing is made.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn thread_cwd_direct_connects_by_the_file_name() {
    if !thread_here() {
        eprintln!("thread-cwd cannot be used here");
        return;
    }
    let (t, place) = setup();
    #[allow(unused_mut)]
    let mut cfg = no_proc(config(&place));
    #[cfg(target_vendor = "apple")]
    {
        cfg.wide = false;
    }
    let p = nest(t.path(), 300, "s.sock");
    let l = listen_at(&p);
    let c = connect(&p, &cfg).unwrap();
    assert_eq!(c.route, names::THREAD_CWD_DIRECT);
    assert!(!b_of(&place).exists());
    ping(&l, c.socket.unwrap());
}

// proc-direct (Linux with /proc) comes first for a connect:
// `/proc/self/fd/<the socket's directory's fd>/<name>`, nothing written.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn proc_direct_comes_first_on_linux() {
    if !proc_here() {
        return;
    }
    let (t, place) = setup();
    let p = nest(t.path(), 300, "s.sock");
    let l = listen_at(&p);
    let c = connect(&p, &config(&place)).unwrap();
    assert_eq!(c.route, names::PROC_DIRECT);
    assert!(!b_of(&place).exists());
    ping(&l, c.socket.unwrap());
}

// widened (Apple): a connect to a path of up to 253 bytes far from the cwd
// goes in a widened sockaddr, nothing written. A bind never uses it: the
// address a bound socket reports fits sun_path.
#[cfg(target_vendor = "apple")]
#[test]
fn widened_connects_up_to_253_bytes_on_apple() {
    let (t, place) = setup();
    let cfg = config(&place);
    let p = nest(t.path(), 253, "s.sock");
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    assert_ne!(r.route, names::WIDENED);
    let len = sys::local_address_len(r.socket.as_ref().unwrap().as_fd()).unwrap();
    assert!(len <= 2 + DIRECT_MAX + 1, "{len}");
    let l = listener(&mut r);
    let c = connect(&p, &cfg).unwrap();
    assert_eq!(c.route, names::WIDENED);
    ping(&l, c.socket.unwrap());
}

// A connect's real failure is the result, and later routes are not tried
// (they would only fail the same way, after writing): ECONNREFUSED for a
// socket nobody listens on, ENOENT for nothing at the path. Nothing is left
// in B.
#[test]
fn a_connect_failure_is_not_retried() {
    let (t, place) = setup();
    let other = t.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    bind_in_child(&p, &other);
    let f = connect(&p, &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::ECONNREFUSED));
    assert!(f.route.is_some());
    let f = connect(&p.with_file_name("none.sock"), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENOENT));
    assert!(entries(&b_of(&place)).is_empty());
}

// A nonblocking connect does not wait: on a full backlog it fails at once
// (EAGAIN on Linux, ECONNREFUSED on macOS), and a stream it returns is
// nonblocking.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
#[test]
fn a_nonblocking_connect_does_not_wait() {
    let (t, place) = setup();
    let cfg = config(&place);
    let p = nest(t.path(), 200, "s.sock");
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    let l = listener(&mut r);
    sys::relisten(l.as_fd(), 1).unwrap();
    let nb = Request::Connect(Connect::Stream { nonblocking: true });
    let mut held = Vec::new();
    let full = loop {
        match route::run(nb, &p, &opts(), false, Some(&cfg)) {
            Ok(c) => {
                let s = c.socket.unwrap();
                assert!(sys::is_nonblocking(s.as_fd()).unwrap());
                held.push(s);
                assert!(held.len() < 16, "the backlog never filled");
            }
            Err(f) => break f.error,
        }
    };
    #[cfg(any(target_os = "linux", target_os = "android"))]
    assert_eq!(errno(&full), Some(libc::EAGAIN));
    #[cfg(target_vendor = "apple")]
    assert_eq!(errno(&full), Some(libc::ECONNREFUSED));
}

// ---------------------------------------------------------------------------
// arguments
// ---------------------------------------------------------------------------

// A long path that does not end with a file name, and a path of any length
// with a NUL byte, are InvalidInput before anything is looked up or made: no
// route, nothing skipped, no B.
#[test]
fn paths_without_a_file_name_or_with_nul_are_invalid_input() {
    let (t, place) = setup();
    let cfg = config(&place);
    let file = nest(t.path(), 200, "x");
    let dir = file.parent().unwrap();
    let mut bad: Vec<PathBuf> = vec![
        PathBuf::from(format!("{}/", dir.display())),
        dir.join("."),
        dir.join(".."),
        PathBuf::from(OsStr::from_bytes(b"s\0.sock")),
    ];
    let mut long = dir.as_os_str().as_bytes().to_vec();
    long.extend_from_slice(b"/s\0.sock");
    bad.push(PathBuf::from(OsStr::from_bytes(&long)));
    for p in bad {
        for req in [BIND, CONNECT] {
            let f = route::run(req, &p, &opts(), true, Some(&cfg))
                .err()
                .unwrap();
            assert_eq!(f.error.kind(), io::ErrorKind::InvalidInput, "{p:?}");
            assert_eq!(f.route, None);
            assert!(f.skipped.is_empty());
        }
    }
    assert!(entries(&place).is_empty());
}

// The socket's directory is looked up first: a missing one is ENOENT and a
// file in its place ENOTDIR, before any route (none named, none skipped).
#[test]
fn a_missing_directory_is_enoent_before_any_route() {
    let (t, place) = setup();
    let cfg = config(&place);
    let deep = nest(t.path(), 200, "x");
    let missing = deep.parent().unwrap().join("none/s.sock");
    let f = bind(&missing, &opts(), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENOENT));
    assert!(f.skipped.is_empty() && f.route.is_none());
    std::fs::write(&deep, b"").unwrap();
    let f = bind(&deep.join("s.sock"), &opts(), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENOTDIR));
    assert!(f.skipped.is_empty() && f.route.is_none());
    assert!(entries(&place).is_empty());
}

// ---------------------------------------------------------------------------
// datagram
// ---------------------------------------------------------------------------

// Datagram sockets take the same routes: a receiver bound at a long path, a
// sender bound at a long path of its own and connected to the receiver by its
// long path. The receiver replies to the sender's reported address (its
// kept spelling), and the reply arrives: the spelling keeps naming the
// sender's socket file. A datagram connect leaves nothing and returns no new
// socket.
#[test]
fn datagram_bind_connect_and_reply() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let server_path = nest(&t.path().join("s"), 200, "server.sock");
    let client_path = nest(&t.path().join("c"), 200, "client.sock");
    let dgram = Request::Bind(SockType::Datagram);
    let r = route::run(dgram, &server_path, &opts(), false, Some(&cfg)).unwrap();
    assert_eq!(r.route, names::LINK_DIR);
    let server = UnixDatagram::from(r.socket.unwrap());
    let r = route::run(dgram, &client_path, &opts(), false, Some(&cfg)).unwrap();
    let client = UnixDatagram::from(r.socket.unwrap());
    let req = Request::Connect(Connect::Datagram(&client));
    let c = route::run(req, &server_path, &opts(), false, Some(&cfg)).unwrap();
    assert!(c.socket.is_none());
    client.send(b"ping").unwrap();
    let mut buf = [0u8; 4];
    let (n, from) = server.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ping");
    server
        .send_to(b"pong", from.as_pathname().unwrap())
        .unwrap();
    let n = client.recv(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"pong");
}

// ---------------------------------------------------------------------------
// resolve
// ---------------------------------------------------------------------------

// resolve_for_bind's spelling is handed to std's bind, which then makes the
// socket at the path; the socket file is taken after the bind (an error
// before it, with nothing to follow) and its removal removes the file and E.
// resolve_for_connect's spelling (link-file, no base) serves many of std's
// connects, then its cleanup removes E and the spelling reaches nothing.
#[test]
fn resolved_spellings_are_used_by_std() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    let r = route::resolve(route::Use::Bind, &p, &opts(), Some(&cfg)).unwrap();
    assert_eq!(r.route, names::LINK_DIR);
    assert!(route::remember_resolved(&r.path, r.cwd).is_err());
    let l = UnixListener::bind(&r.path).unwrap();
    assert!(is_socket(&p));
    let file = SocketFile::new(
        route::remember_resolved(&r.path, r.cwd).unwrap(),
        r.link.clone(),
    );

    let rc = route::resolve(route::Use::Connect, &p, &opts(), Some(&cfg)).unwrap();
    assert_eq!(rc.route, names::LINK_FILE);
    assert_eq!(rc.base, None);
    for _ in 0..3 {
        ping(&l, UnixStream::connect(&rc.path).unwrap().into());
    }
    rc.link.as_ref().unwrap().cleanup().unwrap();
    assert!(UnixStream::connect(&rc.path).is_err());

    file.remove().unwrap();
    assert!(!p.exists());
    assert_no_e(&b_of(&place));
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

// SocketFile::remove removes the file the bind made and E, once; it does not
// close the listener; a later remove does nothing even when a new socket is
// at the path by then; and a file put in the bound file's place while the
// listener is open is left alone (still Ok).
#[test]
fn remove_removes_the_bound_file_once_and_not_a_replacement() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    let l = listener(&mut r);
    let file = SocketFile::new(r.file.take().unwrap(), r.link.clone());
    file.remove().unwrap();
    assert!(!p.exists());
    assert_no_e(&b_of(&place));
    l.set_nonblocking(true).unwrap();
    assert_eq!(l.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    let _again = listen_at(&p);
    file.remove().unwrap();
    assert!(is_socket(&p));

    let q = nest(&t.path().join("q"), 200, "s.sock");
    let mut r = bind(&q, &opts(), &cfg).unwrap();
    let _l = listener(&mut r);
    let file = SocketFile::new(r.file.take().unwrap(), r.link.clone());
    std::fs::remove_file(&q).unwrap();
    std::fs::write(&q, b"someone else's").unwrap();
    file.remove().unwrap();
    assert_eq!(std::fs::read(&q).unwrap(), b"someone else's");
}

// A relative path bound with the socket file taken, while the cwd changes
// between the bind and the look-up of the file, would take a file of the
// same name in another directory: the cwd is compared around both, and a
// change fails the call (the socket closed, the file left). Run in a child:
// the cwd is process-wide.
#[test]
fn a_relative_bind_ex_fails_when_the_cwd_changes_in_between() {
    let t = tmp();
    std::fs::create_dir(t.path().join("a")).unwrap();
    std::fs::create_dir(t.path().join("b")).unwrap();
    // A socket of the same name in the other directory: what the look-up
    // would take if the change went unseen.
    let _other = UnixListener::bind(t.path().join("b/s.sock")).unwrap();
    run_child(
        "tests::child_cwd_changes",
        &[("SOCKAT_CHILD_PATH", t.path().as_os_str())],
    );
    assert!(is_socket(&t.path().join("a/s.sock")));
}

#[test]
fn child_cwd_changes() {
    if !in_child() {
        return;
    }
    let root = env_path("SOCKAT_CHILD_PATH");
    std::env::set_current_dir(root.join("a")).unwrap();
    let mut cfg = config(&root);
    cfg.between_cwd_lookups = || {
        std::env::set_current_dir(env_path("SOCKAT_CHILD_PATH").join("b")).unwrap();
    };
    let f = route::run(BIND, Path::new("s.sock"), &opts(), true, Some(&cfg))
        .err()
        .unwrap();
    assert_eq!(f.error.kind(), io::ErrorKind::Other);
    assert_eq!(f.route, Some(names::STANDARD));
}

// When bind_ex cannot remember the socket file (here: a long relative path,
// the cwd changed between the bind and the look-up), the listener is closed,
// the E of the spelling (link-dir) is removed, and the socket file is left
// where it is: it could not be told to be the one bound, so removing it might
// remove what replaced it (hub SPEC "SocketFile"). Run in a child: the cwd is
// process-wide.
#[test]
fn a_bind_ex_that_cannot_remember_closes_cleans_e_and_keeps_the_file() {
    let t = tmp();
    std::fs::create_dir(t.path().join("p")).unwrap();
    std::fs::create_dir(t.path().join("b")).unwrap();
    std::fs::create_dir_all(t.path().join("a").join(long_relative().parent().unwrap())).unwrap();
    run_child(
        "tests::child_cwd_changes_through_link_dir",
        &[("SOCKAT_CHILD_PATH", t.path().as_os_str())],
    );
    let file = t.path().join("a").join(long_relative());
    assert!(is_socket(&file));
    let b = b_of(&t.path().join("p"));
    assert_eq!(entries(&b).len(), 1);
    assert_no_e(&b);
}

/// A relative path longer than std accepts.
fn long_relative() -> PathBuf {
    let p = PathBuf::from("d".repeat(60))
        .join("e".repeat(60))
        .join("f".repeat(60))
        .join("s.sock");
    assert!(path::len(&p) > DIRECT_MAX);
    p
}

#[test]
fn child_cwd_changes_through_link_dir() {
    if !in_child() {
        return;
    }
    let root = env_path("SOCKAT_CHILD_PATH");
    std::env::set_current_dir(root.join("a")).unwrap();
    let mut cfg = real_link_dir(&root.join("p"));
    cfg.between_cwd_lookups = || {
        std::env::set_current_dir(env_path("SOCKAT_CHILD_PATH").join("b")).unwrap();
    };
    let f = route::run(BIND, &long_relative(), &opts(), true, Some(&cfg))
        .err()
        .unwrap();
    assert_eq!(f.error.kind(), io::ErrorKind::Other);
    assert_eq!(f.route, Some(names::LINK_DIR));
    let abs = root.join("a").join(long_relative());
    let e = connect(&abs, &real_link_dir(&root.join("p")))
        .err()
        .unwrap();
    assert_eq!(
        errno(&e.error),
        Some(libc::ECONNREFUSED),
        "the listener is open"
    );
}

// An Options.base of the wrong form is an argument error before the name in
// the abstract namespace is looked at (Linux / Android): an abstract name is
// otherwise handed to std as is (a bind's error would come from standard,
// and resolve would hand the name back), and the wrong base would go unseen
// (hub SPEC "エラー": the form is checked before the abstract name). No route
// is named.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn an_options_base_of_the_wrong_form_is_refused_before_an_abstract_name() {
    let (_t, place) = setup();
    let cfg = config(&place);
    let name = format!("\0sockat-test-{}", base::random_name().unwrap());
    let p = PathBuf::from(OsStr::from_bytes(name.as_bytes()));
    let options = Options {
        base: Some(PathBuf::from("relative/b")),
        ..opts()
    };
    for req in [BIND, CONNECT] {
        let f = route::run(req, &p, &options, true, Some(&cfg))
            .err()
            .unwrap();
        assert_eq!(f.error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(f.route, None);
        assert!(f.skipped.is_empty());
    }
    let f = route::resolve(route::Use::Bind, &p, &options, Some(&cfg))
        .err()
        .unwrap();
    assert_eq!(f.error.kind(), io::ErrorKind::InvalidInput);
    assert_eq!(f.route, None);
}

// ---------------------------------------------------------------------------
// umask
// ---------------------------------------------------------------------------

// A umask that takes the owner's bits (0277: no write, 0477: no read, 0177:
// no search) or all of group and others (0077) still gives B and D mode 0711
// and a bind at the path (the umask still applies to the socket file
// itself, as with std). Run in a child (the umask is process-wide).
#[test]
fn a_umask_does_not_change_b_and_d() {
    for mask in ["277", "477", "177", "77"] {
        let t = tmp();
        run_child(
            "tests::child_umask",
            &[
                ("SOCKAT_CHILD_PATH", t.path().as_os_str()),
                ("SOCKAT_CHILD_MASK", OsStr::new(mask)),
            ],
        );
    }
}

#[test]
fn child_umask() {
    if !in_child() {
        return;
    }
    let root = env_path("SOCKAT_CHILD_PATH");
    let mask = u32::from_str_radix(&std::env::var("SOCKAT_CHILD_MASK").unwrap(), 8).unwrap();
    let place = root.join("p");
    std::fs::create_dir(&place).unwrap();
    let p = nest(&root, 200, "s.sock");
    sys::set_umask(mask);
    let r = bind(&p, &opts(), &real_link_dir(&place)).unwrap();
    sys::set_umask(0o022);
    assert_eq!(r.route, names::LINK_DIR);
    let b = b_of(&place);
    assert_eq!(std::fs::metadata(&b).unwrap().mode() & 0o7777, 0o711);
    let d = b.join(entries(&b).pop().unwrap());
    assert_eq!(std::fs::metadata(&d).unwrap().mode() & 0o7777, 0o711);
    assert!(is_socket(&p));
}

// ---------------------------------------------------------------------------
// names and places
// ---------------------------------------------------------------------------

// The counted E names are base 36 (0-9, a-z): short, and never the same
// twice.
#[test]
fn counted_names_are_base_36() {
    assert_eq!(base::counted_name(0), "0");
    assert_eq!(base::counted_name(35), "z");
    assert_eq!(base::counted_name(36), "10");
    let all: std::collections::HashSet<_> = (0..2000).map(base::counted_name).collect();
    assert_eq!(all.len(), 2000);
}

// The random names are 10 letters and digits from the system's generator,
// and two draws differ.
#[test]
fn random_names_are_10_alphanumerics() {
    let a = base::random_name().unwrap();
    let b = base::random_name().unwrap();
    assert_eq!(a.len(), 10);
    assert!(a.bytes().all(|c| c.is_ascii_alphanumeric()));
    assert_ne!(a, b);
}

// The places 1-3 come from the environment in the order /tmp,
// $XDG_RUNTIME_DIR, $TMPDIR, each only when set to an absolute path (an
// empty or relative one is not used), and a path already listed is not
// listed again.
#[test]
fn the_places_come_from_the_environment() {
    let p = |t: Option<&str>, x: Option<&str>| {
        base::places_from(t.map(Into::into), x.map(Into::into))
            .into_iter()
            .map(|(_, p)| p)
            .collect::<Vec<_>>()
    };
    let v = |s: &[&str]| s.iter().map(PathBuf::from).collect::<Vec<_>>();
    assert_eq!(p(None, None), v(&["/tmp"]));
    assert_eq!(
        p(Some("/t/"), Some("/run/u")),
        v(&["/tmp", "/run/u", "/t/"])
    );
    assert_eq!(p(Some(""), Some("rel")), v(&["/tmp"]));
    assert_eq!(p(Some("/tmp/"), None), v(&["/tmp"]));
}
