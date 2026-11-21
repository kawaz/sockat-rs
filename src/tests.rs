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
        .prefix("srt-")
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

/// A situation in which sockat returns the error the OS gave, as std meets it
/// at a short path ([`std_errno`]).
#[derive(Clone, Copy, Debug)]
enum Short {
    /// A stream bind whose directory is missing.
    BindInAMissingDirectory,
    /// A stream bind whose directory is a regular file.
    BindUnderAFile,
    /// A stream bind with a regular file at the path.
    BindOntoAFile,
    /// A stream connect with nothing at the path (its directory there).
    ConnectToNothing,
    /// A stream connect to a socket file nobody listens on.
    ConnectToNobodyListening,
    /// Removing a file from a directory its owner cannot write to (not as
    /// root, whom the mode does not stop).
    RemoveFromAnUnwritableDirectory,
}

/// The errno std's own call returns for `case` on this OS, the situation made
/// at a short path in a directory of the call's own. A test compares the
/// errno sockat returns with this rather than with a constant: sockat returns
/// the OS's error unchanged, and the OSes may answer the same situation with
/// different errnos (illumos answers a datagram connect to a socket file no
/// socket holds with EDESTADDRREQ where Linux and macOS say ECONNREFUSED; hub
/// SPEC "エラー"). std must fail here, with an errno: if it succeeds the situation
/// was not made, and an errno equal to nothing would prove nothing.
fn std_errno(case: Short) -> i32 {
    let t = tmp();
    let p = t.path().join("s.sock");
    let r = match case {
        Short::BindInAMissingDirectory => {
            UnixListener::bind(t.path().join("none/s.sock")).map(drop)
        }
        Short::BindUnderAFile => {
            std::fs::write(t.path().join("f"), b"").unwrap();
            UnixListener::bind(t.path().join("f/s.sock")).map(drop)
        }
        Short::BindOntoAFile => {
            std::fs::write(&p, b"").unwrap();
            UnixListener::bind(&p).map(drop)
        }
        Short::ConnectToNothing => UnixStream::connect(&p).map(drop),
        Short::ConnectToNobodyListening => {
            // Bound by a child that has exited: a listener dropped here
            // could still be held, and listen, in a child another test is
            // starting meanwhile (between its fork and its exec).
            bind_in_child(&p, t.path());
            UnixStream::connect(&p).map(drop)
        }
        Short::RemoveFromAnUnwritableDirectory => {
            assert_ne!(sys::effective_uid(), 0, "the mode does not stop root");
            let dir = t.path().join("u");
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("f"), b"").unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            let r = std::fs::remove_file(dir.join("f"));
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            r
        }
    };
    let Err(e) = r else {
        panic!("{case:?}: std succeeded at a short path")
    };
    e.raw_os_error()
        .unwrap_or_else(|| panic!("{case:?}: std's error has no errno: {e}"))
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

/// The spelling the kernel keeps for `listener`, read from `getsockname` as
/// the kernel wrote it ([`sys::local_address`]), not through std's
/// `local_addr()`: Rust 1.99 and earlier drop its last byte on OpenBSD
/// (rust-lang/rust#118925), and what sockat promises is the kernel's spelling.
fn reported(listener: &UnixListener) -> PathBuf {
    sys::local_address(listener.as_fd()).unwrap().1
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

// The tests read the spelling the kernel keeps from the address it writes
// (sys::path_in, behind reported() and sys::recv_from), so that reading must
// give the whole path whichever length the kernel reports with it: one that
// counts the NUL (Linux), one that does not (FreeBSD, NetBSD, a caller that
// bound without it), and the whole structure (OpenBSD's getsockname, the
// case std's reading of Rust 1.99 and earlier cuts short). A length past the
// buffer reads only the buffer, and one shorter than the header reads
// nothing. Only the reading is checked here, as OpenBSD is not at hand.
#[test]
fn the_kernels_address_is_read_up_to_the_nul_whatever_length_comes_with_it() {
    let offset = std::mem::offset_of!(libc::sockaddr_un, sun_path);
    let path = b"/tmp/x/s.sock";
    let mut storage = [0u8; 256];
    storage[offset..offset + path.len()].copy_from_slice(path);
    let whole = offset + sys::SUN_PATH_CAP;
    for len in [offset + path.len(), offset + path.len() + 1, whole, 1000] {
        assert_eq!(
            sys::path_in(&storage, len),
            Path::new("/tmp/x/s.sock"),
            "{len}"
        );
    }
    assert_eq!(sys::path_in(&storage, offset - 1), Path::new(""));
}

// A path std accepts goes to std unchanged: the longest one (sun_path less
// the NUL: 107 on Linux / illumos, 103 on Apple and the BSDs) is bound by
// std (route standard, the whole path, its last byte included, kept by the
// kernel as the address, nothing made in the place), and one byte more is not
// (a route through B, with standard passed over). The boundary is the one std
// itself draws, so every path std can take behaves as with std.
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
// failure there is std's own, from the route standard: the errno std's bind
// gives for a missing directory on this OS, not a constant (the OS's errno is
// passed on unchanged, and OSes may differ).
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
    assert_eq!(
        errno(&f.error),
        Some(std_errno(Short::BindInAMissingDirectory))
    );
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
// file. Before the cleanup the spelling reaches the socket file, so the
// ENOENT after it comes from the cleanup and not from a spelling that never
// named anything.
#[test]
fn a_spelling_cleaned_up_names_nothing() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    let other = p.with_file_name("other");
    std::fs::write(&other, b"keep").unwrap();
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    let spelling = reported(&listener(&mut r));
    let reached = std::fs::metadata(&spelling).unwrap();
    let file = std::fs::symlink_metadata(&p).unwrap();
    assert_eq!((reached.dev(), reached.ino()), (file.dev(), file.ino()));
    r.link.as_ref().unwrap().cleanup().unwrap();
    let e = std::fs::remove_file(&spelling).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOENT));
    assert!(is_socket(&p));
    assert_eq!(std::fs::read(&other).unwrap(), b"keep");
}

// A bind through link-dir that fails for real (a file is at the path) is that
// failure, from link-dir, with the errno std's bind gives a short path with
// a file at it on this OS (EADDRINUSE where it has been seen; the OS's errno
// is passed on unchanged, so it is compared, not fixed), and the E made for
// it is removed before the error is returned: a failed bind leaves no E. The
// routes after it are not tried (they would fail the same way), so nothing
// but standard is in skipped.
#[test]
fn a_failed_bind_through_link_dir_removes_its_e_and_is_not_retried() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    std::fs::write(&p, b"").unwrap();
    let f = bind(&p, &opts(), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(std_errno(Short::BindOntoAFile)));
    assert_eq!(f.route, Some(names::LINK_DIR));
    assert_eq!(skipped_names(&f.skipped), vec![names::STANDARD]);
    assert_no_e(&b_of(&place));
}

/// A default bind (one that returns no `SocketFile`, as `bind` and
/// `datagram::bind` do): `route::run` without remembering the file.
fn bind_default(p: &Path, cfg: &Config) -> Result<Reached, route::Failed> {
    route::run(BIND, p, &opts(), false, Some(cfg))
}

/// The E in the only D of `b`, as `(name, what it points to)`, sorted.
fn links_in(b: &Path) -> Vec<(String, PathBuf)> {
    let ds = entries(b);
    assert_eq!(ds.len(), 1, "{ds:?}");
    let d = b.join(&ds[0]);
    entries(&d)
        .into_iter()
        .map(|e| {
            let to = std::fs::read_link(d.join(&e)).unwrap();
            (e, to)
        })
        .collect()
}

// The default binds share one link-dir E per (D, socket's directory) (hub
// SPEC-routes "link-dir", RV-Q2): a long-lived process binding many sockets
// in one directory keeps one E for it, not one per bind, since nothing ever
// cleans the default bind's E up. So:
//
// - three default binds (stream and datagram) in one directory leave one E,
//   pointing to that directory's real path, and each socket is at its path
//   and reached through the spelling the kernel keeps (`<B>/<D>/<E>/<name>`)
// - a default bind in another directory makes its own E (one per directory)
// - bind_ex and resolve_for_bind still make an E per call (their cleanup()
//   removes theirs alone), and cleaning one up leaves the shared E
#[test]
fn default_binds_share_one_link_dir_e_per_directory() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let b = b_of(&place);
    let dir = nest(t.path(), 200, "x").parent().unwrap().to_path_buf();
    let real = std::fs::canonicalize(&dir).unwrap();
    let mut listeners = Vec::new();
    for name in ["s1.sock", "s2.sock"] {
        let mut r = bind_default(&dir.join(name), &cfg).unwrap();
        assert_eq!(r.route, names::LINK_DIR);
        listeners.push((dir.join(name), listener(&mut r)));
    }
    let dgram = Request::Bind(SockType::Datagram);
    let r = route::run(dgram, &dir.join("d.sock"), &opts(), false, Some(&cfg)).unwrap();
    assert_eq!(r.route, names::LINK_DIR);
    let _dgram = UnixDatagram::from(r.socket.unwrap());
    assert!(is_socket(&dir.join("d.sock")));
    assert_eq!(links_in(&b), vec![("0".to_owned(), real.clone())]);
    for (p, l) in &listeners {
        assert!(is_socket(p));
        let spelling = reported(l);
        assert_eq!(spelling.parent().unwrap().file_name().unwrap(), "0");
        ping(l, UnixStream::connect(&spelling).unwrap().into());
    }

    let other = nest(&t.path().join("o"), 200, "s.sock");
    let _o = listener(&mut bind_default(&other, &cfg).unwrap());
    assert_eq!(links_in(&b).len(), 2);

    let mut ex = bind(&dir.join("e.sock"), &opts(), &cfg).unwrap();
    let _ex = listener(&mut ex);
    let resolved =
        route::resolve(route::Use::Bind, &dir.join("r.sock"), &opts(), Some(&cfg)).unwrap();
    assert_eq!(links_in(&b).len(), 4);
    ex.link.as_ref().unwrap().cleanup().unwrap();
    resolved.link.as_ref().unwrap().cleanup().unwrap();
    let left = links_in(&b);
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left.contains(&("0".to_owned(), real)));
}

