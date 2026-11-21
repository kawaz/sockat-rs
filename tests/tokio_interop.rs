//! The async-runtime recipe from the crate docs, run for real with tokio: the
//! listener from sockat is switched to non-blocking, the stream comes from
//! `connect_nonblocking` already so, and both are adopted by `tokio::net`.
//! tokio is only a dev-dependency; the crate itself does not depend on any
//! runtime.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn long_path(base: &Path, total: usize) -> PathBuf {
    let mut dir = base.to_path_buf();
    while dir.as_os_str().len() + 1 + "s.sock".len() + 61 <= total {
        dir.push("d".repeat(60));
    }
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("s.sock");
    assert!(p.as_os_str().as_bytes().len() > 253);
    p
}

// A listener and a client on a path far beyond sun_path, both adopted by
// tokio, exchange data in both directions. tokio's `from_std` leaves putting
// the socket in non-blocking mode to the caller: the recipe in the docs calls
// `set_nonblocking(true)` on the listener, and connects with
// `connect_nonblocking`, which returns a non-blocking stream and does not
// wait on the runtime's thread (no `spawn_blocking`). This test follows the
// recipe verbatim.
#[tokio::test]
async fn std_sockets_from_sockat_are_adopted_by_tokio() {
    let t = tempfile::Builder::new()
        .prefix("srt-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = long_path(t.path(), 400);

    let listener = sockat::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();

    let client = sockat::connect_nonblocking(&path).unwrap();
    let mut client = tokio::net::UnixStream::from_std(client).unwrap();

    let (mut server, _) = listener.accept().await.unwrap();
    client.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    server.write_all(b"pong").await.unwrap();
    client.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"pong");
}

// bind_ex's socket goes to tokio like bind's (it is std's own type, moved
// into tokio::net::UnixListener::from_std), and the SocketFile stays behind:
// once tokio's listener has served a connection, remove() removes the socket
// file, before tokio's listener is dropped. This is why remove() does not
// close the listener itself: it no longer has it.
// Its B is the test's own (Options.base), not the one the tests of every
// sockat implementation share.
#[tokio::test]
async fn bind_ex_removes_after_the_listener_went_to_tokio() {
    let t = tempfile::Builder::new()
        .prefix("srt-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = long_path(t.path(), 400);

    let sockat::Bound { socket, file, .. } = sockat::bind_ex(
        &path,
        sockat::Options {
            base: Some(t.path().join("b")),
            ..Default::default()
        },
    )
    .unwrap();
    socket.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(socket).unwrap();
    let client = sockat::connect_nonblocking(&path).unwrap();
    let mut client = tokio::net::UnixStream::from_std(client).unwrap();
    let (mut server, _) = listener.accept().await.unwrap();
    client.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    server.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");

    assert!(path.exists());
    file.remove().unwrap();
    drop(listener);
    assert!(std::fs::symlink_metadata(&path).is_err());
}
