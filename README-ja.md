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

## コマンド sockat

コマンドもあります。アプリに組み込む前のお試しや、sockat を組み込めないプログラムに渡すパスを作るのに便利です。長いパスを渡すと、同じソケットを指して上限に収まるパスを出します。

```sh
cargo install sockat-cli
# または Homebrew (macOS・Linux)
brew install kawaz/tap/sockat
```

```console
$ sockat /とても/深い/ディレクトリ/.../app.sock
/tmp/sockat-1000/3f9c2a7b1e04/app.sock

$ sockat --json /とても/深い/ディレクトリ/.../app.sock
{
  "path": "/tmp/sockat-1000/3f9c2a7b1e04/app.sock",
  "bytes": 38,
  "limit": 107,
  "original": "/とても/深い/ディレクトリ/.../app.sock",
  "original_bytes": 214,
  "route": "link-dir",
  "link": "/tmp/sockat-1000/3f9c2a7b1e04",
  "target": "/とても/深い/ディレクトリ/...",
  "for": ["bind", "connect"],
  "warnings": []
}
```

## もっと詳しく

- [利用マニュアル](./docs/MANUAL-ja.md)
- [リファレンス](./docs/REFERENCE-ja.md)
- [技術説明](https://github.com/kawaz/sockat/blob/main/pages/technical-ja.md)

## ライセンス

MIT License, Yoshiaki Kawazu (@kawaz)