// The shared E is used again only while it is still the symlink to the
// directory (hub SPEC-routes "link-dir": readlinkat on D's fd, and what it
// points to compared with the directory's real path). One removed by hand,
// or put back pointing elsewhere, is let go: the next default bind makes a
// new E and binds there, the socket ends up at its own path (never in the
// directory the replaced E points to), and the new E is shared from then on.
#[test]
fn a_shared_e_removed_or_pointing_elsewhere_is_made_again() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let b = b_of(&place);
    let dir = nest(t.path(), 200, "x").parent().unwrap().to_path_buf();
    let real = std::fs::canonicalize(&dir).unwrap();
    let elsewhere = tmp();
    let _first = listener(&mut bind_default(&dir.join("s1.sock"), &cfg).unwrap());
    let d = b.join(entries(&b).pop().unwrap());

    std::fs::remove_file(d.join("0")).unwrap();
    let mut r = bind_default(&dir.join("s2.sock"), &cfg).unwrap();
    let l = listener(&mut r);
    assert!(is_socket(&dir.join("s2.sock")));
    assert_eq!(links_in(&b), vec![("1".to_owned(), real.clone())]);
    ping(&l, UnixStream::connect(reported(&l)).unwrap().into());

    std::fs::remove_file(d.join("1")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), d.join("1")).unwrap();
    let mut r = bind_default(&dir.join("s3.sock"), &cfg).unwrap();
    let l = listener(&mut r);
    assert!(is_socket(&dir.join("s3.sock")));
    assert!(
        entries(elsewhere.path()).is_empty(),
        "bound through the replaced E"
    );
    let spelling = reported(&l);
    assert_eq!(spelling.parent().unwrap().file_name().unwrap(), "2");
    let mut r = bind_default(&dir.join("s4.sock"), &cfg).unwrap();
    assert_eq!(reported(&listener(&mut r)).parent(), spelling.parent());
}

// A default bind that fails for real on link-dir (a file at the path: the
// errno std's bind gives a short path in the same situation on this OS,
// compared rather than fixed) leaves the shared E, the one an earlier call
// made (a connect through that call's spelling still works) and the one it
// made itself when it was the first in its directory: another default bind
// may be going through it at the same moment, and the next default bind
// there uses it (hub SPEC-routes "経路の失敗の扱い"). The E of bind_ex
// (link-dir) and of the default bind's link-file (macOS, NetBSD: an E per
// bind) are still removed when their bind fails.
#[test]
fn a_failed_default_bind_leaves_the_shared_e_for_the_next() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let b = b_of(&place);
    let dir = nest(t.path(), 200, "x").parent().unwrap().to_path_buf();
    let l = listener(&mut bind_default(&dir.join("s1.sock"), &cfg).unwrap());
    std::fs::write(dir.join("f.sock"), b"").unwrap();
    let f = bind_default(&dir.join("f.sock"), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(std_errno(Short::BindOntoAFile)));
    assert_eq!(f.route, Some(names::LINK_DIR));
    assert_eq!(links_in(&b).len(), 1);
    ping(&l, UnixStream::connect(reported(&l)).unwrap().into());

    let fresh = nest(&t.path().join("n"), 200, "f.sock");
    let fresh_real = std::fs::canonicalize(fresh.parent().unwrap()).unwrap();
    std::fs::write(&fresh, b"").unwrap();
    let f = bind_default(&fresh, &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(std_errno(Short::BindOntoAFile)));
    let left = links_in(&b);
    assert_eq!(left.len(), 2, "the failed bind's own E was removed");
    let made = left
        .iter()
        .find(|(_, to)| *to == fresh_real)
        .unwrap()
        .0
        .clone();
    std::fs::remove_file(&fresh).unwrap();
    let l = listener(&mut bind_default(&fresh, &cfg).unwrap());
    assert_eq!(
        links_in(&b).len(),
        2,
        "the next bind did not use the E left"
    );
    assert_eq!(
        reported(&l).parent().unwrap().file_name().unwrap(),
        made.as_str()
    );

    let ex = nest(&t.path().join("x"), 200, "f.sock");
    std::fs::write(&ex, b"").unwrap();
    let f = bind(&ex, &opts(), &cfg).err().unwrap();
    assert_eq!(f.route, Some(names::LINK_DIR));
    assert_eq!(links_in(&b).len(), 2, "bind_ex left its E");

    if cfg!(any(target_vendor = "apple", target_os = "netbsd")) {
        // The same B and D as above (clones of a config share its B).
        let lf = Config {
            follows: Follows::Yes,
            ..cfg.clone()
        };
        let p = nest(&t.path().join("lf"), 400, &"n".repeat(200));
        std::fs::write(&p, b"").unwrap();
        let f = bind_default(&p, &lf).err().unwrap();
        assert_eq!(f.route, Some(names::LINK_FILE));
        assert_eq!(
            links_in(&b).len(),
            2,
            "the default bind's link-file left its E"
        );
    }
}

// Many default binds at once in one directory, on many threads, all
// succeed with one D and one E for that directory between them (hub SPEC
// "実装が確かめる試験の観点": the sharing of the default bind's E does not
// race), and every socket is at its own path and answers.
#[test]
fn concurrent_default_binds_share_one_e() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let dir = nest(t.path(), 200, "x").parent().unwrap().to_path_buf();
    std::thread::scope(|s| {
        for i in 0..16 {
            let (cfg, p) = (&cfg, dir.join(format!("s{i:02}.sock")));
            s.spawn(move || {
                let mut r = bind_default(&p, cfg).unwrap();
                assert_eq!(r.route, names::LINK_DIR);
                let l = listener(&mut r);
                ping(&l, connect(&p, cfg).unwrap().socket.unwrap());
            });
        }
    });
    let real = std::fs::canonicalize(&dir).unwrap();
    assert_eq!(links_in(&b_of(&place)), vec![("0".to_owned(), real)]);
    for i in 0..16 {
        assert!(is_socket(&dir.join(format!("s{i:02}.sock"))));
    }
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
// promise is "the same as std". Only the sockets sockat makes itself are
// worth checking: a bind makes one on the routes that bind a socket of its
// own (thread-cwd on Apple, at on FreeBSD, taken here by a file name too long
// for link-dir with link-file out of the order), while standard, link-dir and
// link-file hand the spelling to std's bind, so on NetBSD and DragonFly every
// bind socket is std's and the listener is made by std instead. A nonblocking
// connect always makes its own socket (std cannot connect one), on every
// route.
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
    let _l = if cfg!(any(target_vendor = "apple", target_os = "freebsd")) {
        let mut r = bind(&long, &opts(), &cfg).unwrap();
        assert!(
            [names::THREAD_CWD, names::AT].contains(&r.route),
            "{}",
            r.route
        );
        let l = listener(&mut r);
        assert_eq!(sys::so_nosigpipe(l.as_fd()).unwrap(), std_says);
        l
    } else {
        listen_at(&long)
    };
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
// at it fails for real: that is the result, with the errno std's bind gives a
// short path with a file at it on this OS (compared, not fixed: the OS's
// errno is passed on unchanged), E is removed, and the next route is not
// tried.
#[cfg(any(target_vendor = "apple", target_os = "netbsd"))]
#[test]
fn a_failed_link_file_bind_is_the_failure() {
    let (t, place) = setup();
    let cfg = no_proc(config(&place));
    let p = nest(t.path(), 400, &"n".repeat(200));
    std::fs::write(&p, b"").unwrap();
    let f = bind(&p, &opts(), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(std_errno(Short::BindOntoAFile)));
    assert_eq!(f.route, Some(names::LINK_FILE));
    assert_no_e(&b_of(&place));
}

// Where whether a bind follows the symlink is not known (told at run time),
// a failed link-file bind with nothing at the path means the kernel did not
// follow it: link-file is passed over, its E removed, and the next route
// tried. With something at the path the same failure is the result: the
// errno std's bind gives a short path with a file at it on this OS (compared,
// not fixed: the OS's errno is passed on unchanged). Forced here on a kernel
// that does not follow (it binds the symlink's own name, which exists).
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
    assert_eq!(errno(&f.error), Some(std_errno(Short::BindOntoAFile)));
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

// Something already at B's default name in one of the places 1-3 that fails
// the checks of B (here: a directory writable by its group, a symlink, a
// regular file) means the name is taken (占拠, hub SPEC-routes "B の作り方"
// 5): the place cannot be used, nothing is made in it (no other B beside the
// one there), what is there is left as it is, and B is made in the next
// place. With no next place, the routes through B cannot be used, and their
// reason says the name is taken (hub SPEC "skipped"), for a bind (link-dir)
// and for a connect (link-file) alike, so the user can tell it from a place
// that is just missing. Another uid's B is not made here (it needs root).
#[test]
fn an_occupied_b_is_passed_over_for_the_next_place() {
    /// How to put something at B's name.
    type Put = fn(&Path);
    let occupy: [(&str, Put); 3] = [
        ("group-writable", |b| {
            std::fs::create_dir(b).unwrap();
            std::fs::set_permissions(b, std::fs::Permissions::from_mode(0o770)).unwrap();
        }),
        ("symlink", |b| {
            std::os::unix::fs::symlink("/", b).unwrap();
        }),
        ("file", |b| std::fs::write(b, b"").unwrap()),
    ];
    for (kind, put) in occupy {
        let t = tmp();
        let taken = t.path().join("taken");
        let next = t.path().join("next");
        std::fs::create_dir(&taken).unwrap();
        std::fs::create_dir(&next).unwrap();
        let b = b_of(&taken);
        put(&b);
        let before = std::fs::symlink_metadata(&b).unwrap();
        let cfg = with_places(
            real_link_dir(&taken),
            &[(Place::Tmp, &taken), (Place::Tmpdir, &next)],
        );
        let r = bind(&nest(&t.path().join("a"), 200, "s.sock"), &opts(), &cfg).unwrap();
        assert_eq!(
            (r.route, r.base),
            (names::LINK_DIR, Some("tmpdir")),
            "{kind}"
        );
        assert_eq!(
            entries(&taken),
            vec![format!("sockat-{}", sys::effective_uid())],
            "{kind}"
        );
        let after = std::fs::symlink_metadata(&b).unwrap();
        assert_eq!(
            (after.ino(), after.mode(), after.file_type().is_dir()),
            (before.ino(), before.mode(), before.file_type().is_dir()),
            "{kind}: what was there was changed"
        );
        if after.file_type().is_dir() {
            assert!(entries(&b).is_empty(), "{kind}: something made in it");
        }

        let alone = no_thread(with_places(real_link_dir(&taken), &[(Place::Tmp, &taken)]));
        let q = nest(&t.path().join("q"), 200, "s.sock");
        let r = bind(&q, &opts(), &alone);
        assert!(
            skipped_of(&r).contains(&(names::LINK_DIR, names::BASE_OCCUPIED)),
            "{kind}: {:?}",
            skipped_of(&r)
        );
        let c = nest(&t.path().join("c"), 300, &"n".repeat(100));
        let _l = listen_at(&c);
        let only_link_file = no_thread(no_direct(no_proc(config(&taken))));
        let c = connect(&c, &only_link_file);
        assert!(
            skipped_of(&c).contains(&(names::LINK_FILE, names::BASE_OCCUPIED)),
            "{kind}: {:?}",
            skipped_of(&c)
        );
        assert_eq!(
            entries(&taken),
            vec![format!("sockat-{}", sys::effective_uid())],
            "{kind}"
        );
    }
}

