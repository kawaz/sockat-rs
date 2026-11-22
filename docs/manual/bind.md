# Bind

> English | [日本語](./bind-ja.md)

## Listen

Use `sockat::bind` where you would use std's `UnixListener::bind`. It returns std's `UnixListener`, already listening.

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

## Listen with tokio

Make the listener nonblocking, then hand it to tokio.

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

## Remove the socket file when done

Closing the listener does not remove the socket file (as with std), so remove it with `remove()` on the `file` that `bind_ex` returns, before the listener is closed.

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

## Choose the base yourself

Make the base sockat uses for a long path a directory of sockat's own inside the application's directory. What earlier processes left there goes by `sweep_base`.

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

## Clear what was left in the bases

What finished processes left in the default bases goes with the OS's cleaning of temporary directories; to clear it at once, call `sweep` at start-up.

```rust
use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;

fn listen(path: &Path) -> io::Result<UnixListener> {
    sockat::sweep()?;
    sockat::bind(path)
}
```
