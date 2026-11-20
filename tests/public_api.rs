//! The public API as a user sees it: paths of any length on whatever route
//! the platform picks, with the environment's own places for B (`/tmp`
//! first). Which route ran is looked at only through the diagnostics the
//! `_ex` functions return; the route-forcing tests live in the crate's unit
//! tests.

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
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap()
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
// the path: std's answer is not always the path (on OpenBSD std's local_addr
// drops the last byte of it, observed in CI), and "the same as std" is the
// promise.
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

// The documented errors for a long path, through the public API: ENOENT for a
// missing directory, EADDRINUSE for an existing file, ECONNREFUSED for a
// socket nobody listens on, InvalidInput for a path without a file name or
// with a NUL byte.
#[test]
fn long_path_errors_through_the_public_api() {
    let t = tmp();
    let p = nest(t.path(), 300, "s.sock");
    let missing = p.parent().unwrap().join("none/s.sock");
    let e = sockat::bind(&missing).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOENT));
    let e = sockat::connect(&missing).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::ENOENT));
    std::fs::write(&p, b"").unwrap();
    let e = sockat::bind(&p).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(libc::EADDRINUSE));
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
    assert_eq!(e.raw_os_error(), Some(libc::ECONNREFUSED));
}

// bind_ex returns the listener (std's type), the SocketFile, what the
// spelling is good for and the diagnostics. For a long path the spelling is
// through B (Linked, base set) or Dangling (thread-cwd / at, no base);
// standard was passed over first. SocketFile::remove removes the socket file
// and cleans up E, does not close the listener, and does nothing the second
// time; a listener of bind_ex does not remove the file when closed, even for
// a short path.
#[test]
fn bind_ex_returns_the_socket_file_and_the_spelling() {
    let t = tmp();
    let p = nest(t.path(), 300, "s.sock");
    let bound = sockat::bind_ex(&p, Options::default()).unwrap();
    assert_eq!(bound.skipped[0].name, "standard");
    match bound.spelling {
        Spelling::Linked { .. } => assert!(bound.base.is_some()),
        Spelling::Dangling => assert!(bound.base.is_none()),
        other => panic!("{other:?}"),
    }
    let spelling = reported(&bound.socket);
    let mut c = sockat::connect(&p).unwrap();
    ping(&bound.socket, &mut c);
    bound.file.remove().unwrap();
    assert!(!p.exists());
    if let Spelling::Linked { .. } = bound.spelling {
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
    let b = sockat::bind_ex(&short, Options::default()).unwrap();
    assert_eq!(b.spelling, Spelling::Original);
    assert_eq!(b.route, "standard");
    drop(b.socket);
    assert!(is_socket(&short));
    b.file.remove().unwrap();
    assert!(!short.exists());
}

// An _ex function that fails keeps the errno, converts into the io::Error by
// `?`, and reports the route that made the call (None when the path was
// refused before any route: the directory missing; nothing skipped then).
#[test]
fn an_ex_error_keeps_the_errno_and_the_routes() {
    let t = tmp();
    let p = nest(t.path(), 300, "s.sock");
    let missing = p.parent().unwrap().join("none/s.sock");
    let e = sockat::connect_ex(&missing, Options::default()).unwrap_err();
    assert_eq!(e.route, None);
    assert!(e.skipped.is_empty());
    let io: io::Error = e.into();
    assert_eq!(io.raw_os_error(), Some(libc::ENOENT));
    std::fs::write(&p, b"").unwrap();
    let e = sockat::bind_ex(&p, Options::default()).unwrap_err();
    assert_eq!(e.error.raw_os_error(), Some(libc::EADDRINUSE));
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
    let for_bind = sockat::resolve_for_bind(&p, Options::default()).unwrap();
    assert!(for_bind.path.as_os_str().len() <= std_max());
    assert!(for_bind.socket_file().is_err(), "nothing bound yet");
    let listener = UnixListener::bind(&for_bind.path).unwrap();
    assert!(is_socket(&p));
    let file = for_bind.socket_file().unwrap();
    let for_connect = sockat::resolve_for_connect(&p, Options::default()).unwrap();
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
    let r = sockat::resolve_for_bind(&short, Options::default()).unwrap();
    assert_eq!(r.path, short);
    assert_eq!(r.spelling, Spelling::Original);
}

// Datagram sockets at long paths through the public API: a receiver bound by
// datagram::bind, an unbound sender connected by datagram::connect, and a
// bound sender whose reported address the receiver replies to. The _ex
// versions report their routes.
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

    let bound = sockat::datagram::bind_ex(&client_path, Options::default()).unwrap();
    let report =
        sockat::datagram::connect_ex(&bound.socket, &server_path, Options::default()).unwrap();
    assert!(!report.route.is_empty());
    bound.socket.send(b"two").unwrap();
    let (n, from) = server.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"two");
    if let Some(from) = from.as_pathname() {
        if matches!(bound.spelling, Spelling::Linked { .. }) {
            server.send_to(b"back", from).unwrap();
            let n = bound.socket.recv(&mut buf).unwrap();
            assert_eq!(&buf[..n], b"back");
        }
    }
    bound.file.remove().unwrap();
    assert!(!client_path.exists());
    let _: UnixDatagram = server;
}

// Options.base puts B where the application says: a long-path bind_ex goes
// through it (base `given`) when its spelling is through B, the connects
// leave nothing in it, and sweep_base keeps this process's live D. An
// Options.base that fails its check (its parent missing) is ENOENT with
// nothing made.
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
        let mut c = UnixStream::connect(reported(&bound.socket)).unwrap();
        ping(&bound.socket, &mut c);
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
// working after it) and makes no B.
#[test]
fn sweep_keeps_this_processs_d() {
    let t = tmp();
    let p = nest(t.path(), 300, "s.sock");
    let bound = sockat::bind_ex(&p, Options::default()).unwrap();
    sockat::sweep().unwrap();
    if let Spelling::Linked { .. } = bound.spelling {
        let mut c = UnixStream::connect(reported(&bound.socket)).unwrap();
        ping(&bound.socket, &mut c);
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
