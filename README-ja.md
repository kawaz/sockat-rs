# sockat

> [English](./README.md) | 日本語

Unix domain socket のパスが長すぎて bind / connect できない。sockat は、それを無くす Rust のライブラリです。

Rust の std は、パスが長いと bind / connect を `io::ErrorKind::InvalidInput` の次のエラーで断ります。

```
path must be shorter than SUN_LEN
```

std の関数を sockat の関数に置き換えるだけで、長いパスでもそのまま動きます。

```rust
use std::io;
use std::os::unix::net::{UnixListener, UnixStream};

fn main() -> io::Result<()> {
    let path = "/とても/深い/ディレクトリ/.../server.sock";

    // let listener: UnixListener = UnixListener::bind(path)?;
    let listener: UnixListener = sockat::bind(path)?;

    // let stream: UnixStream = UnixStream::connect(path)?;
    let stream: UnixStream = sockat::connect(path)?;

    Ok(())
}
```

返ってくるのは std と同じ型で、短いパスは今までどおり std にそのまま渡します。長いパスの時だけ、sockat が同じ場所を指す短い道を裏で用意してつなぎます。パスを短くする工夫も、OS ごとの上限も、気にしなくて構いません。std の型なので、tokio 等の async ランタイムにも std のソケットと同じ手順で渡せます。

## 入れ方

```sh
cargo add sockat
```

## もっと詳しく

- [利用マニュアル](./docs/MANUAL-ja.md)
- [リファレンス](./docs/REFERENCE-ja.md)
- [技術説明](https://github.com/kawaz/sockat/blob/main/pages/technical-ja.md)

## ライセンス

MIT License, Yoshiaki Kawazu (@kawaz)
