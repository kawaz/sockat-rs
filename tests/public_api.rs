//! The public API as a user sees it: paths of any length on whatever route
//! the platform picks. Which route ran is looked at only through the
//! diagnostics the `_ex` functions return; the route-forcing tests live in
//! the crate's unit tests.
//!
//! The functions without options use the environment's own places for B
//! (`/tmp/sockat-<uid>` first), which the tests of every sockat
//! implementation share: there a test only adds its own D (held by its lock)
//! and its own E, and never removes B, relies on what else is in it, or
//! counts it. The `_ex` and resolve functions are given a B of the test's own
//! ([`own_base`]).

use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use sockat::{Options, Spelling};

/// `<base>/<dirs>/<name>` of exactly `total` bytes, directories created.
fn nest(base: &Path, total: usize, name: &str) -> PathBuf {
    let fixed = base.as_os_str().len() + 1 + name.len();
    assert!(total >= fixed);
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
    assert_eq!(p.as_os_str().as_bytes().len(), total);
    p
}

/// The longest path std accepts on this target, `sun_path` less the NUL (107
/// on Linux, 103 on macOS): sockat hands a path up to this length to std and
/// takes its own routes past it.
fn std_max() -> usize {
    fn sun_path_len<const N: usize>(_: fn(&libc::sockaddr_un) -> &[libc::c_char; N]) -> usize {
        N
    }
    sun_path_len(|un| &un.sun_path) - 1
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("srt-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// Options with a B of this test's own, `<t>/b`, so the test neither leans on
/// nor changes the B the tests of every sockat implementation share.
fn own_base(t: &tempfile::TempDir) -> Options {
    Options {
        base: Some(t.path().join("b")),
        ..Default::default()
    }
}

fn is_socket(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_socket())
}

fn ping(listener: &UnixListener, client: &mut UnixStream) {
    let (mut server, _) = listener.accept().unwrap();
    client.write_all(b"ping").unwrap();
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    server.write_all(b"pong").unwrap();
    client.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"pong");
}

/// A situation in which sockat returns the error the OS gave, as std meets it
/// at a short path ([`std_errno`]).
#[derive(Clone, Copy, Debug)]
enum Short {
    /// A stream bind whose directory is missing.
    BindInAMissingDirectory,
    /// A stream connect whose directory is missing.
    ConnectInAMissingDirectory,
    /// A stream bind with a regular file at the path.
    BindOntoAFile,
    /// A stream connect to a socket file nobody listens on.
    ConnectToNobodyListening,
    /// A datagram connect to a socket file no socket holds (its socket bound
    /// and closed).
    DatagramConnectToNobodyHolding,
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
    let missing = t.path().join("none/s.sock");
    let r = match case {
        Short::BindInAMissingDirectory => UnixListener::bind(&missing).map(drop),
        Short::ConnectInAMissingDirectory => UnixStream::connect(&missing).map(drop),
        Short::BindOntoAFile => {
            std::fs::write(&p, b"").unwrap();
            UnixListener::bind(&p).map(drop)
        }
        Short::ConnectToNobodyListening => {
            drop(UnixListener::bind(&p).unwrap());
            UnixStream::connect(&p).map(drop)
        }
        Short::DatagramConnectToNobodyHolding => {
            drop(UnixDatagram::bind(&p).unwrap());
            UnixDatagram::unbound().unwrap().connect(&p)
        }
    };
    let Err(e) = r else {
        panic!("{case:?}: std succeeded at a short path")
    };
    e.raw_os_error()
        .unwrap_or_else(|| panic!("{case:?}: std's error has no errno: {e}"))
}

/// Whether this std drops the last byte of the path when it reads a socket
/// address (`local_addr`, `peer_addr`, `recv_from`): Rust 1.99 and earlier
/// do on OpenBSD (rust-lang/rust#118925, fixed in 1.100.0). Measured rather
/// than told by the target or the version: std binds a short path, and the
/// address its own `local_addr()` reads is compared with that path. Anything
/// but the path or the path less its last byte fails, as a std this test
/// does not know.
fn std_drops_last_byte() -> bool {
    let t = tmp();
    let p = t.path().join("s.sock");
    let l = UnixListener::bind(&p).unwrap();
    let read = l.local_addr().unwrap();
    let read = read.as_pathname().unwrap();
    if read == p {
        return false;
    }
    let bytes = p.as_os_str().as_bytes();
    assert_eq!(
        read.as_os_str().as_bytes(),
        &bytes[..bytes.len() - 1],
        "std reads {read:?} for {p:?}"
    );
    true
}