// A call that passes over a place whose B's default name is taken (hub
// SPEC "skipped") adds that to the reason of a route through B that cannot be
// used for another reason too: here the next place's real path is too long
// for the file name, so link-dir is passed over as too long with the taken
// place added, and no B is made in either place (the spelling is known not
// to fit, hub SPEC-routes "実体の置き場"). Once a call has made the next
// place's B the process's B, later calls do not look at the taken place, and
// their reason does not mention it.
#[test]
fn a_reason_says_the_call_passed_over_an_occupied_place() {
    let t = tmp();
    let taken = t.path().join("taken");
    let next = t.path().join("n".repeat(40));
    std::fs::create_dir(&taken).unwrap();
    std::fs::create_dir(&next).unwrap();
    std::fs::write(b_of(&taken), b"").unwrap();
    let cfg = with_places(
        real_link_dir(&next),
        &[(Place::Tmp, &taken), (Place::Tmpdir, &next)],
    );
    let long = name_just_past(&next);
    // resolve tries the routes through B alone (no thread-cwd / at, which
    // would make a B to work in).
    let reasons = |p: &Path| {
        let f = route::resolve(route::Use::Bind, p, &opts(), Some(&cfg))
            .err()
            .unwrap();
        f.skipped
            .iter()
            .map(|s| (s.name, s.reason))
            .collect::<Vec<_>>()
    };
    let first = reasons(&nest(&t.path().join("a"), 250, &long));
    assert!(
        first.contains(&(names::LINK_DIR, names::TOO_LONG_OCCUPIED)),
        "{first:?}"
    );
    assert!(entries(&next).is_empty(), "B made where no spelling fits");

    let r = bind(&nest(&t.path().join("b"), 200, "s.sock"), &opts(), &cfg).unwrap();
    assert_eq!(r.base, Some("tmpdir"));
    let later = reasons(&nest(&t.path().join("c"), 250, &long));
    assert!(
        later.contains(&(names::LINK_DIR, names::TOO_LONG)),
        "{later:?}"
    );
}

// A B already at the default name that passes the checks is used with its
// mode as it is (0700 here: sockat does not change the mode of a B it did not
// make).
#[test]
fn an_existing_b_that_passes_the_checks_is_used_as_it_is() {
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
// and the diagnostic base is `given` whatever the form of the spelling. The
// spelling the kernel keeps (read from getsockname) reaches the socket
// through that B, and still does after sweep_base, which leaves this
// process's live D. A
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
        let mut r = bind(&q, &options, &cfg).unwrap();
        assert_eq!(r.route, names::LINK_DIR);
        assert_eq!(r.base, Some("given"));
        let l = listener(&mut r);
        ping(&l, UnixStream::connect(reported(&l)).unwrap().into());
        base::sweep_base(&base).unwrap();
        ping(&l, UnixStream::connect(reported(&l)).unwrap().into());
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
// and B itself) is judged when the process first takes B, and kept, except
// that B itself, judged not to let others search, is looked at again (fstat
// of B's fd) by each call for usable_cross_uid (hub SPEC-routes
// "実体の置き場"). Two processes making B at once can take it between the
// other's mkdirat (0700) and its mode set to 0711; kept as judged then, that
// B would never serve usable_cross_uid in that process although it is 0711.
//
// - B 0700 when taken: usable_cross_uid passes it over while it is 0700, and
//   once it is 0711 the next usable_cross_uid call goes through it (through
//   the D already in it: no second D)
// - B 0711 when taken: still gives a spelling marked usable_cross_uid after
//   it is made 0700 (what lets others search is not looked at again)
// - a way to B that was 0700 when B was taken: B is not taken for
//   usable_cross_uid after the way is made 0711 (the way is not looked at
//   again)
// - an Options.base taken while 0700: EPERM for usable_cross_uid before any
//   route while it is 0700, and gone through once it is 0711
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
    let crossing = |r: &Reached| {
        matches!(
            r.spelling,
            Some(Spelling::Linked {
                usable_cross_uid: true,
                ..
            })
        )
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
    let r = bind(&nest(&t.path().join("b"), 200, "s"), &cross, &cfg);
    assert!(
        skipped_of(&r).contains(&(names::LINK_DIR, names::NO_CROSS_UID_BASE)),
        "{:?}",
        r.as_ref().map(|r| (r.route, r.spelling))
    );
    mode(&b, 0o711);
    let r = bind(&nest(&t.path().join("b2"), 200, "s"), &cross, &cfg).unwrap();
    assert_eq!((r.route, r.base), (names::LINK_DIR, Some("tmp")));
    assert!(crossing(&r), "{:?}", r.spelling);
    assert_eq!(entries(&b).len(), 1, "a second D in B");

    let way = t.path().join("way");
    std::fs::create_dir(&way).unwrap();
    mode(&way, 0o700);
    let cfg = no_link_file(no_proc(config(&way)));
    bind(&nest(&t.path().join("w"), 200, "s"), &opts(), &cfg).unwrap();
    mode(&way, 0o711);
    let r = bind(&nest(&t.path().join("w2"), 200, "s"), &cross, &cfg);
    assert!(
        skipped_of(&r).contains(&(names::LINK_DIR, names::NO_CROSS_UID_BASE)),
        "{:?}",
        r.as_ref().map(|r| (r.route, r.spelling))
    );

    let open = t.path().join("open");
    std::fs::create_dir(&open).unwrap();
    mode(&open, 0o711);
    let cfg = no_link_file(no_proc(config(&open)));
    bind(&nest(&t.path().join("c"), 200, "s"), &opts(), &cfg).unwrap();
    mode(&b_of(&open), 0o700);
    let r = bind(&nest(&t.path().join("d"), 200, "s"), &cross, &cfg).unwrap();
    assert!(crossing(&r), "{:?}", r.spelling);

    let given = t.path().join("given");
    std::fs::create_dir(&given).unwrap();
    mode(&given, 0o700);
    let at = |cross: bool| Options {
        usable_cross_uid: cross,
        base: Some(given.clone()),
    };
    let r = bind(&nest(&t.path().join("e"), 200, "s"), &at(false), &cfg).unwrap();
    assert_eq!(r.base, Some("given"));
    let f = bind(&nest(&t.path().join("f"), 200, "s"), &at(true), &cfg)
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::EPERM));
    assert_eq!(f.route, None);
    assert!(f.skipped.is_empty());
    mode(&given, 0o711);
    let r = bind(&nest(&t.path().join("f2"), 200, "s"), &at(true), &cfg).unwrap();
    assert_eq!((r.route, r.base), (names::LINK_DIR, Some("given")));
    assert!(crossing(&r), "{:?}", r.spelling);
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
    assert!(crossing(&r), "{:?}", r.spelling);
}

// The way to an Options.base (the directories from the root down to its
// parent) is checked on every call (hub SPEC-routes "実体の置き場": the safety
// check of Options.base is made on every call; sweep_base checks its path as
// Options.base is). One that passed once and then became changeable by
// another user (a directory on the way made group-writable, not sticky) is
// EPERM for the next bind, before any route (no route, nothing skipped), and
// for sweep_base; once it is safe again, the next bind goes through it. A
// check kept from the first call would let another user swap the way, and B
// with it.
#[test]
fn the_way_to_an_options_base_is_checked_on_every_call() {
    let (t, auto) = setup();
    let cfg = real_link_dir(&auto);
    let way = t.path().join("way");
    std::fs::create_dir(&way).unwrap();
    let given = Options {
        base: Some(way.join("b")),
        ..opts()
    };
    let mode = |m: u32| {
        std::fs::set_permissions(&way, std::fs::Permissions::from_mode(m)).unwrap();
    };
    let r = bind(&nest(&t.path().join("a"), 200, "s.sock"), &given, &cfg).unwrap();
    assert_eq!(r.base, Some("given"));
    mode(0o770);
    let f = bind(&nest(&t.path().join("b"), 200, "s.sock"), &given, &cfg)
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::EPERM));
    assert_eq!(f.route, None);
    assert!(f.skipped.is_empty());
    let e = base::sweep_base(&way.join("b")).unwrap_err();
    assert_eq!(errno(&e), Some(libc::EPERM));
    mode(0o700);
    let r = bind(&nest(&t.path().join("c"), 200, "s.sock"), &given, &cfg).unwrap();
    assert_eq!(r.base, Some("given"));
    base::sweep_base(&way.join("b")).unwrap();
}

