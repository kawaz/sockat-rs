# DGRAM を使う

> [English](./datagram.md) | 日本語

## 受ける

標準ライブラリの `UnixDatagram::bind` の代わりに `sockat::datagram::bind` を使う。

```rust
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::Path;

fn open(path: &Path) -> io::Result<UnixDatagram> {
    sockat::datagram::bind(path)
}

fn receive(socket: &UnixDatagram) -> io::Result<Vec<u8>> {
    let mut buf = [0; 1024];
    let n = socket.recv(&mut buf)?;
    Ok(buf[..n].to_vec())
}
```

## 送る

`unbound` のソケットを `sockat::datagram::connect` で相手につなぐ。

```rust
use std::io;
use std::path::Path;

fn send(path: &Path, message: &[u8]) -> io::Result<()> {
    let socket = sockat::datagram::unbound()?;
    sockat::datagram::connect(&socket, path)?;
    socket.send(message)?;
    Ok(())
}
```

## 返事をもらう

送り手も長いパスに bind すれば、受け手は `recv_from` のアドレスへ返事を送れる (綴りが辿れる間。OpenBSD では Rust 1.100 以上。[Rust に固有の注意](../REFERENCE-ja.md#rust-に固有の注意))。

```rust
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::Path;

fn ask(path: &Path, my_path: &Path, question: &[u8]) -> io::Result<Vec<u8>> {
    let socket = sockat::datagram::bind(my_path)?;
    sockat::datagram::connect(&socket, path)?;
    socket.send(question)?;
    let mut buf = [0; 1024];
    let n = socket.recv(&mut buf)?;
    Ok(buf[..n].to_vec())
}

fn answer(socket: &UnixDatagram) -> io::Result<()> {
    let mut buf = [0; 1024];
    let (n, from) = socket.recv_from(&mut buf)?;
    if let Some(from) = from.as_pathname() {
        socket.send_to(&buf[..n], from)?;
    }
    Ok(())
}
```

## 長いパスへ send_to する

`send_to` は、`resolve_for_connect` の綴りに送る。

```rust
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::Path;

fn send_to(socket: &UnixDatagram, path: &Path, message: &[u8]) -> io::Result<()> {
    let to = sockat::resolve_for_connect(path, sockat::Options::default())?;
    let sent = socket.send_to(message, &to.path);
    to.cleanup()?;
    sent.map(drop)
}
```

## 終わる時にソケットのファイルを消す

`datagram::bind_ex` が返す `file` の `remove()` で、ソケットを閉じる前に消す。

```rust
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::Path;

fn run(path: &Path, serve: impl FnOnce(&UnixDatagram) -> io::Result<()>) -> io::Result<()> {
    let bound = sockat::datagram::bind_ex(path, sockat::Options::default())?;
    let served = serve(&bound.socket);
    bound.file.remove()?;
    drop(bound.socket);
    served
}
```
