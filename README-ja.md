# sockat

> [English](./README.md) | 日本語

`sun_path` の上限 (NUL 込みで macOS と BSD は 104 バイト、Linux は 108 バイト) を超えるパスでも、Unix domain socket を bind / connect できる Rust のライブラリ。

言語によらない振る舞いの正本は [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) で、sockat-rs はその Rust 実装。

```rust
let listener = sockat::bind("/とても/深い/ディレクトリ/.../server.sock")?;
let stream = sockat::connect("/とても/深い/ディレクトリ/.../server.sock")?;
```

## 使い方

std の関数の代わりに sockat の関数を使う。

| sockat | 代わりにする std |
|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` |
| `sockat::connect(path)` | `UnixStream::connect(path)` |
| `sockat::connect_nonblocking(path)` | (無し。待たずに非ブロッキングの stream を返す) |
| `sockat::datagram::bind(path)`・`unbound()`・`connect(&sock, path)` | `UnixDatagram` の同じ名前の関数 |

- 戻り値は std の型そのもの
- std が受け付ける長さのパスは std にそのまま渡すので、std と同じに振る舞う
- 長いパスでも、ソケットファイルは渡したパスにできる
- 長いパスの bind は、アプリの置き場の外 (`/tmp/sockat-<uid>` 等) に小さなディレクトリと symlink を残す。何が残りどう片付くかは仕様の [置き場に残る物](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#置き場に残る物) と [`sweep`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#sweep)、Rust での呼び方 (`sockat::sweep()`・`sockat::sweep_base(path)`) は [マニュアル](docs/MANUAL-ja.md#sweep-と-sweep_base)
- ソケットファイルを後で消したい時、綴りを後で使えるかやどう届いたかを知りたい時は `_ex` (`bind_ex` 等) を、綴りを自分で std に渡したい時は `resolve_for_bind` / `resolve_for_connect` を使う

## 長いパスへの届き方

長いパスは、元のパスと同じ所を指す短い綴りでカーネルに渡す。置き場 B・D・E、経路、OS ごとに試す順番は仕様の [長いパスの経路と試す順番](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスの経路と試す順番)、どこまで届くかは [届く範囲の表](https://github.com/kawaz/sockat/blob/main/pages/reach/README-ja.md)。sockat-rs は仕様のすべての経路を作り、どの OS でも仕様の順のとおりに試す。

## 後始末と診断つきの _ex

```rust
let bound = sockat::bind_ex(path, Default::default())?;
match bound.spelling {
    sockat::Spelling::Linked { removable, .. } => { /* 綴りは辿れる。removable が false なら綴りで消すと E が消えるだけ */ }
    sockat::Spelling::Dangling => { /* 綴りはどこにも届かない */ }
    _ => { /* 元のパス */ }
}
// ... bound.socket で accept する (tokio 等に渡してもよい) ...
bound.file.remove()?; // ソケットのファイルと E を消す。listener を閉じる前に
drop(bound.socket);
```

- `bind_ex`・`connect_ex`・`connect_nonblocking_ex`・`datagram::bind_ex`・`datagram::connect_ex` は、`sockat::Options` (`usable_cross_uid`: 別の uid も辿れる綴りを求める、`base`: B をアプリの場所に置く) を取り、通った経路・B・飛ばした経路を返す
- `bind_ex` は、綴りを後で使えるか (`Spelling`) と、ソケットのファイルを消す `SocketFile` (`remove()`・`cleanup()`) を返す
- 失敗は `ExError` (errno を保ち、`?` で `io::Error` に戻る)

## 長いパスで std と違う点

言語によらない違いと、使ってはいけない書き方は仕様の [長いパスで標準と違う点](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスで標準と違う点) と [使ってはいけない書き方](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#使ってはいけない書き方)。Rust では:

- 長いパスでは `local_addr()`・`peer_addr()`・`recv_from()` のアドレスが元のパスでなく bind の綴りになる。ソケットのファイルは元のパスか `SocketFile::remove()` で消す
- OpenBSD の Rust 1.99 以前の std は、それらのアドレスの最後の 1 バイトを落とす。`recv_from` のアドレスへ DGRAM を返信するには Rust 1.100 以上が要る

Rust での形と推奨コードは [マニュアルの「長いパスで std と違う点」](docs/MANUAL-ja.md#長いパスで-std-と違う点)、[「使ってはいけない書き方」](docs/MANUAL-ja.md#使ってはいけない書き方)、[「Rust に固有の注意」](docs/MANUAL-ja.md#rust-に固有の注意)。

## async ランタイム

戻り値は std の型なので、tokio 等にそのまま渡せる。

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` は待たず (後片付けも unlink だけ)、非ブロッキングのまま返すので、runtime の worker から直接呼べる。

## ドキュメント

- [マニュアル](docs/MANUAL-ja.md): Rust での関数と型、片付けの口、使ってはいけない書き方の Rust 版、エラーの型、Rust に固有の注意
- [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md): 言語によらない振る舞いの正本
- [言語ごとの注意](https://github.com/kawaz/sockat/blob/main/pages/langs-ja.md): 各言語の実装に固有の注意の一覧
- [設計の判断 (DR)](docs/decisions/INDEX.md)

## ライセンス

MIT License, Yoshiaki Kawazu (@kawaz)