// A B or D removed from under the process (by the OS's clean-up or by hand)
// is let go and made again by the next call, and the one let go is kept with
// its fd open for the rest of the process (hub SPEC-routes "実体の置き場").
// Closed, its fd number would go to the next directory opened, and a live
// socket bound through `/proc/<pid>/fd/<that number>/...` (procB) would then
// name a place in that directory. So after D and then B are removed and the
// next binds make new ones, the fds held on the old D and the old B are
// still open on them (looked at through /dev/fd, which shows what an fd is
// open on).
#[test]
fn a_removed_b_or_d_is_let_go_with_its_fd_kept_open() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let id = |fd: std::os::fd::RawFd| {
        let m = std::fs::metadata(format!("/dev/fd/{fd}")).unwrap();
        (m.dev(), m.ino())
    };
    // A default bind: its E holds nothing of D, so only the process holds it.
    let _a = bind_default(&nest(&t.path().join("a"), 200, "s.sock"), &cfg).unwrap();
    let b = b_of(&place);
    let (b_fd, d_fd) = (
        base::registered_fd(&b).unwrap(),
        base::registered_d_fd(&b).unwrap(),
    );
    let (b_id, d_id) = (id(b_fd), id(d_fd));

    std::fs::remove_dir_all(b.join(entries(&b).pop().unwrap())).unwrap();
    let _b = bind(&nest(&t.path().join("b"), 200, "s.sock"), &opts(), &cfg).unwrap();
    assert_ne!(
        base::registered_d_fd(&b),
        Some(d_fd),
        "the D let go is used"
    );
    assert_eq!(id(d_fd), d_id, "the fd of the D let go was closed");

    std::fs::remove_dir_all(&b).unwrap();
    let _c = bind(&nest(&t.path().join("c"), 200, "s.sock"), &opts(), &cfg).unwrap();
    assert!(b.is_dir());
    assert_eq!(id(b_fd), b_id, "the fd of the B let go was closed");
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

/// The uid and gid a child gives up root to ([`unprivileged`]): nobody on
/// Linux and the BSDs; any unused number serves.
const NOBODY: libc::uid_t = 65534;

/// Whether the test named `test` (its full name, `tests::...`) is to run its
/// body here, as a user that a directory's mode stops. Not root: yes, here.
/// Root (whom the mode does not stop): no; the test runs in a child process
/// ([`run_child`]) that gives up root for good first ([`sys::drop_root`] to
/// [`NOBODY`]), and the child's result is the test's. The child makes its own
/// temporary directories after giving up root, so it owns everything it walks
/// through below the sticky `/tmp`; `TMPDIR` is set to `/tmp` for it, as the
/// parent's may be a directory only root can enter.
fn unprivileged(test: &str) -> bool {
    if in_child() {
        if sys::effective_uid() == 0 {
            sys::drop_root(NOBODY).unwrap();
        }
        assert_ne!(sys::effective_uid(), 0);
        return true;
    }
    if sys::effective_uid() == 0 {
        run_child(test, &[("TMPDIR", OsStr::new("/tmp"))]);
        return false;
    }
    true
}

// An Options.base that passes its check but cannot be made (mkdir fails) is
// that error, with no route (no route made the call) and the routes passed
// over before it in skipped, for a bind, a connect (whose link-file is the
// route that needs B) and resolve_for_bind; nothing is made there or in
// another place. The base is under a directory of this user with mode 0500:
// the check passes (owned by this user, nobody else can write to it) and
// mkdir fails with the errno std's mkdir gets there (EACCES), which is none
// of the errnos the check returns (ENOENT, ENOTDIR, EPERM), so the error seen
// is mkdir's. Root, whom the mode does not stop, runs it in a child that has
// given up root ([`unprivileged`]).
#[test]
fn an_options_base_that_cannot_be_made_after_its_check_is_that_error() {
    if !unprivileged("tests::an_options_base_that_cannot_be_made_after_its_check_is_that_error") {
        return;
    }
    let (t, auto) = setup();
    let ro = t.path().join("ro");
    std::fs::create_dir(&ro).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500)).unwrap();
    let given = ro.join("b");
    let want = std::fs::create_dir(&given)
        .expect_err("the premise: mkdir in a 0500 directory fails")
        .raw_os_error();
    assert!(
        ![libc::ENOENT, libc::ENOTDIR, libc::EPERM].contains(&want.unwrap()),
        "the premise: mkdir's errno {want:?} is not one the check returns"
    );
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
    assert!(entries(&ro).is_empty());
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

// sweep() passes over a B it cannot read (no permission to list it) without
// an error, as it passes over a place or B that fails the checks (hub SPEC
// "sweep"); what is in it is left. Not as root, whom the mode does not stop.
#[test]
fn sweep_passes_over_a_b_it_cannot_read() {
    if sys::effective_uid() == 0 {
        eprintln!("the mode does not stop root");
        return;
    }
    let (t, place) = setup();
    bind_in_child(&nest(t.path(), 200, "s.sock"), &place);
    let b = b_of(&place);
    let before = entries(&b);
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o111)).unwrap();
    let swept = base::sweep(&[(Place::Tmp, place.clone())]);
    std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0o711)).unwrap();
    swept.unwrap();
    assert_eq!(entries(&b), before);
}

/// [`base::AFTER_SWEEP_LOCK`] for the tests: the D just locked is moved aside
/// (`<name>.moved`) and a new empty directory takes its name, as another
/// process's sweep and a new D of the same name could.
fn replace_d_after_lock(b: &Path, name: &OsStr) {
    let mut moved = name.to_os_string();
    moved.push(".moved");
    std::fs::rename(b.join(name), b.join(moved)).unwrap();
    std::fs::create_dir(b.join(name)).unwrap();
}

// sweep removes a dead D only while its name in B is still the directory it
// locked (hub SPEC-routes "D と E": B and D are listed through their fds, and
// the name's (st_dev, st_ino) is compared with the locked fd after the lock).
// A D replaced between the lock and the removal (here: moved aside and a new
// empty directory put at its name) is left: the new directory stays, and so
// does the D moved aside with its E.
#[test]
fn sweep_removes_a_d_only_while_its_name_is_the_one_locked() {
    let (t, place) = setup();
    bind_in_child(&nest(t.path(), 200, "s.sock"), &place);
    let b = b_of(&place);
    let d = entries(&b).pop().unwrap();
    base::AFTER_SWEEP_LOCK.set(Some(replace_d_after_lock));
    let swept = base::sweep(&[(Place::Tmp, place.clone())]);
    base::AFTER_SWEEP_LOCK.set(None);
    swept.unwrap();
    assert!(
        b.join(&d).is_dir(),
        "the directory put at D's name was removed"
    );
    assert_eq!(entries(&b.join(format!("{d}.moved"))).len(), 1);
}

// sweep removes the D of a process that ended (no lock held on it), with its
// E, and leaves: a live process's D (a child that binds and waits), this
// process's own D (the spelling the kernel keeps for its bind, read from
// getsockname, still reaches the socket after the sweep), a D holding
// something but symlinks (not one sockat left), an E directly in B (a
// connect's), and B itself.
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
    let mut own = bind(&nest(&t.path().join("own"), 200, "s.sock"), &opts(), &cfg).unwrap();
    let own = listener(&mut own);
    let foreign = b.join("notsockatD");
    std::fs::create_dir(&foreign).unwrap();
    std::fs::write(foreign.join("file"), b"").unwrap();
    std::os::unix::fs::symlink("/", b.join("connectE00")).unwrap();
    assert_eq!(entries(&b).len(), 5);

    base::sweep(&cfg.places).unwrap();
    assert_eq!(entries(&b).len(), 4, "{:?}", in_b(&b));
    assert!(foreign.join("file").exists());
    assert!(b.join("connectE00").is_symlink());
    ping(&own, UnixStream::connect(reported(&own)).unwrap().into());
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

// sweep_base does the same in an Options.base. A parent or B that is not
// there has nothing to sweep: success, nothing made (hub SPEC "sweep"). One
// that fails its check is that check's error, making nothing: a relative
// path is InvalidInput, a symlink EPERM, a file ENOTDIR, a B others can write
// to EPERM.
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
    base::sweep_base(&t.path().join("none/b")).unwrap();
    assert!(!t.path().join("none").exists());
    base::sweep_base(&t.path().join("nob")).unwrap();
    assert!(!t.path().join("nob").exists());

    let e = base::sweep_base(Path::new("rel/b")).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
    let link = t.path().join("link");
    std::os::unix::fs::symlink(&base, &link).unwrap();
    let e = base::sweep_base(&link).unwrap_err();
    assert_eq!(errno(&e), Some(libc::EPERM), "{e}");
    let file = t.path().join("file");
    std::fs::write(&file, b"").unwrap();
    let e = base::sweep_base(&file).unwrap_err();
    assert_eq!(errno(&e), Some(libc::ENOTDIR), "{e}");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o771)).unwrap();
    let e = base::sweep_base(&base).unwrap_err();
    assert_eq!(errno(&e), Some(libc::EPERM), "{e}");
}

