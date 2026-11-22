# Connect

> English | [日本語](./connect-ja.md)

## Connect

Use `sockat::connect` where you would use the standard library's `UnixStream::connect`. It returns the standard library's `UnixStream`.

```rust
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

fn ask(path: &Path, question: &str) -> io::Result<String> {
    let mut stream = sockat::connect(path)?;
    stream.write_all(question.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer)?;
    Ok(answer)
}
```

## Connect with tokio

`connect_nonblocking` does not wait for the peer and returns a nonblocking stream, so call it on the runtime and hand the stream to tokio as it is.

```rust
use std::io;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

async fn ask(path: &Path, question: &str) -> io::Result<String> {
    let stream = sockat::connect_nonblocking(path)?;
    let mut stream = UnixStream::from_std(stream)?;
    stream.write_all(question.as_bytes()).await?;
    let mut answer = String::new();
    BufReader::new(&mut stream).read_line(&mut answer).await?;
    Ok(answer)
}
```

## Use the same base as the bind

An application that chose its base ([Choose the base yourself](./bind.md#choose-the-base-yourself)) passes the same `base` to its connects.

```rust
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;

fn connect(path: &Path, app_dir: &Path) -> io::Result<UnixStream> {
    let options = sockat::Options {
        base: Some(app_dir.join("sockat")),
        ..Default::default()
    };
    Ok(sockat::connect_ex(path, options)?.socket)
}
```

## See how it was reached

`connect_ex` tells the route taken and the routes passed over. The names are for diagnostics: do not branch on them.

```rust
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;

fn connect(path: &Path) -> io::Result<UnixStream> {
    let connected = sockat::connect_ex(path, sockat::Options::default())
        .inspect_err(|e| eprintln!("failed: route {:?}, skipped {:?}", e.route, e.skipped))?;
    eprintln!("route {}, skipped {:?}", connected.route, connected.skipped);
    Ok(connected.socket)
}
```
