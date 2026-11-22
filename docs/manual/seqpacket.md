# Use SEQPACKET

> English | [日本語](./seqpacket-ja.md)

sockat has no SEQPACKET functions yet, and Rust's std has no SEQPACKET type, so make the socket yourself (with [socket2](https://crates.io/crates/socket2)'s `Type::SEQPACKET`) and bind / connect it to the spelling of [resolve](./resolve.md). Runs on the OSes that make SEQPACKET sockets (Linux, ...).

```toml
[dependencies]
socket2 = { version = "0.6", features = ["all"] }
```

## Listen

Make the socket first, then bind it to the spelling of `resolve_for_bind`. When the bind fails, call `cleanup()`; once bound, take what removes the socket file with `socket_file()`.

```rust
use socket2::{Domain, SockAddr, Socket, Type};
use std::io;
use std::path::Path;

fn listen(path: &Path) -> io::Result<(Socket, sockat::SocketFile)> {
    let socket = Socket::new(Domain::UNIX, Type::SEQPACKET, None)?;
    let spelled = sockat::resolve_for_bind(path, sockat::Options::default())?;
    let bound = SockAddr::unix(&spelled.path)
        .and_then(|addr| socket.bind(&addr))
        .and_then(|()| socket.listen(128));
    if let Err(e) = bound {
        spelled.cleanup()?;
        return Err(e);
    }
    let file = spelled.socket_file()?;
    Ok((socket, file))
}
```

## Connect

Connect to the spelling of `resolve_for_connect`. The connection does not use the spelling once made, so call `cleanup()` right after the connect.

```rust
use socket2::{Domain, SockAddr, Socket, Type};
use std::io;
use std::path::Path;

fn connect(path: &Path) -> io::Result<Socket> {
    let socket = Socket::new(Domain::UNIX, Type::SEQPACKET, None)?;
    let to = sockat::resolve_for_connect(path, sockat::Options::default())?;
    let connected = SockAddr::unix(&to.path).and_then(|addr| socket.connect(&addr));
    to.cleanup()?;
    connected.map(|()| socket)
}
```

## Remove the socket file when done

Remove it with `remove()` on the `SocketFile` from `socket_file()`, before the listener is closed.

```rust
use socket2::Socket;
use std::io;

fn stop(listener: Socket, file: sockat::SocketFile) -> io::Result<()> {
    file.remove()?;
    drop(listener);
    Ok(())
}
```