// sweep and sweep_base take only the directories right under B whose name
// has the form of a D: 10 of the 62 letters and digits, as D's names are made
// (hub SPEC "sweep"). B is sockat's own, but what else is found in it is not
// sockat's to remove, even a directory that would otherwise pass for a dead
// D (no lock held on it, only symlinks or nothing in it): a name 1 shorter or
// longer, or with a character that is not a letter or a digit. The dead D of
// the form are removed. Checked in an Options.base (sweep_base) and in the B
// of a place (sweep).
#[test]
fn sweep_takes_only_names_of_the_form_of_a_d() {
    let not_d = [
        "abcdefghi",
        "abcdefghijk",
        "abcde-ghij",
        "abcde.ghij",
        "abcde_ghij",
        "abcde ghij",
        "empty",
    ];
    let d_form = ["deadDDDDDD", "0123456789", "zZ9aA0bB1c"];
    let plant = |b: &Path| {
        for name in not_d.iter().chain(&d_form) {
            let d = b.join(name);
            std::fs::create_dir(&d).unwrap();
            if *name != "empty" {
                std::os::unix::fs::symlink("/", d.join("0")).unwrap();
            }
        }
    };
    let mut want: Vec<String> = not_d.iter().map(|s| s.to_string()).collect();
    want.sort();

    let t = tmp();
    let given = t.path().join("b");
    std::fs::create_dir(&given).unwrap();
    plant(&given);
    base::sweep_base(&given).unwrap();
    assert_eq!(entries(&given), want);

    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let _own = bind(&nest(t.path(), 200, "s.sock"), &opts(), &cfg).unwrap();
    let b = b_of(&place);
    let own_d = entries(&b);
    assert_eq!(own_d.len(), 1);
    plant(&b);
    base::sweep(&cfg.places).unwrap();
    let mut left = want.clone();
    left.extend(own_d);
    left.sort();
    assert_eq!(entries(&b), left);
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

/// A `/proc` of another PID namespace or mount, standing in for one that
/// disagrees with this process: `<it>/self/fd/<n>` and `<it>/<pid>/fd/<n>`
/// for every small `n` are symlinks to `to`, whatever this process's fd `n`
/// is open on.
fn disagreeing_proc(at: &Path, to: &Path) -> PathBuf {
    for who in ["self".to_owned(), sys::process_id().to_string()] {
        let fds = at.join(who).join("fd");
        std::fs::create_dir_all(&fds).unwrap();
        for n in 0..512 {
            std::os::unix::fs::symlink(to, fds.join(n.to_string())).unwrap();
        }
    }
    at.to_path_buf()
}

// /proc is used only where `<proc>/self/fd/<fd>` names what the fd is open on
// (the same (st_dev, st_ino) by stat as by fstat; hub SPEC "用語"): a /proc
// of another PID namespace or mount may list another process's fds.
//
// - every `self/fd/<n>` leading to another directory, holding a socket of the
//   same name: the check of `/` that chooses the row fails, so the connect
//   takes the row without /proc, never proc-direct, and reaches the socket at
//   the path, not the other one
// - every `self/fd/<n>` leading to `/`: the row's check of `/` passes, and
//   proc-direct's own check of the socket's directory's fd (each call) fails,
//   so proc-direct is passed over (/proc unusable) and a later route reaches
//   the socket at the path
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn proc_direct_is_used_only_where_proc_names_the_fd() {
    let (t, place) = setup();
    let p = nest(&t.path().join("real"), 300, "s.sock");
    let l = listen_at(&p);
    l.set_nonblocking(true).unwrap();

    let other = t.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let decoy = UnixListener::bind(other.join("s.sock")).unwrap();
    decoy.set_nonblocking(true).unwrap();
    let cfg = Config {
        proc: disagreeing_proc(&t.path().join("p1"), &other),
        ..config(&place)
    };
    let c = connect(&p, &cfg).unwrap();
    assert_ne!(c.route, names::PROC_DIRECT);
    assert!(
        decoy.accept().is_err(),
        "the connect reached the other socket"
    );
    assert!(l.accept().is_ok());

    let cfg = Config {
        proc: disagreeing_proc(&t.path().join("p2"), Path::new("/")),
        ..config(&place)
    };
    let c = connect(&p, &cfg).unwrap();
    assert_ne!(c.route, names::PROC_DIRECT);
    assert!(
        c.skipped.contains(&route::Skipped {
            name: names::PROC_DIRECT,
            reason: names::PROC_UNUSABLE
        }),
        "{:?}",
        c.skipped
    );
    assert!(l.accept().is_ok());
}

// The estimate of a procB spelling's length, made before B is made, counts
// on procB only where `<proc>/<pid>/fd/<fd>` of a directory open now names
// what that fd is open on (hub SPEC-routes "綴りの形"). With a /proc that
// lists other fds, a file name that only a procB spelling would fit is known
// not to fit: no B is made in the place (one made would be of no use, its
// real path too long), and the resolve is ENAMETOOLONG with link-dir passed
// over as too long.
#[test]
fn the_estimate_counts_on_procb_only_where_proc_names_the_fd() {
    let t = tmp();
    let place = t.path().join("l".repeat(40));
    std::fs::create_dir(&place).unwrap();
    let other = t.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let cfg = Config {
        proc: disagreeing_proc(&t.path().join("p"), &other),
        ..no_link_file(config(&place))
    };
    let p = nest(&t.path().join("a"), 250, &name_just_past(&place));
    let f = route::resolve(route::Use::Bind, &p, &opts(), Some(&cfg))
        .err()
        .unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENAMETOOLONG));
    assert!(
        f.skipped.contains(&route::Skipped {
            name: names::LINK_DIR,
            reason: names::TOO_LONG
        }),
        "{:?}",
        f.skipped
    );
    assert!(entries(&place).is_empty(), "B made where no spelling fits");
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
    let (len, _) = sys::local_address(r.socket.as_ref().unwrap().as_fd()).unwrap();
    assert!(len <= 2 + DIRECT_MAX + 1, "{len}");
    let l = listener(&mut r);
    let c = connect(&p, &cfg).unwrap();
    assert_eq!(c.route, names::WIDENED);
    ping(&l, c.socket.unwrap());
}

// A connect's real failure is the result, and later routes are not tried
// (they would only fail the same way, after writing), for a socket nobody
// listens on and for nothing at the path. The errno is the OS's, passed on
// unchanged, so it is compared with what std's connect gives a short path in
// the same situation on this OS (ECONNREFUSED and ENOENT where they have been
// seen) rather than fixed. Nothing is left in B.
#[test]
fn a_connect_failure_is_not_retried() {
    let (t, place) = setup();
    let other = t.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let cfg = real_link_dir(&place);
    let p = nest(t.path(), 200, "s.sock");
    bind_in_child(&p, &other);
    let f = connect(&p, &cfg).err().unwrap();
    assert_eq!(
        errno(&f.error),
        Some(std_errno(Short::ConnectToNobodyListening))
    );
    assert!(f.route.is_some());
    let f = connect(&p.with_file_name("none.sock"), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(std_errno(Short::ConnectToNothing)));
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

// An empty path is an argument error (hub SPEC "短いパス"), refused at the
// entry of every call before std sees it: std's bind on Linux takes it as a
// request to autobind to an unnamed abstract address, a socket the caller
// did not name (and bind_ex then failed with ENOENT looking for its file),
// while macOS's kernel answers EINVAL. So every request (stream and datagram
// bind, blocking and nonblocking connect, datagram connect, with or without
// remembering the file) and both resolves give InvalidInput with no errno,
// no route and nothing skipped (no route was tried), and nothing is made in
// the place for B or at Options.base.
#[test]
fn an_empty_path_is_invalid_input_before_std() {
    let (t, place) = setup();
    let cfg = config(&place);
    let given = Options {
        base: Some(t.path().join("given")),
        ..opts()
    };
    let caller = UnixDatagram::unbound().unwrap();
    let requests = [
        BIND,
        Request::Bind(SockType::Datagram),
        CONNECT,
        Request::Connect(Connect::Stream { nonblocking: true }),
        Request::Connect(Connect::Datagram(&caller)),
    ];
    let empty = Path::new("");
    for options in [opts(), given.clone()] {
        for req in requests {
            for remember in [false, true] {
                let Err(f) = route::run(req, empty, &options, remember, Some(&cfg)) else {
                    panic!("{req:?}: an empty path was taken")
                };
                assert_eq!(f.error.kind(), io::ErrorKind::InvalidInput, "{req:?}");
                assert_eq!(f.error.raw_os_error(), None, "{req:?}: {}", f.error);
                assert_eq!(f.route, None, "{req:?}");
                assert!(f.skipped.is_empty(), "{req:?}");
            }
        }
        for for_ in [route::Use::Bind, route::Use::Connect] {
            let Err(f) = route::resolve(for_, empty, &options, Some(&cfg)) else {
                panic!("{for_:?}: an empty path was resolved")
            };
            assert_eq!(f.error.kind(), io::ErrorKind::InvalidInput, "{for_:?}");
            assert_eq!(f.error.raw_os_error(), None, "{for_:?}");
            assert_eq!(f.route, None, "{for_:?}");
            assert!(f.skipped.is_empty(), "{for_:?}");
        }
    }
    assert!(entries(&place).is_empty());
    assert!(!t.path().join("given").exists());
}

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

// The socket's directory is looked up first: a missing one, and a file in its
// place, fail before any route (none named, none skipped), with the errno a
// bind through the path would give: the one std's bind gives a short path in
// the same situation on this OS (ENOENT and ENOTDIR where they have been
// seen). Compared rather than fixed: the missing directory's errno is the
// OS's, passed on unchanged, and the file's is made to be what the OS's bind
// would answer.
#[test]
fn a_missing_directory_is_enoent_before_any_route() {
    let (t, place) = setup();
    let cfg = config(&place);
    let deep = nest(t.path(), 200, "x");
    let missing = deep.parent().unwrap().join("none/s.sock");
    let f = bind(&missing, &opts(), &cfg).err().unwrap();
    assert_eq!(
        errno(&f.error),
        Some(std_errno(Short::BindInAMissingDirectory))
    );
    assert!(f.skipped.is_empty() && f.route.is_none());
    std::fs::write(&deep, b"").unwrap();
    let f = bind(&deep.join("s.sock"), &opts(), &cfg).err().unwrap();
    assert_eq!(errno(&f.error), Some(std_errno(Short::BindUnderAFile)));
    assert!(f.skipped.is_empty() && f.route.is_none());
    assert!(entries(&place).is_empty());
}

// ---------------------------------------------------------------------------
// datagram
// ---------------------------------------------------------------------------

// Datagram sockets take the same routes: a receiver bound at a long path, a
// sender bound at a long path of its own and connected to the receiver by its
// long path. The receiver replies to the sender's address as the kernel gives
// it with the datagram (its kept spelling, read from `recvfrom` as the kernel
// wrote it rather than through std's `SocketAddr`, which drops the last byte
// on OpenBSD with Rust 1.99 and earlier, rust-lang/rust#118925), and the
// reply arrives: the spelling keeps naming the sender's socket file. A
// datagram connect leaves nothing and returns no new socket.
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
    let (n, from) = sys::recv_from(server.as_fd(), &mut buf).unwrap();
    assert_eq!(&buf[..n], b"ping");
    assert_eq!(from, sys::local_address(client.as_fd()).unwrap().1);
    server.send_to(b"pong", &from).unwrap();
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
        crate::Clean::new(r.link.clone()),
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
    let file = SocketFile::new(r.file.take().unwrap(), crate::Clean::new(r.link.clone()));
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
    let file = SocketFile::new(r.file.take().unwrap(), crate::Clean::new(r.link.clone()));
    std::fs::remove_file(&q).unwrap();
    std::fs::write(&q, b"someone else's").unwrap();
    file.remove().unwrap();
    assert_eq!(std::fs::read(&q).unwrap(), b"someone else's");
}

