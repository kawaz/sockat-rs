//! The public API as a user sees it: `sockat::bind` / `sockat::connect` with
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
// path, and it can be removed by that path. Two bases: the default temporary
// directory (on macOS $TMPDIR is already about 50 bytes, and longer under
// sandboxes and test harnesses) and /tmp.
#[test]
fn bind_and_connect_work_at_any_length() {
    let bases = [
        tempfile::tempdir().unwrap(),
        tempfile::Builder::new()
            .prefix("sockat-")
            .tempdir_in("/tmp")
            .unwrap(),
    ];
    for base in &bases {
        for total in [100, 101, 108, 200, 253, 254, 400, 1000] {
            let total = total.max(base.path().as_os_str().len() + 10);
            let p = nest(base.path(), total, "s.sock");
            let listener = sockat::bind(&p).unwrap();
            assert!(is_socket(&p), "{total}: {p:?}");
            let mut client = sockat::connect(&p).unwrap();
            ping(&listener, &mut client);
            std::fs::remove_file(&p).unwrap();
            assert!(!is_socket(&p));
        }
    }
}

// Short paths are std's business: sockat hands them over unchanged, so what
// std reports (local_addr, peer_addr) is the path itself, exactly as with
// UnixListener::bind.
#[test]
fn a_short_path_behaves_exactly_like_std() {
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    let p = t.path().join("s.sock");
    let listener = sockat::bind(&p).unwrap();
    assert_eq!(
        listener.local_addr().unwrap().as_pathname(),
        Some(p.as_path())
    );
    let client = sockat::connect(&p).unwrap();
    assert_eq!(client.peer_addr().unwrap().as_pathname(), Some(p.as_path()));
    let (accepted, _) = listener.accept().unwrap();
    assert_eq!(
        accepted.local_addr().unwrap().as_pathname(),
        Some(p.as_path())
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
// file name alone cannot be reached, InvalidInput for a path without a file
// name.
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
