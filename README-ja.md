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
- fork はせず、プロセスの cwd も呼び出し元のスレッドの cwd も変えない
- ソケットファイルを後で消したい時、綴りを後で使えるかやどう届いたかを知りたい時は `_ex` (`bind_ex` 等) を、綴りを自分で std に渡したい時は `resolve_for_bind` / `resolve_for_connect` を使う

## なぜ要るか

std の `bind` / `connect` は `sun_path` に収まらないパスを断る。ファイルシステムには上限が無いのに、深い一時ディレクトリ (macOS の `$TMPDIR` はそれだけで 50 バイト前後、試験の枠組みやサンドボックスがさらに掘る) にソケットを置くとすぐに当たる。上限が効くのは `bind(2)` / `connect(2)` に渡す引数の長さなので、sockat は同じ場所を指す短い綴りでカーネルに渡す。

## 長いパスへの届き方

カーネルは bind の綴りをソケットの寿命の間覚え、`getsockname` で返す。上のライブラリはその綴りでソケットのファイルを消し、DGRAM の受け手はその綴りへ返信する。だから bind の綴りは、後で辿っても同じ所を指し続ける物にする。

- **B**: この利用者の置き場のディレクトリ。`/tmp/sockat-<uid>` (無理なら `$XDG_RUNTIME_DIR/sockat`、`$TMPDIR/sockat-<uid>`、ソケットのディレクトリ)。mode 0711、使い回し、sockat は消さない
- **D**: B の中の、プロセスごとのディレクトリ。プロセスが `flock` を掛け続け、死んだプロセスの D は OS の一時ディレクトリの掃除か `sockat::sweep()` で消える
- **E**: D の中の symlink。ソケットのディレクトリを指す (link-dir、綴り `B/D/E/<ファイル名>`) か、ソケットのファイルを指す (link-file、綴り `B/D/E`。bind が symlink を辿る macOS と NetBSD)

B は `/proc/<pid>/fd/<B の fd>` (Linux) か実パスで綴る。それでも収まらない bind は、sockat のスレッドだけが cwd を D に移して (Linux、Apple の OS)、FreeBSD は `bindat(2)` で届かせる。connect の綴りは相手に残らないので、connect は書き込みもスレッドも要らない経路 (`/proc/self/fd`、cwd からの相対パス、macOS の広げた sockaddr、FreeBSD の `connectat(2)`) を先に、スレッドの cwd、B の直下にすぐ消す symlink の順に試し、何も残さない。

| OS | bind | connect |
|---|---|---|
| Linux、Android | link-dir、thread-cwd | proc-direct (`/proc` が無ければ relative)、thread-cwd-direct、link-file |
| macOS、iOS (Apple) | link-dir、link-file、thread-cwd | relative、widened、thread-cwd-direct、link-file |
| FreeBSD | link-dir、at | at-direct、link-file |
| NetBSD | link-dir、link-file | relative、link-file |
| OpenBSD、illumos | link-dir | relative、link-file |

経路と、どこまで届くかは [マニュアル](docs/MANUAL-ja.md#長いパスへの届き方)。

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

- **カーネルが返すアドレスは元のパスでない。** `local_addr()`・`peer_addr()`、`ss -x` 等は bind の綴りを返す。後で使えるかは `Spelling` で分かる。ソケットのファイルは元のパスか `SocketFile::remove()` で消す
- **状態がアプリの置き場の外に増える。** B と D が `/tmp` 等に残る。既定の `bind` の E と死んだプロセスの D は、OS の一時ディレクトリの掃除か `sweep()` まで残る。ソケットのディレクトリの B と、確かめに外れて作った B (`sockat-<uid>.<乱数>`) は、`sweep()` でも OS の掃除でも消えないことがある
- **片付けた後の綴りは使わない。** `/tmp` の B は消えた後に他人が同じ名前で作り直しうる。`/proc` を通る綴りはプロセスが終われば辿れない
- **生きている D が消されうる環境がある。** systemd v242 より古い Linux と、B が macOS の `$TMPDIR` にある時 (綴りが辿れなくなるだけ)
- **`PrivateTmp` の相手** は `/tmp` を通る綴りを辿れない (`Spelling` の `usable_with_private_tmp`)

詳しくは [マニュアル](docs/MANUAL-ja.md#長いパスで-std-と違う点) と [使ってはいけない書き方](docs/MANUAL-ja.md#使ってはいけない書き方)。

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

- [マニュアル](docs/MANUAL-ja.md): 使い方、経路、片付け、std と違う点、エラー
- [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md): 言語によらない振る舞いの正本
- [設計の判断 (DR)](docs/decisions/INDEX.md)

## ライセンス

MIT License, Yoshiaki Kawazu (@kawaz)
