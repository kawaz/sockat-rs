# bind する

> [English](./bind.md) | 日本語

## 待ち受ける

std の `UnixListener::bind` の代わりに `sockat::bind` を使う。返るのは listen 済みの std の `UnixListener`。

```rust
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;

fn echo(mut stream: UnixStream) -> io::Result<()> {
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    stream.write_all(line.as_bytes())
}

fn start(path: &Path) -> io::Result<()> {
    let listener = sockat::bind(path)?;
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || echo(stream));
        }
    });
    Ok(())
}
```

## tokio で待ち受ける

listener を非ブロッキングにしてから tokio に渡す。

```rust
use std::io;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

async fn echo(mut stream: UnixStream) -> io::Result<()> {
    let mut line = String::new();
    BufReader::new(&mut stream).read_line(&mut line).await?;
    stream.write_all(line.as_bytes()).await
}

async fn start(path: &Path) -> io::Result<()> {
    let listener = sockat::bind(path)?;
    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(echo(stream));
        }
    });
    Ok(())
}
```

## 終わる時にソケットのファイルを消す

listener を閉じてもソケットのファイルは消えない (std と同じ) ので、`bind_ex` が返す `file` の `remove()` で、listener を閉じる前に消す。

```rust
use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;

fn run(path: &Path, serve: impl FnOnce(&UnixListener) -> io::Result<()>) -> io::Result<()> {
    let bound = sockat::bind_ex(path, sockat::Options::default())?;
    let served = serve(&bound.socket);
    bound.file.remove()?;
    drop(bound.socket);
    served
}
```

## 置き場を自分で決める

長いパスの時に sockat が使う置き場を、アプリのディレクトリの中の sockat 専用のディレクトリにする。前のプロセスが残した物は `sweep_base` で片付ける。

```rust
use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;

fn listen(path: &Path, app_dir: &Path) -> io::Result<UnixListener> {
    let base = app_dir.join("sockat");
    sockat::sweep_base(&base)?;
    let options = sockat::Options {
        base: Some(base),
        ..Default::default()
    };
    Ok(sockat::bind_ex(path, options)?.socket)
}
```

## 置き場に残った物を片付ける

既定の置き場に終わったプロセスが残した物は OS の一時ディレクトリの掃除で消えるが、すぐ片付けたい時は起動時に `sweep` を呼ぶ。

```rust
use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;

fn listen(path: &Path) -> io::Result<UnixListener> {
    sockat::sweep()?;
    sockat::bind(path)
}
```
