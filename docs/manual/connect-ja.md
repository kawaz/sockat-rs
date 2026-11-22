# connect する

> [English](./connect.md) | 日本語

## つなぐ

std の `UnixStream::connect` の代わりに `sockat::connect` を使う。返るのは std の `UnixStream`。

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

## tokio でつなぐ

`connect_nonblocking` は相手を待たずに非ブロッキングのストリームを返すので、runtime の中から呼んで、そのまま tokio に渡せる。

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

## bind と同じ置き場を使う

置き場を自分で決めたアプリ ([置き場を自分で決める](./bind-ja.md#置き場を自分で決める)) は、connect にも同じ `base` を渡す。

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

## どう届いたかを見る

`connect_ex` は、通った経路と飛ばした経路を返す。名前は診断用で、分岐には使わない。

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
