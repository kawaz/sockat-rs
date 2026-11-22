# sockat

> English | [日本語](./README-ja.md)

Your Unix domain socket path is too long to bind / connect. sockat is the Rust library that makes that go away.

When the path is long, Rust's std refuses to bind / connect with this error, of kind `io::ErrorKind::InvalidInput`:

```
path must be shorter than SUN_LEN
```

Replace the std functions with sockat's, and long paths just work.

```rust
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};

fn main() -> io::Result<()> {
    let path = "/a/very/deep/directory/.../server.sock";

    // let listener: UnixListener = UnixListener::bind(path)?;
    let listener: UnixListener = sockat::bind(path)?;

    // let stream: UnixStream = UnixStream::connect(path)?;
    let stream: UnixStream = sockat::connect(path)?;

    Ok(())
}
```

You get back the same types as std, and a short path goes straight to std as before. Only for a long path does sockat quietly set up a short way to the same place and connect through it. You don't need to shorten paths yourself or care about each OS's limit. Because these are std types, you can hand them to tokio and other async runtimes the same way as any std socket.

## Installation

```sh
cargo add sockat
```

## The sockat command

There is a command too. It is handy for trying sockat out before building it into an application, and for making a path to hand to a program that cannot use sockat. Given a long path, it prints a path to the same socket that fits the limit.

```sh
cargo install sockat-cli
```

```console
$ sockat /a/very/long/directory/.../app.sock
/tmp/sockat-1000/3f9c2a7b1e04/app.sock

$ sockat --json /a/very/long/directory/.../app.sock
{
  "path": "/tmp/sockat-1000/3f9c2a7b1e04/app.sock",
  "bytes": 38,
  "limit": 107,
  "original": "/a/very/long/directory/.../app.sock",
  "original_bytes": 214,
  "route": "link-dir",
  "link": "/tmp/sockat-1000/3f9c2a7b1e04",
  "target": "/a/very/long/directory/...",
  "for": ["bind", "connect"],
  "warnings": []
}
```

## More

- [User manual](https://github.com/kawaz/sockat-rs/blob/main/docs/MANUAL.md)
- [Reference](https://github.com/kawaz/sockat-rs/blob/main/docs/REFERENCE.md)
- [How it works](https://github.com/kawaz/sockat/blob/main/pages/technical.md)

## License

MIT License, Yoshiaki Kawazu (@kawaz)
