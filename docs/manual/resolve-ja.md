# パスだけを解決する (resolve)

> [English](./resolve.md) | 日本語

bind / connect は自分で (std や他のライブラリで) して、長いパスの代わりに渡す短いパス (綴り) だけを sockat から受け取る。

## std で bind する

`resolve_for_bind` の `path` を std に渡す。ソケットのファイルは、bind の後に `socket_file()` で受け取った `SocketFile` の `remove()` で、listener を閉じる前に消す。

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

## 同じ相手に何度もつなぐ

`resolve_for_connect` の `path` は何度でも connect に使える。使い終わったら `cleanup()` する。

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

## パスを取るライブラリに渡す

綴りはただのパスなので、パスを取るライブラリにそのまま渡せる。tokio の例:

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
