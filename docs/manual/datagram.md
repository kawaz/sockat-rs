# Use DGRAM

> English | [日本語](./datagram-ja.md)

## Receive

Use `sockat::datagram::bind` where you would use std's `UnixDatagram::bind`.

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

## Send

Connect an `unbound` socket to the peer with `sockat::datagram::connect`.

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

## Get a reply

When the sender is bound to a long path too, the receiver can reply to the address from `recv_from` (while the spelling can be followed; on OpenBSD with Rust 1.100 or later, see [Caveats specific to Rust](../REFERENCE.md#caveats-specific-to-rust)).

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

## send_to a long path

`send_to` the spelling of `resolve_for_connect`.

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

## Remove the socket file when done

Remove it with `remove()` on the `file` that `datagram::bind_ex` returns, before the socket is closed.

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
