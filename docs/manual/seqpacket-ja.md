# SEQPACKET を使う

> [English](./seqpacket.md) | 日本語

sockat には SEQPACKET の関数がまだ無く、Rust の std にも SEQPACKET の型が無いので、ソケットは自分で ([socket2](https://crates.io/crates/socket2) の `Type::SEQPACKET` で) 作り、[resolve](./resolve-ja.md) の綴りに bind / connect する。SEQPACKET を作れる OS (Linux 等) で動く。

```toml
[dependencies]
socket2 = { version = "0.6", features = ["all"] }
```

## 待ち受ける

ソケットを先に作り、`resolve_for_bind` の綴りに bind する。bind に失敗したら `cleanup()` し、bind できたら `socket_file()` でソケットのファイルを消す口を受け取る。

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

## つなぐ

`resolve_for_connect` の綴りに connect する。つないだ後の接続は綴りを使わないので、connect したらすぐ `cleanup()` してよい。

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

## 終わる時にソケットのファイルを消す

`socket_file()` で受け取った `SocketFile` の `remove()` で、listener を閉じる前に消す。

```rust
use socket2::Socket;
use std::io;

fn stop(listener: Socket, file: sockat::SocketFile) -> io::Result<()> {
    file.remove()?;
    drop(listener);
    Ok(())
}
```
