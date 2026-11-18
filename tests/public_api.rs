//! The public API as a user sees it: `sockat::bind` and the four connects with
//! paths of any length, on whatever route the platform picks. Which route ran
//! is deliberately not observable here (the route-forcing tests live in the
//! crate's unit tests).

use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

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

fn is_socket(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_socket())
        .unwrap_or(false)
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

// The promise of the crate: a socket can be bound and reached at a path of any
// length (well past sun_path's 104 / 108 bytes), the socket file is at that
// path, and it can be removed by that path. Both sides of the length std
// accepts are included (the last length handed to std and the first one
// past it). Two bases: the default temporary directory (on macOS $TMPDIR is
// already about 50 bytes, and longer under sandboxes and test harnesses) and
// /tmp.
#[test]
fn bind_and_connect_work_at_any_length() {
    let bases = [
        tempfile::tempdir().unwrap(),
        tempfile::Builder::new()
            .prefix("sockat-")
            .tempdir_in("/tmp")
            .unwrap(),
    ];
    let max = std_max();
    for base in &bases {
        for total in [100, max, max + 1, 108, 200, 253, 254, 400, 1000] {
            let total = total.max(base.path().as_os_str().len() + 10);
            let p = nest(base.path(), total, "s.sock");
            let listener = sockat::bind(&p).unwrap();
            assert!(is_socket(&p), "{total}: {p:?}");
            let mut client = sockat::connect(&p).unwrap();
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

// The four connects (the two that may fork with the crate feature
// `any`) in an ordinary environment (a writable /tmp, the socket
// under a temporary directory with a short file name): every length is
// reached without forking on every platform (DR-0006: the symlink route under
// /tmp takes any directory depth), so the two connects that may fork report
// that they did not, and the two that may not connect as well. The nonblocking
// ones hand back a stream that does not wait: a read with nothing to read is
// WouldBlock.
#[test]
fn in_an_ordinary_environment_no_connect_forks() {
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    for total in [std_max() + 1, 253, 400, 1000] {
        let p = nest(t.path(), total, &format!("{total}.sock"));
        let listener = sockat::bind(&p).unwrap();

        let mut c = sockat::connect(&p).unwrap();
        ping(&listener, &mut c);

        let mut c = sockat::connect_nonblocking(&p).unwrap();
        let mut buf = [0u8; 1];
        let e = c.read(&mut buf).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "{total}");
        drop(listener.accept().unwrap());

        #[cfg(feature = "any")]
        {
            let sockat::AnyConnection {
                stream: mut c,
                forked,
            } = sockat::connect_any(&p).unwrap();
            assert!(!forked, "{total}");
            ping(&listener, &mut c);

            let sockat::AnyConnection {
                stream: mut c,
                forked,
            } = sockat::connect_any_nonblocking(&p).unwrap();
            assert!(!forked, "{total}");
            let e = c.read(&mut buf).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock, "{total}");
        }
    }
}

// A long file name far from the cwd, too long to fit behind
// /tmp/sockat.XXXXXXXXXX/ (over 84 bytes for the plain sockaddr on Linux, 80
// on Apple and the BSDs, where a bind never widens the sockaddr) and, on
// Linux, behind /proc/self/fd/<n>/ (over about 90), but within the plain
// sockaddr of the thread cwd and fork routes (100 bytes). `bind` reaches it
// (through the thread cwd route on Linux and Apple where it can be used,
// otherwise forking; FreeBSD through bindat).
// The connects reach it through a symlink to the socket file, whose spelling
// does not contain the file name, so in an ordinary environment (a writable
// /tmp) none of them forks, and the plain `connect` and `connect_nonblocking`
// work too (the two that may fork are checked with the crate feature
// `any`).
#[test]
fn a_long_file_name_is_connected_to_without_forking() {
    const NAME_LEN: usize = 100;
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    let p = nest(t.path(), 600, &"n".repeat(NAME_LEN));
    let listener = sockat::bind(&p).unwrap();
    assert!(is_socket(&p));
    let mut c = sockat::connect(&p).unwrap();
    ping(&listener, &mut c);
    let mut c = sockat::connect_nonblocking(&p).unwrap();
    let mut buf = [0u8; 1];
    let e = c.read(&mut buf).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::WouldBlock);
    drop(listener.accept().unwrap());
    #[cfg(feature = "any")]
    {
        let sockat::AnyConnection {
            stream: mut c,
            forked,
        } = sockat::connect_any(&p).unwrap();
        assert!(!forked);
        ping(&listener, &mut c);
        let any = sockat::connect_any_nonblocking(&p).unwrap();
        assert!(!any.forked);
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
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    let p = t.path().join("s.sock");
    let (std_local, std_peer, std_accepted) = {
        let listener = UnixListener::bind(&p).unwrap();
        let client = UnixStream::connect(&p).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let addrs = (
            listener
                .local_addr()
                .unwrap()
                .as_pathname()
                .map(Path::to_path_buf),
            client
                .peer_addr()
                .unwrap()
                .as_pathname()
                .map(Path::to_path_buf),
            accepted
                .local_addr()
                .unwrap()
                .as_pathname()
                .map(Path::to_path_buf),
        );
        // Closed before the unlink: on illumos an unlink followed by the
        // close can make a concurrent bind fail with EADDRINUSE (a race in
        // the kernel).
        drop((listener, client, accepted));
        std::fs::remove_file(&p).unwrap();
        addrs
    };
    let listener = sockat::bind(&p).unwrap();
    assert_eq!(
        listener.local_addr().unwrap().as_pathname(),
        std_local.as_deref()
    );
    let client = sockat::connect(&p).unwrap();
    assert_eq!(
        client.peer_addr().unwrap().as_pathname(),
        std_peer.as_deref()
    );
    let (accepted, _) = listener.accept().unwrap();
    assert_eq!(
        accepted.local_addr().unwrap().as_pathname(),
        std_accepted.as_deref()
    );
    drop((client, accepted));
    // And a std client reaches a sockat listener (and the other way round).
    let mut std_client = UnixStream::connect(&p).unwrap();
    ping(&listener, &mut std_client);
    let q = t.path().join("q.sock");
    let std_listener = UnixListener::bind(&q).unwrap();
    let mut c = sockat::connect(&q).unwrap();
    ping(&std_listener, &mut c);
}

// The returned types are plain std sockets: a long-path listener accepts a
// client from another thread, and the streams can be cloned and shut down as
// usual. Nothing about the route leaks into the types.
#[test]
fn the_results_are_ordinary_std_sockets() {
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
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
                let t = tempfile::Builder::new()
                    .prefix("sockat-")
                    .tempdir_in("/tmp")
                    .unwrap();
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
// missing directory, EADDRINUSE for an existing file, ENAMETOOLONG when the
// file name alone is too long for every bind route, InvalidInput for a path
// without a file name.
#[test]
fn long_path_errors_through_the_public_api() {
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    let p = nest(t.path(), 400, "s.sock");
    let missing = p.parent().unwrap().join("nope/s.sock");
    assert_eq!(
        sockat::bind(&missing).unwrap_err().raw_os_error(),
        Some(libc::ENOENT)
    );
    assert_eq!(
        sockat::connect(&missing).unwrap_err().raw_os_error(),
        Some(libc::ENOENT)
    );
    let _l = sockat::bind(&p).unwrap();
    assert_eq!(
        sockat::bind(&p).unwrap_err().raw_os_error(),
        Some(libc::EADDRINUSE)
    );
    let long_name = t.path().join("n".repeat(254));
    assert_eq!(
        sockat::bind(&long_name).unwrap_err().raw_os_error(),
        Some(libc::ENAMETOOLONG)
    );
    let mut dir_slash = p.parent().unwrap().as_os_str().to_owned();
    dir_slash.push("/");
    assert_eq!(
        sockat::bind(&dir_slash).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
}

// The address a long-path bind leaves on the socket fits sun_path on every
// platform, so whoever reads it (std's local_addr, CPython's getsockname, Go's
// net.FileConn) gets it whole: std shows at most std_max bytes. On Apple that
// is because a bind never takes the widened sockaddr (DR-0024): a socket bound
// through it has an address of up to 255 bytes, which std shows cut short at
// sun_path's 104 bytes with no NUL. Checked on the listener, on the stream it
// accepts and as the client's peer address, for lengths up to 253 bytes (all
// within the widened form) and past it.
#[test]
fn the_address_of_a_long_path_bind_fits_sun_path() {
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    for total in [std_max() + 1, 200, 253, 400] {
        let p = nest(t.path(), total, &format!("{total}.sock"));
        let listener = sockat::bind(&p).unwrap();
        let client = sockat::connect(&p).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        for (what, addr) in [
            ("listener", listener.local_addr().unwrap()),
            ("accepted", accepted.local_addr().unwrap()),
            ("client's peer", client.peer_addr().unwrap()),
        ] {
            let shown = addr.as_pathname().unwrap();
            assert!(
                shown.as_os_str().len() <= std_max(),
                "{total} {what}: {shown:?}"
            );
        }
    }
}

// The longest file name a bind reaches is sun_path less the NUL (std_max: 107
// on Linux, 103 on Apple and the BSDs), on every platform: every bind route
// that does not hand the kernel the whole path ends with the file name in the
// plain sockaddr (on Apple too, a bind never widening it, DR-0024). A name of
// std_max bytes directly under a short directory binds and is reached; one
// byte more is ENAMETOOLONG, and nothing is created.
#[test]
fn a_bind_reaches_a_file_name_up_to_sun_path_less_the_nul() {
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    let at = t.path().join("a".repeat(std_max()));
    let listener = sockat::bind(&at).unwrap();
    assert!(is_socket(&at));
    let mut client = sockat::connect(&at).unwrap();
    ping(&listener, &mut client);
    let over = t.path().join("o".repeat(std_max() + 1));
    assert_eq!(
        sockat::bind(&over).unwrap_err().raw_os_error(),
        Some(libc::ENAMETOOLONG)
    );
    assert!(!over.exists());
}

// A long path needs no more permission on the socket's directory than std's
// bind / connect of a short path: search and write, not read. On Linux the
// /proc route opens the directory O_PATH, which takes no permission at all on
// the directory itself, so a directory without read permission (mode 0300)
// must work at any length. Root bypasses permission checks, so under root
// (as in `just test-linux`, docker) the test cannot tell anything and says so;
// the GitHub Actions runner is not root, which is where it bites.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_directory_without_read_permission_is_enough_on_linux() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    if std::fs::metadata(t.path()).unwrap().uid() == 0 {
        eprintln!("skipped: running as root, which bypasses the permission under test");
        return;
    }
    let p = nest(t.path(), 400, "s.sock");
    let dir = p.parent().unwrap().to_path_buf();
    // Put the mode back whatever happens, so the temporary directory can be
    // removed.
    struct Restore(PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    let _restore = Restore(dir.clone());
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o300)).unwrap();
    // The premise: the directory really cannot be read.
    assert_eq!(
        std::fs::read_dir(&dir).unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
    let listener = sockat::bind(&p).unwrap();
    let mut client = sockat::connect(&p).unwrap();
    ping(&listener, &mut client);
}