// On macOS and NetBSD a bind follows a symlink in the last component of the
// path and makes the socket where it points. What bind_ex remembers is then
// the socket the symlink led to (its real path and (st_dev, st_ino)), not the
// symlink: remove removes that socket and leaves the user's symlink (hub SPEC
// "remove": remove must not remove the user's symlink). Checked by every
// route a bind takes there: standard (a short path), link-dir, link-file,
// and thread-cwd (Apple).
#[cfg(any(target_vendor = "apple", target_os = "netbsd"))]
#[test]
fn remove_leaves_a_symlink_at_the_path_and_removes_where_it_led() {
    let (t, place) = setup();
    let real = t.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let mut cases = vec![
        (names::STANDARD, t.path().join("s.sock"), config(&place)),
        (
            names::LINK_DIR,
            nest(&t.path().join("ld"), 200, "s.sock"),
            real_link_dir(&place),
        ),
        (
            names::LINK_FILE,
            nest(&t.path().join("lf"), 400, &"n".repeat(200)),
            no_proc(config(&place)),
        ),
    ];
    if cfg!(target_vendor = "apple") && thread_here() {
        cases.push((
            names::THREAD_CWD,
            nest(&t.path().join("tc"), 250, &name_past_link_dir()),
            real_link_dir(&place),
        ));
    }
    for (i, (route, p, cfg)) in cases.into_iter().enumerate() {
        let target = real.join(format!("{i}.sock"));
        std::os::unix::fs::symlink(&target, &p).unwrap();
        let mut r = bind(&p, &opts(), &cfg).unwrap_or_else(|f| panic!("{route}: {}", f.error));
        assert_eq!(r.route, route);
        let _l = listener(&mut r);
        assert!(is_socket(&target), "{route}: the bind followed the symlink");
        let file = SocketFile::new(r.file.take().unwrap(), crate::Clean::new(r.link.clone()));
        assert_eq!(
            file.path(),
            std::fs::canonicalize(&target).unwrap(),
            "{route}"
        );
        file.remove().unwrap();
        assert!(
            std::fs::symlink_metadata(&target).is_err(),
            "{route}: the socket is left"
        );
        assert!(p.is_symlink(), "{route}: the symlink is removed");
    }
}

// ForBind's socket_file after the user's own bind through a path whose last
// component is a symlink (followed by the bind on macOS and NetBSD)
// remembers the socket the symlink led to, so its remove removes that socket
// and leaves the symlink; a symlink to what is not a socket is ENOTSOCK and
// nothing is remembered (hub SPEC "ForBind の SocketFile"). A short path (the
// spelling is the path) and a long one (link-dir).
#[cfg(any(target_vendor = "apple", target_os = "netbsd"))]
#[test]
fn socket_file_through_a_symlink_at_the_path_remembers_where_it_led() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let real = t.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let paths = [
        t.path().join("s.sock"),
        nest(&t.path().join("ld"), 200, "s.sock"),
    ];
    for (i, p) in paths.iter().enumerate() {
        let target = real.join(format!("{i}.sock"));
        std::os::unix::fs::symlink(&target, p).unwrap();
        let r = route::resolve(route::Use::Bind, p, &opts(), Some(&cfg)).unwrap();
        let _l = UnixListener::bind(&r.path).unwrap();
        assert!(is_socket(&target));
        let file = SocketFile::new(
            route::remember_resolved(&r.path, r.cwd).unwrap(),
            crate::Clean::new(r.link),
        );
        assert_eq!(file.path(), std::fs::canonicalize(&target).unwrap());
        file.remove().unwrap();
        assert!(std::fs::symlink_metadata(&target).is_err(), "{p:?}");
        assert!(p.is_symlink(), "{p:?}");

        let q = p.with_file_name("q.sock");
        let not_socket = real.join(format!("{i}.file"));
        std::fs::write(&not_socket, b"not a socket").unwrap();
        std::os::unix::fs::symlink(&not_socket, &q).unwrap();
        let r = route::resolve(route::Use::Bind, &q, &opts(), Some(&cfg)).unwrap();
        let e = route::remember_resolved(&r.path, r.cwd).unwrap_err();
        assert_eq!(errno(&e), Some(libc::ENOTSOCK), "{q:?}");
    }
}

// ---------------------------------------------------------------------------
// a path is not folded lexically
// ---------------------------------------------------------------------------

/// A path through a symlink and `..` under `r`: `<r>/a/sym` is a symlink to
/// `<r>/b/t`, so the kernel takes `<r>/a/sym/../<name>` to be `<r>/b/<name>`
/// (it follows `sym`, then climbs out of `b/t`), while folding `sym/..` on the
/// string would make it `<r>/a/<name>`. A listening socket (the decoy) is put
/// at `<r>/a/<name>`: what a folded path would hit.
struct AcrossSymlink {
    /// `<r>/a/sym/../<name>`.
    path: PathBuf,
    /// The real path of `<r>/b`.
    real_dir: PathBuf,
    /// The real path of `<r>/b/<name>`, where the socket must be.
    real: PathBuf,
    decoy_path: PathBuf,
    decoy_id: (u64, u64),
    decoy: UnixListener,
}

fn across_symlink(r: &Path, name: &str) -> AcrossSymlink {
    let target = r.join("b").join("t");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(r.join("a")).unwrap();
    let sym = r.join("a").join("sym");
    if std::fs::symlink_metadata(&sym).is_err() {
        std::os::unix::fs::symlink(&target, &sym).unwrap();
    }
    let decoy_path = r.join("a").join(name);
    let decoy = listen_at(&decoy_path);
    let real_dir = std::fs::canonicalize(r.join("b")).unwrap();
    AcrossSymlink {
        path: sym.join("..").join(name),
        real: real_dir.join(name),
        real_dir,
        decoy_id: file_id(&decoy_path),
        decoy_path,
        decoy,
    }
}

impl AcrossSymlink {
    /// The decoy is still the socket put there, and nothing connected to it.
    fn untouched(&self, route: &str) {
        assert_eq!(
            std::fs::symlink_metadata(&self.decoy_path)
                .ok()
                .map(|m| (m.dev(), m.ino())),
            Some(self.decoy_id),
            "{route}: the socket at a/<name> was removed or replaced"
        );
        self.decoy.set_nonblocking(true).unwrap();
        assert_eq!(
            self.decoy.accept().err().map(|e| e.kind()),
            Some(io::ErrorKind::WouldBlock),
            "{route}: a connect reached the socket at a/<name>"
        );
    }
}

fn file_id(p: &Path) -> (u64, u64) {
    let m = std::fs::symlink_metadata(p).unwrap();
    (m.dev(), m.ino())
}

// A path is never folded lexically (hub SPEC "長いパスの経路と試す順番": the
// directory is looked up by the OS). With `a/sym` a symlink to `b/t`, the
// kernel takes `a/sym/../<name>` to be `b/<name>`, and folding `sym/..` on the
// string makes it `a/<name>`, another file. On every route a bind takes here
// (standard for a short path; link-dir through B's real path and through
// procB; link-file; thread-cwd / at):
//
// - the socket is made in `b`
// - the E the spelling keeps leads to `b` (link-dir: E is the real path of
//   `b`; link-file: E resolves to `b/<name>`)
// - the socket file remembered for remove() is `b/<name>`, so remove()
//   removes it and leaves the socket at `a/<name>` (a remove that folded the
//   path removed another server's socket in another implementation)
// - a connect by the same path reaches the socket in `b`, not the one at
//   `a/<name>`
#[test]
fn a_bind_through_a_symlink_and_dot_dot_is_not_folded() {
    let (t, place) = setup();
    let long = |k: &str| t.path().join(k).join("x".repeat(100));
    let s = || "s.sock".to_owned();
    let mut cases = vec![
        (names::STANDARD, t.path().join("st"), s(), config(&place)),
        (names::LINK_DIR, long("ld"), s(), real_link_dir(&place)),
    ];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if proc_here() {
        cases.push((names::LINK_DIR, long("lp"), s(), config(&place)));
    }
    if cfg!(any(target_vendor = "apple", target_os = "netbsd")) {
        let cfg = no_proc(config(&place));
        cases.push((names::LINK_FILE, long("lf"), "n".repeat(200), cfg));
    }
    let in_d = if cfg!(target_os = "freebsd") {
        Some(names::AT)
    } else if thread_here() {
        Some(names::THREAD_CWD)
    } else {
        None
    };
    if let Some(route) = in_d {
        let cfg = real_link_dir(&place);
        cases.push((route, long("tc"), name_past_link_dir(), cfg));
    }
    for (route, r, name, cfg) in cases {
        let x = across_symlink(&r, &name);
        assert_eq!(
            path::len(&x.path) <= DIRECT_MAX,
            route == names::STANDARD,
            "{route}: the premise of the path's length"
        );
        let mut got =
            bind(&x.path, &opts(), &cfg).unwrap_or_else(|f| panic!("{route}: {}", f.error));
        assert_eq!(got.route, route);
        assert!(is_socket(&x.real), "{route}: no socket in b");
        let l = listener(&mut got);
        let spelling = reported(&l);
        if route == names::LINK_DIR {
            let e = spelling.parent().unwrap();
            assert_eq!(std::fs::read_link(e).unwrap(), x.real_dir, "{route}");
        }
        if route == names::LINK_FILE {
            let to = std::fs::read_link(&spelling).unwrap();
            assert_eq!(std::fs::canonicalize(to).unwrap(), x.real, "{route}");
        }
        let c = connect(&x.path, &cfg).unwrap();
        x.untouched(route);
        ping(&l, c.socket.unwrap());
        let file = SocketFile::new(
            got.file.take().unwrap(),
            crate::Clean::new(got.link.clone()),
        );
        assert_eq!(file.path(), x.real, "{route}");
        file.remove().unwrap();
        assert!(std::fs::symlink_metadata(&x.real).is_err(), "{route}");
        x.untouched(route);
    }
}