/// Connect through `listener`'s spelling, as std's `local_addr()` reads it,
/// and exchange a message. Where std drops the last byte of what it reads
/// ([`std_drops_last_byte`]) that reading names nothing whatever sockat
/// does, so the step is left out with the reason on stderr, naming
/// `kernel_check`, the test in src/tests.rs that connects through the
/// spelling read by getsockname.
fn ping_through_spelling(listener: &UnixListener, kernel_check: &str) {
    if std_drops_last_byte() {
        eprintln!(
            "connect through the spelling not tried: this std drops the last byte of the \
             addresses it reads on OpenBSD (rust-lang/rust#118925, fixed in Rust 1.100.0); \
             checked with getsockname by {kernel_check} in src/tests.rs"
        );
        return;
    }
    let mut c = UnixStream::connect(reported(listener)).unwrap();
    ping(listener, &mut c);
}

fn reported(listener: &UnixListener) -> PathBuf {
    listener
        .local_addr()
        .unwrap()
        .as_pathname()
        .unwrap()
        .to_path_buf()
}

// The promise of the crate: a socket can be bound and reached at a path of any
// length (well past sun_path's 104 / 108 bytes), the socket file is at that
// path, and it can be removed by that path. Both sides of the length std
// accepts are included (the last length handed to std and the first one
// past it), and a nonblocking connect too. Two bases: the default temporary
// directory (on macOS $TMPDIR alone is about 50 bytes, longer under
// sandboxes and test harnesses) and /tmp.
#[test]
fn bind_and_connect_work_at_any_length() {
    let bases = [tempfile::tempdir().unwrap(), tmp()];
    let max = std_max();
    for base in &bases {
        for total in [100, max, max + 1, 108, 200, 253, 254, 400, 1000] {
            let total = total.max(base.path().as_os_str().len() + 10);
            let p = nest(base.path(), total, "s.sock");
            let listener = sockat::bind(&p).unwrap();
            assert!(is_socket(&p), "{total}: {p:?}");
            let mut client = sockat::connect(&p).unwrap();
            ping(&listener, &mut client);
            let mut client = sockat::connect_nonblocking(&p).unwrap();
            client.set_nonblocking(false).unwrap();
            ping(&listener, &mut client);
            // Closed before the unlink: on illumos an unlink followed by the
            // close can make a concurrent bind fail with EADDRINUSE (a race in
            // the kernel).
            drop((client, listener));
            std::fs::remove_file(&p).unwrap();
            assert!(!is_socket(&p));
        }
    }
}

// Short paths are std's business: sockat hands them over unchanged, so the
// addresses the sockets report (local_addr, peer_addr) are exactly what std
// reports for the same path. Compared with std's own answer rather than with
// the path: std's answer is not always the path (on OpenBSD the local_addr
// of Rust 1.99 and earlier drops the last byte of it, rust-lang/rust#118925),
// and "the same as std" is the promise.
#[test]
fn a_short_path_behaves_exactly_like_std() {
    let t = tmp();
    let p = t.path().join("s.sock");
    let addr = |a: io::Result<std::os::unix::net::SocketAddr>| {
        a.unwrap().as_pathname().map(Path::to_path_buf)
    };
    let (std_local, std_peer, std_accepted) = {
        let listener = UnixListener::bind(&p).unwrap();
        let client = UnixStream::connect(&p).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let addrs = (
            addr(listener.local_addr()),
            addr(client.peer_addr()),
            addr(accepted.local_addr()),
        );
        drop((listener, client, accepted));
        std::fs::remove_file(&p).unwrap();
        addrs
    };
    let listener = sockat::bind(&p).unwrap();
    assert_eq!(addr(listener.local_addr()), std_local);
    let client = sockat::connect(&p).unwrap();
    assert_eq!(addr(client.peer_addr()), std_peer);
    let (accepted, _) = listener.accept().unwrap();
    assert_eq!(addr(accepted.local_addr()), std_accepted);
    drop((client, accepted));
    let mut std_client = UnixStream::connect(&p).unwrap();
    ping(&listener, &mut std_client);
}

