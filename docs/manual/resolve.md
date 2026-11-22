# Resolve only the path

> English | [日本語](./resolve-ja.md)

Do the bind / connect yourself (with the standard library or another library), and take from sockat only the short path (the spelling) to hand over in place of the long one.

## Bind with the standard library

Hand the `path` of `resolve_for_bind` to the standard library. Remove the socket file with `remove()` on the `SocketFile` that `socket_file()` gives after the bind, before the listener is closed.

```rust
use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;

fn listen(path: &Path) -> io::Result<(UnixListener, sockat::SocketFile)> {
    let spelled = sockat::resolve_for_bind(path, sockat::Options::default())?;
    let listener = match UnixListener::bind(&spelled.path) {
        Ok(listener) => listener,
        Err(e) => {
            spelled.cleanup()?;
            return Err(e);
        }
    };
    let file = spelled.socket_file()?;
    Ok((listener, file))
}

fn stop(listener: UnixListener, file: sockat::SocketFile) -> io::Result<()> {
    file.remove()?;
    drop(listener);
    Ok(())
}
```

## Connect to the same peer many times

The `path` of `resolve_for_connect` can be used for any number of connects. Call `cleanup()` when done with it.

```rust
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

fn ask(to: &Path, question: &str) -> io::Result<String> {
    let mut stream = UnixStream::connect(to)?;
    stream.write_all(question.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(&stream).read_line(&mut answer)?;
    Ok(answer)
}

fn ask_all(path: &Path, questions: &[&str]) -> io::Result<Vec<String>> {
    let to = sockat::resolve_for_connect(path, sockat::Options::default())?;
    let answers = questions.iter().map(|q| ask(&to.path, q)).collect();
    to.cleanup()?;
    answers
}
```

## Hand it to a library that takes a path

A spelling is just a path, so a library that takes a path takes it as it is. With tokio:

```rust
use std::io;
use std::path::Path;
use tokio::net::{UnixListener, UnixStream};

async fn listen(path: &Path) -> io::Result<(UnixListener, sockat::SocketFile)> {
    let spelled = sockat::resolve_for_bind(path, sockat::Options::default())?;
    let listener = match UnixListener::bind(&spelled.path) {
        Ok(listener) => listener,
        Err(e) => {
            spelled.cleanup()?;
            return Err(e);
        }
    };
    let file = spelled.socket_file()?;
    Ok((listener, file))
}

async fn connect(path: &Path) -> io::Result<UnixStream> {
    let to = sockat::resolve_for_connect(path, sockat::Options::default())?;
    let stream = UnixStream::connect(&to.path).await;
    to.cleanup()?;
    stream
}
```