// The same for a connect (hub SPEC "長いパスの経路と試す順番"): every connect
// route forced here (proc-direct, thread-cwd-direct, widened, at-direct,
// link-file, and the spelling of resolve_for_connect handed to std) opens or
// spells the socket's directory as the kernel resolves `a/sym/..`, so it
// reaches the socket at `b/<name>`, not the one at `a/<name>`. relative is
// checked in a child (it needs the cwd near the path).
#[test]
fn a_connect_through_a_symlink_and_dot_dot_is_not_folded() {
    let (t, place) = setup();
    let x = across_symlink(&t.path().join("x".repeat(100)), "s.sock");
    assert!(path::len(&x.path) > DIRECT_MAX);
    let l = listen_at(&x.real);
    // link-file on every target, the others where the target has them (none
    // is pushed on NetBSD, OpenBSD and illumos).
    #[allow(unused_mut)]
    let mut cases = vec![(
        names::LINK_FILE,
        no_thread(no_direct(no_proc(config(&place)))),
    )];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if proc_here() {
        cases.push((names::PROC_DIRECT, config(&place)));
    }
    #[cfg(target_vendor = "apple")]
    cases.push((names::WIDENED, config(&place)));
    #[cfg(target_os = "freebsd")]
    cases.push((names::AT_DIRECT, config(&place)));
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    if thread_here() {
        #[allow(unused_mut)]
        let mut cfg = no_proc(config(&place));
        #[cfg(target_vendor = "apple")]
        {
            cfg.wide = false;
        }
        cases.push((names::THREAD_CWD_DIRECT, cfg));
    }
    for (route, cfg) in cases {
        let c = connect(&x.path, &cfg).unwrap_or_else(|f| panic!("{route}: {}", f.error));
        assert_eq!(c.route, route);
        x.untouched(route);
        ping(&l, c.socket.unwrap());
    }
    let rc = route::resolve(route::Use::Connect, &x.path, &opts(), Some(&config(&place))).unwrap();
    assert_eq!(rc.route, names::LINK_FILE);
    let c = UnixStream::connect(&rc.path).unwrap();
    x.untouched("resolve_for_connect");
    ping(&l, c.into());
}

// A relative path through a symlink and `..` is not folded either (hub SPEC
// "長いパスの経路と試す順番"), with the cwd at `<r>`:
//
// - a short `a/sym/../s.sock` goes to std as it is (standard, CwdRelative):
//   the socket is made in `b`, and remove() removes `b/s.sock` and leaves the
//   socket at `a/s.sock` (a short relative remove that folded the path
//   removed another server's socket in another implementation)
// - resolve_for_bind hands a short one back as it is, and its socket_file()
//   remembers the socket in `b`
// - relative (not on FreeBSD) spells a long absolute path below the cwd as
//   `a/sym/../c.sock`, which reaches the socket in `b`
//
// Run in a child: the cwd is process-wide.
#[test]
fn a_relative_path_through_a_symlink_and_dot_dot_is_not_folded() {
    let t = tmp();
    let place = t.path().join("p");
    std::fs::create_dir(&place).unwrap();
    let r = t.path().join("x".repeat(100));
    std::fs::create_dir(&r).unwrap();
    run_child(
        "tests::child_relative_path_through_a_symlink_and_dot_dot",
        &[
            ("SOCKAT_CHILD_PATH", r.as_os_str()),
            ("SOCKAT_CHILD_PLACE", place.as_os_str()),
        ],
    );
}

#[test]
fn child_relative_path_through_a_symlink_and_dot_dot() {
    if !in_child() {
        return;
    }
    let r = std::fs::canonicalize(env_path("SOCKAT_CHILD_PATH")).unwrap();
    let place = env_path("SOCKAT_CHILD_PLACE");
    std::env::set_current_dir(&r).unwrap();

    let x = across_symlink(&r, "s.sock");
    let rel = Path::new("a/sym/../s.sock");
    let mut got = bind(rel, &opts(), &config(&place)).unwrap();
    assert_eq!(got.route, names::STANDARD);
    assert_eq!(got.spelling, Some(Spelling::CwdRelative));
    assert!(is_socket(&x.real));
    let l = listener(&mut got);
    let file = SocketFile::new(
        got.file.take().unwrap(),
        crate::Clean::new(got.link.clone()),
    );
    assert_eq!(file.path(), x.real);
    file.remove().unwrap();
    assert!(std::fs::symlink_metadata(&x.real).is_err());
    x.untouched("standard");
    drop(l);

    let y = across_symlink(&r, "s2.sock");
    let rel = Path::new("a/sym/../s2.sock");
    let resolved = route::resolve(route::Use::Bind, rel, &opts(), Some(&config(&place))).unwrap();
    assert_eq!(resolved.path, rel);
    let _l = UnixListener::bind(&resolved.path).unwrap();
    let route::Kept::File(remembered) =
        route::remember_resolved(&resolved.path, resolved.cwd).unwrap()
    else {
        panic!("no file remembered")
    };
    assert_eq!(remembered.path, y.real);
    y.untouched("resolve_for_bind");

    #[cfg(not(target_os = "freebsd"))]
    {
        let z = across_symlink(&r, "c.sock");
        assert!(path::len(&z.path) > DIRECT_MAX);
        let l = listen_at(&z.real);
        let cfg = no_proc(config(&place));
        let c = connect(&z.path, &cfg).unwrap();
        assert_eq!(c.route, names::RELATIVE);
        z.untouched(names::RELATIVE);
        ping(&l, c.socket.unwrap());
    }
}

thread_local! {
    /// The file the [`replace_after_bind`] hook puts in the socket's place.
    // The initializer is `const`; clippy on illumos says otherwise of the
    // macro's expansion there.
    #[allow(clippy::missing_const_for_thread_local)]
    static REPLACED: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// [`route::AFTER_BIND`] for the tests: the socket file at [`REPLACED`] is
/// replaced by a regular file right after the bind, as another process could.
fn replace_after_bind() {
    REPLACED.with_borrow(|p| {
        let p = p.as_ref().unwrap();
        std::fs::remove_file(p).unwrap();
        std::fs::write(p, b"not a socket").unwrap();
    });
}

// What a bind_ex finds where the bind made the socket must be a socket: one
// replaced right after the bind by something else (a regular file) is not
// remembered, so remove cannot remove it (hub SPEC "remove": what the symlink
// led to is not a socket, treated as not remembered). The call fails
// (ENOTSOCK), the listener is closed, E is removed, and the replacement and
// the user's symlink are left. Checked for a symlink at the path on macOS and
// NetBSD (where the bind follows it), and for the socket file itself on
// every target, by each route a bind takes here.
#[test]
fn a_bind_ex_that_finds_no_socket_where_it_bound_fails() {
    let (t, place) = setup();
    let real = t.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let mut cases = vec![
        (names::STANDARD, t.path().join("s.sock"), config(&place)),
        (
            names::LINK_DIR,
            nest(&t.path().join("ld"), 200, "s.sock"),
            real_link_dir(&place),
        ),
    ];
    if cfg!(any(target_vendor = "apple", target_os = "netbsd")) {
        cases.push((
            names::LINK_FILE,
            nest(&t.path().join("lf"), 400, &"n".repeat(200)),
            no_proc(config(&place)),
        ));
    }
    if cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple"
    )) && thread_here()
    {
        cases.push((
            names::THREAD_CWD,
            nest(&t.path().join("tc"), 250, &name_past_link_dir()),
            real_link_dir(&place),
        ));
    }
    let follows = cfg!(any(target_vendor = "apple", target_os = "netbsd"));
    for (i, (route, p, cfg)) in cases.into_iter().enumerate() {
        for through_symlink in [false, true] {
            if through_symlink && !follows {
                continue;
            }
            let p = match through_symlink {
                false => p.clone(),
                true => p.with_file_name(format!("l{}", p.file_name().unwrap().to_str().unwrap())),
            };
            let at = match through_symlink {
                false => p.clone(),
                true => {
                    let target = real.join(format!("{i}.sock"));
                    std::os::unix::fs::symlink(&target, &p).unwrap();
                    target
                }
            };
            REPLACED.set(Some(at.clone()));
            route::AFTER_BIND.set(Some(replace_after_bind));
            let f = bind(&p, &opts(), &cfg).err();
            route::AFTER_BIND.set(None);
            let f = f.unwrap_or_else(|| panic!("{route} {through_symlink}: bound"));
            assert_eq!(errno(&f.error), Some(libc::ENOTSOCK), "{route}");
            assert_eq!(f.route, Some(route));
            assert_eq!(std::fs::read(&at).unwrap(), b"not a socket", "{route}");
            if through_symlink {
                assert!(p.is_symlink(), "{route}");
            }
            assert_no_e(&b_of(&place));
        }
    }
}

/// `f` with every removal of an E on this thread failing (`EPERM`, the E
/// left), as the system could refuse it ([`base::FAIL_UNLINK_E`]).
fn with_e_unremovable<T>(f: impl FnOnce() -> T) -> T {
    base::FAIL_UNLINK_E.set(true);
    let out = f();
    base::FAIL_UNLINK_E.set(false);
    out
}

/// The E left in `b`: directly in B (the plain connect's) and in its D.
fn e_left(b: &Path) -> usize {
    entries(b)
        .iter()
        .map(|name| match b.join(name) {
            e if e.is_symlink() => 1,
            d => entries(&d).len(),
        })
        .sum()
}

