//! The async-runtime recipe from the crate docs, run for real with tokio: the
//! std sockets from sockat are switched to non-blocking and adopted by
//! `tokio::net`. tokio is only a dev-dependency; the crate itself does not
//! depend on any runtime.

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
// the socket in non-blocking mode to the caller, so the recipe in the docs
// includes `set_nonblocking(true)`; this test follows the recipe verbatim.
#[tokio::test]
async fn std_sockets_from_sockat_are_adopted_by_tokio() {
    let t = tempfile::Builder::new()
        .prefix("sockat-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = long_path(t.path(), 400);

    let listener = sockat::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();

    let p = path.clone();
    let client = tokio::task::spawn_blocking(move || sockat::connect(&p))
        .await
        .unwrap()
        .unwrap();
    client.set_nonblocking(true).unwrap();
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