// The returned types are plain std sockets: a long-path listener accepts a
// client from another thread, and the streams can be cloned and shut down as
// usual. Nothing about the route leaks into the types.
#[test]
fn the_results_are_ordinary_std_sockets() {
    let t = tmp();
    let p = nest(t.path(), 500, "s.sock");
    let listener = sockat::bind(&p).unwrap();
    let pc = p.clone();
    let h = std::thread::spawn(move || {
        let mut c = sockat::connect(&pc).unwrap();
        c.write_all(b"hello").unwrap();
        c.shutdown(std::net::Shutdown::Write).unwrap();
        let mut s = String::new();
        c.read_to_string(&mut s).unwrap();
        s
    });
    let (mut s, _) = listener.accept().unwrap();
    let mut got = String::new();
    s.try_clone().unwrap().read_to_string(&mut got).unwrap();
    assert_eq!(got, "hello");
    s.write_all(b"bye").unwrap();
    drop(s);
    assert_eq!(h.join().unwrap(), "bye");
}

// Many threads binding and connecting long paths at once, while other threads
// keep resolving a path relative to the cwd: no call may change the cwd of the
// process (other threads' relative lookups would break), and every socket must
// land at its own path.
#[test]
fn concurrent_calls_leave_the_cwd_alone() {
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
                let t = tmp();
                for round in 0..20 {
                    let total = [120, 250, 400, 900][(i + round) % 4];
                    let p = nest(t.path(), total, &format!("{round}.sock"));
                    let listener = sockat::bind(&p).unwrap();
                    assert!(is_socket(&p));
                    let mut client = sockat::connect(&p).unwrap();
                    ping(&listener, &mut client);
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
}

// The documented errors for a long path, through the public API: a missing
// directory, an existing file and a socket nobody listens on fail as std
// fails at a short path, with the same errno on this OS (ENOENT, EADDRINUSE
// and ECONNREFUSED where they have been seen; the OS's errno is passed on
// unchanged and OSes may differ, so it is compared, not fixed). A path
// without a file name or with a NUL byte is InvalidInput, sockat's own.
#[test]
fn long_path_errors_through_the_public_api() {
    let t = tmp();
    let p = nest(t.path(), 300, "s.sock");
    let missing = p.parent().unwrap().join("none/s.sock");
    let e = sockat::bind(&missing).unwrap_err();
    assert_eq!(
        e.raw_os_error(),
        Some(std_errno(Short::BindInAMissingDirectory))
    );
    let e = sockat::connect(&missing).unwrap_err();
    assert_eq!(
        e.raw_os_error(),
        Some(std_errno(Short::ConnectInAMissingDirectory))
    );
    std::fs::write(&p, b"").unwrap();
    let e = sockat::bind(&p).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(std_errno(Short::BindOntoAFile)));
    std::fs::remove_file(&p).unwrap();
    let e = sockat::bind(p.parent().unwrap().join("..")).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    let mut nul = p.as_os_str().as_bytes().to_vec();
    nul.insert(nul.len() - 2, 0);
    let nul = PathBuf::from(std::ffi::OsStr::from_bytes(&nul));
    assert_eq!(
        sockat::connect(&nul).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    // A socket nobody listens on: bound by std at a short path in another
    // process's place would do; here, a listener dropped before the connect.
    let q = nest(&t.path().join("q"), 300, "q.sock");
    drop(sockat::bind(&q).unwrap());
    let e = sockat::connect(&q).unwrap_err();
    assert_eq!(
        e.raw_os_error(),
        Some(std_errno(Short::ConnectToNobodyListening))
    );
}

// The default binds (`bind`, `datagram::bind`) in one directory go through
// one E (hub SPEC-routes "link-dir"): the spellings the kernel keeps for them
// share everything but the file name, so a process binding many sockets in
// one directory leaves one E there, not one per bind. bind_ex makes an E of
// its own (its cleanup removes that one alone), so its spelling goes through
// another. Only the part before the file name is compared: std on OpenBSD
// before Rust 1.100 cuts the last byte of what it reads.
#[test]
fn default_binds_in_one_directory_share_their_e() {
    let t = tmp();
    let dir = nest(t.path(), 200, "x").parent().unwrap().to_path_buf();
    let a = sockat::bind(dir.join("a.sock")).unwrap();
    let b = sockat::bind(dir.join("b.sock")).unwrap();
    let d = sockat::datagram::bind(dir.join("d.sock")).unwrap();
    let through = |p: PathBuf| p.parent().unwrap().to_path_buf();
    let shared = through(reported(&a));
    assert_eq!(through(reported(&b)), shared);
    let dgram = d.local_addr().unwrap().as_pathname().unwrap().to_path_buf();
    assert_eq!(through(dgram), shared);
    let ex = sockat::bind_ex(dir.join("e.sock"), Default::default()).unwrap();
    assert_ne!(through(reported(&ex.socket)), shared);
    ex.file.remove().unwrap();
    ping_through_spelling(&a, "default_binds_share_one_link_dir_e_per_directory");
}

// An empty path is an argument error through every public function (hub
// SPEC "短いパス"), whatever the OS: InvalidInput with no errno, before std
// sees it (std's bind on Linux would autobind to an unnamed abstract address,
// a socket the caller did not name; macOS's kernel answers EINVAL). The _ex
// and resolve functions report no route and nothing skipped (no route was
// tried), and nothing is made at the Options.base they are given.
#[test]
fn an_empty_path_is_invalid_input_everywhere() {
    let t = tmp();
    let check = |what: &str, e: io::Error| {
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{what}: {e}");
        assert_eq!(e.raw_os_error(), None, "{what}: {e}");
    };
    let check_ex = |what: &str, e: sockat::ExError| {
        assert_eq!(e.route, None, "{what}");
        assert!(e.skipped.is_empty(), "{what}");
        check(what, e.error);
    };
    let caller = UnixDatagram::unbound().unwrap();
    check("bind", sockat::bind("").unwrap_err());
    check("connect", sockat::connect("").unwrap_err());
    check(
        "connect_nonblocking",
        sockat::connect_nonblocking("").unwrap_err(),
    );
    check("datagram::bind", sockat::datagram::bind("").unwrap_err());
    check(
        "datagram::connect",
        sockat::datagram::connect(&caller, "").unwrap_err(),
    );
    check_ex("bind_ex", sockat::bind_ex("", own_base(&t)).err().unwrap());
    check_ex(
        "connect_ex",
        sockat::connect_ex("", own_base(&t)).err().unwrap(),
    );
    check_ex(
        "connect_nonblocking_ex",
        sockat::connect_nonblocking_ex("", own_base(&t))
            .err()
            .unwrap(),
    );
    check_ex(
        "datagram::bind_ex",
        sockat::datagram::bind_ex("", own_base(&t)).err().unwrap(),
    );
    check_ex(
        "datagram::connect_ex",
        sockat::datagram::connect_ex(&caller, "", own_base(&t))
            .err()
            .unwrap(),
    );
    check_ex(
        "resolve_for_bind",
        sockat::resolve_for_bind("", own_base(&t)).err().unwrap(),
    );
    check_ex(
        "resolve_for_connect",
        sockat::resolve_for_connect("", own_base(&t)).err().unwrap(),
    );
    assert!(!t.path().join("b").exists(), "Options.base was made");
}

// A datagram connect at a long path to a socket file no socket holds (bound,
// then closed) fails as std's datagram connect fails at a short path in the
// same situation, with the same errno on this OS. sockat hands the connect's
// error back unchanged, and that errno is the OS's own choice: ECONNREFUSED
// on Linux and macOS, EDESTADDRREQ on illumos (hub SPEC "エラー", measured
// in sockat-go's CI). So the test compares with std on the same OS rather
// than fixing either: a fixed ECONNREFUSED would fail on illumos for a
// sockat that does exactly what it should.
#[test]
fn a_datagram_connect_to_a_socket_nobody_holds_fails_as_std_does() {
    let t = tmp();
    let p = nest(t.path(), 300, "d.sock");
    drop(sockat::datagram::bind(&p).unwrap());
    assert!(is_socket(&p));
    let unbound = sockat::datagram::unbound().unwrap();
    let e = sockat::datagram::connect(&unbound, &p).unwrap_err();
    assert_eq!(
        e.raw_os_error(),
        Some(std_errno(Short::DatagramConnectToNobodyHolding))
    );
}

// bind_ex returns the listener (std's type), the SocketFile, what the
// spelling is good for and the diagnostics. For a long path the spelling is
// through B (Linked, base set) or Dangling (thread-cwd / at, no base);
// standard was passed over first. SocketFile::remove removes the socket file
// and cleans up E, does not close the listener, and does nothing the second
// time; a listener of bind_ex does not remove the file when closed, even for
// a short path. A Linked spelling, as std's local_addr reads it, names
// something before remove and nothing after it. That step is left out, with
// the reason on stderr, only where std itself drops the last byte of the
// address it reads (Rust 1.99 and earlier on OpenBSD, rust-lang/rust#118925,
// fixed in 1.100.0; measured by std_drops_last_byte): std's reading names
// nothing there to begin with. The same promise at the kernel's level is
// checked in src/tests.rs, which reads the spelling with getsockname itself:
// remove_removes_the_bound_file_once_and_not_a_replacement (no E left) and
// a_spelling_cleaned_up_names_nothing (the spelling reached the socket and
// names nothing once cleaned up).
#[test]
fn bind_ex_returns_the_socket_file_and_the_spelling() {
    let t = tmp();
    let p = nest(t.path(), 300, "s.sock");
    let bound = sockat::bind_ex(&p, own_base(&t)).unwrap();
    assert_eq!(bound.skipped[0].name, "standard");
    match bound.spelling {
        Spelling::Linked { .. } => assert!(bound.base.is_some()),
        Spelling::Dangling => assert!(bound.base.is_none()),
        other => panic!("{other:?}"),
    }
    let spelling = reported(&bound.socket);
    let check_spelling =
        matches!(bound.spelling, Spelling::Linked { .. }) && !std_drops_last_byte();
    if check_spelling {
        assert!(std::fs::symlink_metadata(&spelling).is_ok());
    } else if matches!(bound.spelling, Spelling::Linked { .. }) {
        eprintln!(
            "spelling {spelling:?} not checked: this std drops the last byte of the addresses \
             it reads on OpenBSD (rust-lang/rust#118925, fixed in Rust 1.100.0); checked with \
             getsockname by a_spelling_cleaned_up_names_nothing in src/tests.rs"
        );
    }
    let mut c = sockat::connect(&p).unwrap();
    ping(&bound.socket, &mut c);
    bound.file.remove().unwrap();
    assert!(!p.exists());
    if check_spelling {
        assert!(std::fs::symlink_metadata(&spelling).is_err());
    }
    bound.socket.set_nonblocking(true).unwrap();
    assert_eq!(
        bound.socket.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    bound.file.remove().unwrap();
    bound.file.cleanup().unwrap();

    let short = t.path().join("short.sock");
    let b = sockat::bind_ex(&short, own_base(&t)).unwrap();
    assert_eq!(b.spelling, Spelling::Original);
    assert_eq!(b.route, "standard");
    drop(b.socket);
    assert!(is_socket(&short));
    b.file.remove().unwrap();
    assert!(!short.exists());
}

// On macOS and NetBSD a bind follows a symlink in the last component of the
// path and makes the socket where it points. bind_ex's SocketFile, and the
// one ForBind::socket_file gives after the user's own bind, then hold the
// socket the symlink led to: remove removes that socket and leaves the
// user's symlink (hub SPEC "remove"), for a short path and a long one.
#[cfg(any(target_vendor = "apple", target_os = "netbsd"))]
#[test]
fn remove_leaves_a_symlink_at_the_path() {
    let t = tmp();
    let real = t.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let paths = [
        t.path().join("s.sock"),
        nest(&t.path().join("long"), 300, "s.sock"),
    ];
    for (i, p) in paths.iter().enumerate() {
        let target = real.join(format!("{i}.sock"));
        std::os::unix::fs::symlink(&target, p).unwrap();
        let bound = sockat::bind_ex(p, own_base(&t)).unwrap();
        assert!(is_socket(&target));
        let mut c = sockat::connect(p).unwrap();
        ping(&bound.socket, &mut c);
        bound.file.remove().unwrap();
        assert!(std::fs::symlink_metadata(&target).is_err(), "{p:?}");
        assert!(p.is_symlink(), "{p:?}");

        let for_bind = sockat::resolve_for_bind(p, own_base(&t)).unwrap();
        let _listener = UnixListener::bind(&for_bind.path).unwrap();
        assert!(is_socket(&target));
        let file = for_bind.socket_file().unwrap();
        file.remove().unwrap();
        assert!(std::fs::symlink_metadata(&target).is_err(), "{p:?}");
        assert!(p.is_symlink(), "{p:?}");
    }
}

// An _ex function that fails keeps the errno, converts into the io::Error by
// `?`, and reports the route that made the call (None when the path was
// refused before any route: the directory missing; nothing skipped then).
// The errno kept is the OS's, the one std gives a short path in the same
// situation on this OS (compared, not fixed: OSes may differ).
#[test]
fn an_ex_error_keeps_the_errno_and_the_routes() {
    let t = tmp();
    let p = nest(t.path(), 300, "s.sock");
    let missing = p.parent().unwrap().join("none/s.sock");
    let e = sockat::connect_ex(&missing, own_base(&t)).unwrap_err();
    assert_eq!(e.route, None);
    assert!(e.skipped.is_empty());
    let io: io::Error = e.into();
    assert_eq!(
        io.raw_os_error(),
        Some(std_errno(Short::ConnectInAMissingDirectory))
    );
    std::fs::write(&p, b"").unwrap();
    let e = sockat::bind_ex(&p, own_base(&t)).unwrap_err();
    assert_eq!(
        e.error.raw_os_error(),
        Some(std_errno(Short::BindOntoAFile))
    );
    assert!(e.route.is_some());
    assert_eq!(e.skipped[0].name, "standard");
}

// The resolved spellings go to std: resolve_for_bind's to std's bind (the
// socket at the path), its socket_file() after the bind to remove the file
// and E; resolve_for_connect's to std's connect, many times, then cleanup.
// A path std accepts resolves to itself.
#[test]
fn resolved_spellings_go_to_std() {
    let t = tmp();
    let p = nest(t.path(), 300, "s.sock");
    let for_bind = sockat::resolve_for_bind(&p, own_base(&t)).unwrap();
    assert!(for_bind.path.as_os_str().len() <= std_max());
    assert!(for_bind.socket_file().is_err(), "nothing bound yet");
    let listener = UnixListener::bind(&for_bind.path).unwrap();
    assert!(is_socket(&p));
    let file = for_bind.socket_file().unwrap();
    let for_connect = sockat::resolve_for_connect(&p, own_base(&t)).unwrap();
    assert_eq!(for_connect.route, "link-file");
    for _ in 0..3 {
        let mut c = UnixStream::connect(&for_connect.path).unwrap();
        ping(&listener, &mut c);
    }
    for_connect.cleanup().unwrap();
    assert!(UnixStream::connect(&for_connect.path).is_err());
    file.remove().unwrap();
    assert!(!p.exists());
    for_bind.cleanup().unwrap();

    let short = t.path().join("s.sock");
    let r = sockat::resolve_for_bind(&short, own_base(&t)).unwrap();
    assert_eq!(r.path, short);
    assert_eq!(r.spelling, Spelling::Original);
}

// Datagram sockets at long paths through the public API: a receiver bound by
// datagram::bind, an unbound sender connected by datagram::connect, and a
// sender bound by datagram::bind_ex whose address, as std's recv_from gives
// it, the receiver replies to when the spelling is Linked: what a user does.
// The _ex versions report their routes. The reply is left out, with the
// reason on stderr, only where std itself drops the last byte of the address
// it reads (Rust 1.99 and earlier on OpenBSD, rust-lang/rust#118925, fixed
// in 1.100.0; measured by std_drops_last_byte): there the reply to std's
// address cannot arrive whatever sockat does, also for a short path. The
// same promise at the kernel's level (the sender's spelling, as the kernel
// gives it with the datagram, reaches the sender) is checked by
// datagram_bind_connect_and_reply in src/tests.rs, which reads the address
// with recvfrom itself. The receive, the reports and remove are checked here
// with every std.
#[test]
fn datagrams_at_long_paths() {
    let t = tmp();
    let server_path = nest(&t.path().join("s"), 300, "server.sock");
    let client_path = nest(&t.path().join("c"), 300, "client.sock");
    let server = sockat::datagram::bind(&server_path).unwrap();
    let unbound = sockat::datagram::unbound().unwrap();
    sockat::datagram::connect(&unbound, &server_path).unwrap();
    unbound.send(b"one").unwrap();
    let mut buf = [0u8; 8];
    let n = server.recv(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"one");

    let bound = sockat::datagram::bind_ex(&client_path, own_base(&t)).unwrap();
    let report = sockat::datagram::connect_ex(&bound.socket, &server_path, own_base(&t)).unwrap();
    assert!(!report.route.is_empty());
    bound.socket.send(b"two").unwrap();
    let (n, from) = server.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"two");
    if let Some(from) = from.as_pathname() {
        if matches!(bound.spelling, Spelling::Linked { .. }) {
            if std_drops_last_byte() {
                eprintln!(
                    "reply to {from:?} not tried: this std drops the last byte of the \
                     addresses it reads on OpenBSD (rust-lang/rust#118925, fixed in Rust \
                     1.100.0), so a reply to std's from cannot arrive; the reply to the \
                     kernel's spelling is checked by datagram_bind_connect_and_reply in \
                     src/tests.rs with recvfrom"
                );
            } else {
                server.send_to(b"back", from).unwrap();
                let n = bound.socket.recv(&mut buf).unwrap();
                assert_eq!(&buf[..n], b"back");
            }
        }
    }
    bound.file.remove().unwrap();
    assert!(!client_path.exists());
    let _: UnixDatagram = server;
}

// Options.base puts B where the application says: a long-path bind_ex goes
// through it (base `given`) when its spelling is through B, the connects
// leave nothing in it, and sweep_base keeps this process's live D (a Linked
// spelling still reaches the socket after it). An Options.base that fails
// its check (its parent missing) is ENOENT with nothing made. The connect
// through the spelling is left out where std drops the last byte of the
// address it reads (ping_through_spelling); options_base_is_b_and_is_made_only_when_used
// in src/tests.rs checks the same with the spelling read by getsockname.
#[test]
fn options_base_holds_b() {
    let t = tmp();
    let base = t.path().join("app");
    let options = Options {
        base: Some(base.clone()),
        ..Default::default()
    };
    let p = nest(t.path(), 300, "s.sock");
    let bound = sockat::bind_ex(&p, options.clone()).unwrap();
    if bound.base.is_some() {
        assert_eq!(bound.base, Some("given"));
    }
    let before: Vec<_> = std::fs::read_dir(&base).unwrap().collect();
    for _ in 0..3 {
        let mut c = sockat::connect_ex(&p, options.clone()).unwrap().socket;
        ping(&bound.socket, &mut c);
    }
    let after: Vec<_> = std::fs::read_dir(&base).unwrap().collect();
    assert_eq!(before.len(), after.len());
    sockat::sweep_base(&base).unwrap();
    let mut c = sockat::connect(&p).unwrap();
    ping(&bound.socket, &mut c);
    if let Spelling::Linked { .. } = bound.spelling {
        ping_through_spelling(
            &bound.socket,
            "options_base_is_b_and_is_made_only_when_used",
        );
    }

    let bad = Options {
        base: Some(t.path().join("none/app")),
        ..Default::default()
    };
    let e = sockat::bind_ex(&p, bad).unwrap_err();
    assert_eq!(e.error.raw_os_error(), Some(libc::ENOENT));
    assert!(!t.path().join("none").exists());
}

// sweep() leaves this process's live D (a spelling bound by bind_ex keeps
// working after it) and makes no B. The test of the default places: it goes
// through the shared B, where it adds only its own D and leaves B (and the
// live D of other processes, locked) in place. The connect through the
// spelling is left out where std drops the last byte of the address it reads
// (ping_through_spelling); sweep_removes_only_dead_processes_d in
// src/tests.rs checks the same with the spelling read by getsockname.
#[test]
fn sweep_keeps_this_processs_d() {
    let t = tmp();
    let p = nest(t.path(), 300, "s.sock");
    let bound = sockat::bind_ex(&p, Options::default()).unwrap();
    sockat::sweep().unwrap();
    if let Spelling::Linked { .. } = bound.spelling {
        ping_through_spelling(&bound.socket, "sweep_removes_only_dead_processes_d");
    }
    bound.file.remove().unwrap();
}

// The address a long-path bind leaves on the socket fits sun_path on every
// platform, so whoever reads it (std's local_addr, CPython's getsockname,
// Go's net.FileConn) gets it whole: no bind spelling uses the widened
// sockaddr. Checked on the listener, on the stream it accepts and as the
// client's peer address, for lengths up to 253 bytes (within the widened
// form) and past it.
#[test]
fn the_address_of_a_long_path_bind_fits_sun_path() {
    let t = tmp();
    for total in [std_max() + 1, 200, 253, 400] {
        let p = nest(&t.path().join(format!("{total}")), total, "s.sock");
        let listener = sockat::bind(&p).unwrap();
        let client = sockat::connect(&p).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        for a in [
            listener.local_addr().unwrap(),
            accepted.local_addr().unwrap(),
            client.peer_addr().unwrap(),
        ] {
            if let Some(path) = a.as_pathname() {
                assert!(path.as_os_str().len() <= std_max(), "{path:?}");
            }
        }
    }
}