// A removal of E made inside a call that fails is passed over in silence: it
// changes neither whether the call succeeds nor the error it returns (hub
// SPEC-routes "経路の失敗の扱い": a failure to remove must not turn a
// successful connect into a failure, nor replace the original error). Each
// such removal: the plain connect's link-file E after a connect that
// succeeded and after one that failed (nobody listening), the E of a bind
// that failed for real (a file at the path) through link-dir, link-file
// (macOS, NetBSD) and thread-cwd (Linux, Apple), the thread-cwd E after a
// bind that succeeded, and the E of a bind_ex that could not remember the
// socket file (ENOTSOCK, sockat's own). The original error of a connect or a
// bind is the OS's, passed on unchanged, so it is compared with what std
// gives a short path in the same situation on this OS, not fixed (the
// removal's error is EPERM, from the test hook). Each E is still there
// afterwards, so the removal did fail.
#[test]
fn a_failed_removal_of_e_inside_a_call_changes_nothing() {
    let (t, place) = setup();
    let b = b_of(&place);

    let cfg = no_thread(no_direct(no_proc(config(&place))));
    let p = nest(&t.path().join("c"), 300, &"n".repeat(100));
    let l = listen_at(&p);
    let c = with_e_unremovable(|| connect(&p, &cfg)).unwrap();
    assert_eq!(c.route, names::LINK_FILE);
    ping(&l, c.socket.unwrap());
    assert_eq!(e_left(&b), 1);
    // The socket nobody listens on is bound by a child that has exited (its
    // B in a place of its own): a listener dropped here could still be held,
    // and listen, in a child another test is starting meanwhile (between its
    // fork and its exec).
    let other = t.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let gone = nest(&t.path().join("g"), 200, "s.sock");
    bind_in_child(&gone, &other);
    let f = with_e_unremovable(|| connect(&gone, &cfg)).err().unwrap();
    assert_eq!(
        errno(&f.error),
        Some(std_errno(Short::ConnectToNobodyListening))
    );
    assert_eq!(f.route, Some(names::LINK_FILE));
    assert_eq!(e_left(&b), 2);

    let mut cases = vec![(
        names::LINK_DIR,
        nest(&t.path().join("ld"), 200, "s.sock"),
        real_link_dir(&place),
    )];
    if cfg!(any(target_vendor = "apple", target_os = "netbsd")) {
        cases.push((
            names::LINK_FILE,
            nest(&t.path().join("lf"), 400, &"n".repeat(200)),
            no_proc(config(&place)),
        ));
    }
    let thread = cfg!(any(
        target_os = "linux",
        target_os = "android",
        target_vendor = "apple"
    )) && thread_here();
    if thread {
        cases.push((
            names::THREAD_CWD,
            nest(&t.path().join("tc"), 250, &name_past_link_dir()),
            real_link_dir(&place),
        ));
    }
    let onto_a_file = std_errno(Short::BindOntoAFile);
    for (route, p, cfg) in &cases {
        let before = e_left(&b);
        std::fs::write(p, b"").unwrap();
        let f = with_e_unremovable(|| bind(p, &opts(), cfg)).err().unwrap();
        assert_eq!(errno(&f.error), Some(onto_a_file), "{route}");
        assert_eq!(f.route, Some(*route));
        assert_eq!(e_left(&b), before + 1, "{route}");
        std::fs::remove_file(p).unwrap();
    }
    if thread {
        let p = nest(&t.path().join("tc2"), 250, &name_past_link_dir());
        let before = e_left(&b);
        let mut r = with_e_unremovable(|| bind(&p, &opts(), &real_link_dir(&place))).unwrap();
        assert_eq!(r.route, names::THREAD_CWD);
        assert!(r.file.is_some());
        let l = listener(&mut r);
        ping(
            &l,
            connect(&p, &real_link_dir(&place)).unwrap().socket.unwrap(),
        );
        assert_eq!(e_left(&b), before + 1);
    }

    let p = nest(&t.path().join("ns"), 200, "s.sock");
    let before = e_left(&b);
    REPLACED.set(Some(p.clone()));
    route::AFTER_BIND.set(Some(replace_after_bind));
    let f = with_e_unremovable(|| bind(&p, &opts(), &real_link_dir(&place))).err();
    route::AFTER_BIND.set(None);
    let f = f.unwrap();
    assert_eq!(errno(&f.error), Some(libc::ENOTSOCK));
    assert_eq!(f.route, Some(names::LINK_DIR));
    assert_eq!(e_left(&b), before + 1);
}

// The cleanup() a user calls is not silent: a failure to remove E (other
// than E being gone already) is its error, telling the user that what they
// asked for was not done (hub SPEC "remove と cleanup"). A later call tries
// again, and once E is removed later calls do nothing. remove() returns the
// error of removing the socket file first, still doing the cleanup, and the
// cleanup's error when the file was removed. ForBind / ForConnect /
// SocketFile share the one cleanup of their E.
#[test]
fn a_cleanup_the_user_calls_returns_the_failure_to_remove_e() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let b = b_of(&place);

    let p = nest(&t.path().join("r"), 200, "s.sock");
    let r = route::resolve(route::Use::Connect, &p, &opts(), Some(&cfg)).unwrap();
    let link = r.link.clone().unwrap();
    let unlinks = || link.unlinks.load(std::sync::atomic::Ordering::Relaxed);
    let for_connect = crate::ForConnect {
        path: r.path,
        route: r.route,
        skipped: r.skipped,
        clean: crate::Clean::new(r.link),
    };
    assert_eq!(e_left(&b), 1);
    let e = with_e_unremovable(|| for_connect.cleanup()).unwrap_err();
    assert_eq!(errno(&e), Some(libc::EPERM));
    assert_eq!(e_left(&b), 1);
    for_connect.cleanup().unwrap();
    assert_eq!(e_left(&b), 0);
    assert_eq!(unlinks(), 2);
    for_connect.cleanup().unwrap();
    assert_eq!(unlinks(), 2, "a call after the success does nothing");

    // remove(): the file removed, E not: the cleanup's error; then both done.
    let p = nest(&t.path().join("s"), 200, "s.sock");
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    let _l = listener(&mut r);
    let file = SocketFile::new(r.file.take().unwrap(), crate::Clean::new(r.link.clone()));
    let link = r.link.clone().unwrap();
    let unlinks = || link.unlinks.load(std::sync::atomic::Ordering::Relaxed);
    let e = with_e_unremovable(|| file.remove()).unwrap_err();
    assert_eq!(errno(&e), Some(libc::EPERM));
    assert!(!p.exists());
    assert_eq!(e_left(&b), 1);
    file.remove().unwrap();
    assert_eq!(e_left(&b), 0);
    assert_eq!(unlinks(), 2);
    let _again = listen_at(&p);
    file.remove().unwrap();
    assert!(is_socket(&p), "a call after the success does nothing");
    assert_eq!(unlinks(), 2);

    // remove(): neither the file nor E removed: the file's error (the
    // socket's directory not writable), the cleanup still tried. The file's
    // error is the OS's, passed on unchanged, so it is compared with what
    // std's remove_file gives a short path in the same situation on this OS
    // (EACCES where it has been seen), not fixed; it must differ from the
    // cleanup's (EPERM, from the test hook), or which of the two came back
    // could not be told. Not as root, whom the mode does not stop.
    if sys::effective_uid() == 0 {
        return;
    }
    let unwritable = std_errno(Short::RemoveFromAnUnwritableDirectory);
    assert_ne!(unwritable, libc::EPERM, "the file's error is the cleanup's");
    let p = nest(&t.path().join("u"), 200, "s.sock");
    let mut r = bind(&p, &opts(), &cfg).unwrap();
    let _l = listener(&mut r);
    let file = SocketFile::new(r.file.take().unwrap(), crate::Clean::new(r.link.clone()));
    let dir = p.parent().unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let e = with_e_unremovable(|| file.remove()).unwrap_err();
    assert_eq!(errno(&e), Some(unwritable));
    assert_eq!(e_left(&b), 1);
    // The file still not removable, E now is: the file's error, E removed.
    let e = file.remove().unwrap_err();
    assert_eq!(errno(&e), Some(unwritable));
    assert_eq!(e_left(&b), 0);
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    file.remove().unwrap();
    assert!(!p.exists());
}

// cleanup() and remove() called at once from many threads remove E once:
// one unlink of E, every call Ok, E gone (hub SPEC "remove と cleanup": the
// removal is one at a time even when called concurrently). After that a call
// does nothing (no further unlink). Repeated over many E, each with its
// callers released together, so that calls that are not one at a time would
// meet.
#[test]
fn concurrent_cleanups_unlink_e_once() {
    let (t, place) = setup();
    let cfg = real_link_dir(&place);
    let b = b_of(&place);
    let threads = 8;
    for i in 0..64 {
        let p = nest(&t.path().join(format!("{i}")), 200, "s.sock");
        let r = route::resolve(route::Use::Bind, &p, &opts(), Some(&cfg)).unwrap();
        let link = r.link.unwrap();
        let _l = UnixListener::bind(&r.path).unwrap();
        let file = SocketFile::new(
            route::remember_resolved(&r.path, None).unwrap(),
            crate::Clean::new(Some(link.clone())),
        );
        let barrier = std::sync::Barrier::new(threads);
        std::thread::scope(|s| {
            for k in 0..threads {
                let (barrier, file) = (&barrier, &file);
                s.spawn(move || {
                    barrier.wait();
                    match k % 2 {
                        0 => file.remove().unwrap(),
                        _ => file.cleanup().unwrap(),
                    }
                });
            }
        });
        assert_eq!(
            link.unlinks.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "E {i}"
        );
        assert!(!p.exists());
        assert_eq!(e_left(&b), 0);
        file.cleanup().unwrap();
        file.remove().unwrap();
        assert_eq!(link.unlinks.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}

// The SocketFiles one ForBind gives (socket_file() called more than once)
// and the ForBind itself share one lock for removing the socket file and E
// (hub SPEC "remove と cleanup": one at a time even when called at once).
// Each SocketFile removed from its own thread at once, with the ForBind's
// cleanup beside them: the socket file is unlinked once (the others find it
// gone: Ok), and E once. Two removals of the file not one at a time could
// both find it still the one bound and both unlink it, the second removing a
// socket bound at the path in between. Repeated over many paths, the
// callers of each released together.
#[test]
fn removals_through_one_for_bind_are_one_at_a_time() {
    let t = tmp();
    let options = Options {
        base: Some(t.path().join("b")),
        ..opts()
    };
    let threads = 8;
    for i in 0..64 {
        let p = nest(&t.path().join(format!("{i}")), 200, "s.sock");
        let for_bind = crate::resolve_for_bind(&p, options.clone()).unwrap();
        let _l = UnixListener::bind(&for_bind.path).unwrap();
        let files: Vec<SocketFile> = (0..threads)
            .map(|_| for_bind.socket_file().unwrap())
            .collect();
        let barrier = std::sync::Barrier::new(threads + 1);
        std::thread::scope(|s| {
            for file in &files {
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    file.remove().unwrap();
                });
            }
            s.spawn(|| {
                barrier.wait();
                for_bind.cleanup().unwrap();
            });
        });
        let relaxed = std::sync::atomic::Ordering::Relaxed;
        assert_eq!(for_bind.clean.file_unlinks.load(relaxed), 1, "file {i}");
        let link = for_bind.clean.link.as_ref().unwrap();
        assert_eq!(link.unlinks.load(relaxed), 1, "E {i}");
        assert!(!p.exists());
    }
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
    // The listener was closed: the connect meets a socket file nobody listens
    // on, and fails with what std's connect gives a short path in that
    // situation on this OS (the OS's errno, passed on unchanged).
    assert_eq!(
        errno(&e.error),
        Some(std_errno(Short::ConnectToNobodyListening)),
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
