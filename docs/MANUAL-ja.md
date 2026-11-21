# sockat マニュアル

> [English](./MANUAL.md) | 日本語

sockat-rs (crate `sockat`) の使い方と、Rust での関数・型の対応、Rust での具体的な書き方、Rust に固有の注意のリファレンス。言語によらない振る舞いの正本は [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) で、sockat-rs はその Rust 実装。Rust で決めた所は [DR-0032](./decisions/DR-0032-resolve-through-persistent-base.md) にある。

言語によらない説明は仕様と pages にある:

- [長いパスで標準と違う点](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスで標準と違う点)
- [使ってはいけない書き方](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#使ってはいけない書き方)
- [置き場に残る物](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#置き場に残る物)
- [`sweep`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#sweep)
- [長いパスの経路と試す順番](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスの経路と試す順番)、[経路の作り方](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md)、[届く範囲の表](https://github.com/kawaz/sockat/blob/main/pages/reach/README-ja.md)
- [エラー](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#エラー)
- [言語ごとの注意](https://github.com/kawaz/sockat/blob/main/pages/langs-ja.md)

## 使い方

```toml
[dependencies]
sockat = "0.5"
```

std の関数の代わりに sockat の関数を使う。長さによらないパスを取り、std の型を返す。

| 関数 | 対応する std | 戻り値 |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listen 済み) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |
| `sockat::connect_nonblocking(path)` | (無し) | `io::Result<UnixStream>` (非ブロッキング) |
| `sockat::datagram::bind(path)` | `UnixDatagram::bind(path)` | `io::Result<UnixDatagram>` |
| `sockat::datagram::unbound()` | `UnixDatagram::unbound()` | `io::Result<UnixDatagram>` |
| `sockat::datagram::connect(&sock, path)` | `sock.connect(path)` | `io::Result<()>` |

後始末と診断が要る時は、それぞれの `_ex` を使う ([後始末と診断つきの _ex](#後始末と診断つきの-_ex))。綴りを自分で std や他のライブラリに渡す時は resolve を使う ([resolve](#resolve))。

```rust
use std::io::{Read, Write};

let path = "/とても/深い/ディレクトリ/.../server.sock";
let listener = sockat::bind(path)?;
let mut client = sockat::connect(path)?;
let (mut server, _) = listener.accept()?;
client.write_all(b"ping")?;
let mut buf = [0; 4];
server.read_exact(&mut buf)?;
```

- std が受け付ける長さのパスは std の関数をそのまま呼ぶ。何も足さず、環境変数も読まない (`connect_nonblocking` は std に無いので、std と同じアドレスで自前のソケットを connect する)
- ソケットファイルは渡したパスにできる。listener を閉じても消えない (std と同じ)
- 自前で作るソケットは std と同じ設定 (close-on-exec、std が付ける OS (macOS / iOS、FreeBSD、NetBSD、DragonFly) では `SO_NOSIGPIPE`、std と同じ listen の backlog)

## connect と connect_nonblocking

- **`connect`** は std の `UnixStream::connect` の代わりに使う。ブロックする
- **`connect_nonblocking`** は async ランタイムで使う。ソケットを connect の前に非ブロッキングにし、そのまま返す。tokio の `UnixStream::from_std` にそのまま渡せる。相手の backlog が満杯なら待たずに失敗する (Linux は `EAGAIN`、macOS は `ECONNREFUSED`)。Unix domain の connect は呼び出しの中で終わるか失敗し、sockat の後片付けは unlink だけなので、待つものは無い

## 長いパスへの届き方

Rust の std が受け付ける長さは `sun_path` − 1 バイト。

| OS | std にそのまま渡す長さ |
|---|---|
| Linux / Android / illumos | 107 バイトまで |
| macOS / iOS 等 Apple の OS / FreeBSD / NetBSD / OpenBSD | 103 バイトまで |

それより長いパスは、元のパスと同じ所を指す短い綴りでカーネルに渡す。置き場 B・D・E、経路、試す順番は仕様の [長いパスの経路と試す順番](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスの経路と試す順番) と [経路の作り方](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md)、どこまで届くかは [届く範囲の表](https://github.com/kawaz/sockat/blob/main/pages/reach/README-ja.md) にある。sockat-rs は仕様のすべての経路を作り、OS ごとに仕様の [bind の順](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#bind-の順) と [connect の順](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#connect-の順) のとおりに試す。

- B の場所をアプリが決める時は [`Options::base`](#options) を渡す
- 通った経路と飛ばした経路は `_ex` の [診断](#診断) で分かる

### NUL を含むパス

先頭以外に NUL を含むパスは、長さによらず何も作らずに `InvalidInput` を返す。Linux / Android で先頭が NUL のパス (abstract namespace) は長さによらず std にそのまま渡す (std は `InvalidInput` を返す。abstract の名前は std の `SocketAddr::from_abstract_name` と `bind_addr` / `connect_addr` で扱う)。

### 空のパス

空のパスは、どの関数でも std に渡さず、何も作らずに `InvalidInput` を返す (`ExError` の `route` は `None`、`skipped` は空)。Linux の std は空のパスの bind を名前の無い abstract のアドレスに autobind する (macOS は `EINVAL`) が、それは渡したパスのソケットではないため。

## 綴りの性質 Spelling

`bind_ex` と `resolve_for_bind` は、bind の綴りを後で使えるかを `Spelling` で返す。分岐はこれで行う。値と属性の意味は仕様の [綴りの性質 `Spelling`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#綴りの性質-spelling)。

| 値 | 綴り | 経路 |
|---|---|---|
| `Original` | 元の絶対パス | standard |
| `CwdRelative` | 元の相対パス。cwd を変えると別の所を指す | standard |
| `Linked { .. }` | B・D・E を通り、元のパスと同じ所を指す | link-dir、link-file |
| `Dangling` | 片付け済みの E を含む。どこにも届かない | thread-cwd、at |

`Linked` の属性:

| 属性 | 型 | 中身 |
|---|---|---|
| `removable` | `bool` | 綴りで unlink すると実体が消えるか。link-dir は true、link-file は false (E が消える) |
| `until` | `Until` | `Until::Process` (procB: プロセスが終われば辿れない) か `Until::Cleanup` (B の実パス: E を片付けるまで) |
| `usable_cross_uid` | `bool` | 別の uid のプロセスが辿れるか |
| `usable_with_private_tmp` | `bool` | 自分か相手が `/tmp` を隔離していても (systemd の `PrivateTmp` 等) 相手が辿れるか |

`Spelling` と `Spelling::Linked` は `#[non_exhaustive]`。match には知らない値を受ける `_` の腕と、`Linked { removable, .. }` の `..` が要る。知らない値は「綴りを当てにしない」と読む。

## Options

`_ex` と resolve は `sockat::Options` を取る。`Options::default()` は既定の関数と同じ。各項目の約束は仕様の [`Options`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#options)。

| 項目 | 中身 |
|---|---|
| `usable_cross_uid: bool` | bind の綴りを別の uid も辿れる物にする。満たせなければ thread-cwd / at (`Dangling`) に進む。bind 系だけが見て、connect 系は見ない |
| `base: Option<PathBuf>` | B そのものの場所 (例 `/tmp/my-app-1000/sockat`)。無ければ作り (mode 0711)、確かめに外れたらエラーを返し ([エラー](#エラー))、自動の置き場に落とさない。bind 系も connect 系も使う。この B は sockat 専用で、他の物を置かない。中の死んだ D は [`sweep_base`](#sweep-と-sweep_base) で消す |

```rust
let options = sockat::Options {
    base: Some("/tmp/my-app-1000/sockat".into()),
    ..Default::default()
};
let bound = sockat::bind_ex(path, options)?;
```

## 後始末と診断つきの _ex

| 関数 | 戻り値 |
|---|---|
| `sockat::bind_ex(path, options)` | `Result<Bound<UnixListener>, ExError>` |
| `sockat::connect_ex(path, options)` | `Result<Connected<UnixStream>, ExError>` |
| `sockat::connect_nonblocking_ex(path, options)` | 同上 (非ブロッキング) |
| `sockat::datagram::bind_ex(path, options)` | `Result<Bound<UnixDatagram>, ExError>` |
| `sockat::datagram::connect_ex(&sock, path, options)` | `Result<Report, ExError>` |

| 型 | 中身 |
|---|---|
| `Bound<S>` | `socket` (std の型)、`file: SocketFile`、`spelling: Spelling`、`route`、`base`、`skipped` |
| `Connected<S>` | `socket`、`route`、`skipped` |
| `Report` | `route`、`skipped` |
| `ExError` | `error: io::Error`、`route` (実際に bind / connect して失敗した経路。無ければ `None`)、`skipped`。`?` で `io::Error` に戻る |

`bind_ex` の listener は、短いパスでも閉じた時にソケットのファイルを消さない。消すのは `SocketFile::remove` だけ。

### 診断

| 項目 | 中身 |
|---|---|
| `route` | 通った経路の名前 |
| `base` | bind の綴りが通る B (`proc`・`tmp`・`runtime`・`tmpdir`・`sockdir`・`given`)。`given` は `Options::base` の B。B を通らない綴り (standard・thread-cwd・at) と connect では無い |
| `skipped` | 試して使えなかった経路 (`Skipped { name, reason }`) を試した順に、経路の名前ごとに 1 件。長いパスでは `standard` が先頭 |

`route`・`base`・`skipped` の名前と `reason` は診断用で、中身を約束しない (経路は増えたり変わったりする)。分岐は `Spelling` で行う。

## 片付け

置き場に何が残り、いつ消えるか (B・D・E、OS の一時ディレクトリの掃除、生きている D が消された時) は仕様の [置き場に残る物](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#置き場に残る物)。Rust では次の口で片付ける。

### SocketFile

`bind_ex` は、bind したソケットのファイルの実パスと `(st_dev, st_ino)` を bind の直後に覚えた `SocketFile` を返す。何を覚え、いつ消し、いつ消さないかは仕様の [`remove`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#remove)。

| メソッド | すること |
|---|---|
| `remove(&self) -> io::Result<()>` | ソケットのファイルを 1 回だけ消し、`cleanup` もする。置き換わっていた・既に無い時は消さずに `Ok(())`。失敗しても `cleanup` まで行い、ファイルの側のエラーを先に、無ければ `cleanup` のエラーを返す |
| `cleanup(&self) -> io::Result<()>` | 綴りが通る E を 1 回だけ消す。ソケットのファイルとソケットは残る |

- どちらも成功した後の呼び出しは何もしない。失敗した後の呼び出しはやり直す。複数のスレッドから同時に呼んでも、消す操作は同時に 1 つ (`ForBind` と、その `socket_file()` を何度か呼んで得た `SocketFile` どうしの間でも)
- Drop では何もしない
- `remove` は listener を閉じない。listener を閉じる前に `remove` を呼び、それから閉じる

```rust
let bound = sockat::bind_ex(path, Default::default())?;
// ... bound.socket で accept する (tokio に渡してもよい) ...
bound.file.remove()?; // listener を閉じる前に
drop(bound.socket);
```

### sweep と sweep_base

| 関数 | すること |
|---|---|
| `sockat::sweep() -> io::Result<()>` | 置き場 1〜3 の既定の名前の B (`/tmp/sockat-<uid>`、`$XDG_RUNTIME_DIR/sockat`、`$TMPDIR/sockat-<uid>`) の中の、持ち主の死んだ D を消す |
| `sockat::sweep_base(path) -> io::Result<()>` | `Options::base` に渡した B に同じことをする。その B を管理するアプリが呼ぶ |

何を消し何を消さないかは仕様の [`sweep`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#sweep)。`sweep_base` のエラーは、形の誤りが `InvalidInput`、symlink が `EPERM`、ディレクトリでなければ `ENOTDIR`、他人の物・group / other が書ける B・途中が他人に書ける時が `EPERM`。

```rust
sockat::sweep()?;
sockat::sweep_base("/tmp/my-app-1000/sockat")?;
```

## resolve

綴りを自分で std や他のライブラリに渡す人向け。綴りはただのパスなので、ソケットの型と操作を問わず使える。約束は仕様の [resolve](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#resolve)。

| 関数 | 綴り | 戻り値 |
|---|---|---|
| `sockat::resolve_for_bind(path, options)` | link-dir の `B/D/E/<ファイル名>`、macOS と NetBSD では link-file の `B/D/E` も | `Result<ForBind, ExError>` (`path`、`spelling`、`route`、`base`、`skipped`、`cleanup()`、`socket_file()`) |
| `sockat::resolve_for_connect(path, options)` | link-file の `B/D/E` (ファイル名の長さによらない) | `Result<ForConnect, ExError>` (`path`、`route`、`skipped`、`cleanup()`) |

- std が受け付ける長さのパスはそのまま返す (`Original` / `CwdRelative`)
- 綴りが収まらない時 (thread-cwd・at でしか届かない長さ、B を作れない) は `ENAMETOOLONG`。その長さに届かせたい時は `bind` / `connect` を使う
- `ForBind::socket_file(&self) -> io::Result<SocketFile>` は、bind した後・listener を閉じる前に呼ぶ。返す `SocketFile` の `remove` はファイルを消して、`ForBind` と同じ E を片付ける。呼ばなければ消す口は無い ([仕様の `ForBind` の `SocketFile`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#forbind-の-socketfile))
- `ForConnect` の綴りは何度でも connect に使え、最後に `cleanup()` する
- `resolve_for_connect` は `usable_cross_uid` を見ない

```rust
let spelled = sockat::resolve_for_bind(path, Default::default())?;
let listener = std::os::unix::net::UnixListener::bind(&spelled.path)?;
let file = spelled.socket_file()?;
// ...
file.remove()?;
drop(listener);

let to = sockat::resolve_for_connect(path, Default::default())?;
for _ in 0..3 {
    let stream = std::os::unix::net::UnixStream::connect(&to.path)?;
    // ...
}
to.cleanup()?;
```

## datagram

`sockat::datagram` は std の `UnixDatagram` の、パスを取る関数と `unbound` を写す。経路と順は stream と同じ。長いパスに bind した送り手には、綴りが辿れる間は受け手が `recv_from` のアドレスへ返信できる (`Spelling::Linked`)。OpenBSD では Rust 1.100 以上が要る ([Rust に固有の注意](#rust-に固有の注意))。

```rust
let server = sockat::datagram::bind(server_path)?;
let client = sockat::datagram::unbound()?;
sockat::datagram::connect(&client, server_path)?;
client.send(b"ping")?;
```

`send_to` と SEQPACKET はまだ無い ([仕様の未確定](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#未確定))。長いパスへ `send_to` したい時は、`resolve_for_connect` の綴りを std の `send_to` に渡す。

## 長いパスで std と違う点

言語によらない違い (カーネルが返すアドレス、運用の道具での見え方、ディレクトリを先に引くこと、状態がアプリの置き場の外に増えること、生きている D が消されうる環境、`PrivateTmp` の相手) は仕様の [長いパスで標準と違う点](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスで標準と違う点)。Rust での形:

- カーネルが返すアドレスは、listener と accept したストリームの `local_addr()`、connect した側の `peer_addr()`、DGRAM の `recv_from()` のアドレス。長いパスでは元のパスでなく bind の綴りになる。後で使えるかは `Spelling` で分かる
- listener を閉じた時は、std の `UnixListener` と同じく、`sockat::bind` の listener も `bind_ex` の listener もソケットのファイルを消さない。消すのは元のパスか `SocketFile::remove()`
- relative の途中で cwd が変わったままなら `Other` のエラー
- OpenBSD の Rust 1.99 以前の std は、アドレスの最後の 1 バイトを落とす ([Rust に固有の注意](#rust-に固有の注意))

## 使ってはいけない書き方

一覧となぜいけないかは仕様の [使ってはいけない書き方](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#使ってはいけない書き方)。Rust での形と代わり:

| 書き方 | Rust での形 | 代わりに |
|---|---|---|
| `Spelling` を見ずに、カーネルが返す綴りでソケットのファイルを消す・`stat` する | `std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())` | 元のパスで `std::fs::remove_file(path)`<br>`SocketFile::remove()` |
| 片付けた後やプロセスが終わった後に、古い綴りを使う | `cleanup()` の後に綴りで connect する | 綴りは `until` の間だけ使う |
| 相対パスで bind し、cwd を変えた後にその相対パスで消す | `sockat::bind("app.sock")` の後の<br>`std::env::set_current_dir(..)` と<br>`std::fs::remove_file("app.sock")` | bind の前に `std::path::absolute` で絶対パスにする<br>`SocketFile::remove()` |
| listener を閉じればソケットのファイルが消えると当てにする | `drop(listener)` だけ | `SocketFile::remove()`<br>元のパスで消す |
| `remove()` で listener も閉じると当てにする | `bound.file.remove()` だけ | listener は自分で閉じる |
| listener を閉じた後に `remove()` を呼ぶ | `drop(bound.socket); bound.file.remove()` | 閉じる前に呼ぶ |
| procB や `usable_cross_uid` が false の綴りを別の uid に辿らせる | `Options::default()` の `bind_ex` の綴りを別の uid のプロセスに渡す | `Options { usable_cross_uid: true, .. }` を渡し、`Spelling` を確かめる |
| `route`・`base`・`skipped`・`reason` の文字列で分岐する | `if bound.route == "link-file"` | `Spelling` で分岐する |

避け方 (crate の doc の試験で動かしている):

```rust
use std::io;
use std::path::Path;

fn serve(path: &Path) -> io::Result<()> {
    let path = std::path::absolute(path)?; // bind の前に絶対パスに
    let bound = sockat::bind_ex(&path, Default::default())?;
    if let sockat::Spelling::Linked { removable: false, .. } = bound.spelling {
        // local_addr() の綴りで消すと E が消えるだけ。元のパスか remove() で消す
    }
    // ... bound.socket で accept する ...
    bound.file.remove()?; // listener を閉じる前に
    drop(bound.socket);
    Ok(())
}
```

## エラー

状況ごとのエラーは仕様の [エラー](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#エラー)。Rust での型:

- 既定の関数は `io::Error`、`_ex` と resolve は `ExError` (`error: io::Error`、`route`、`skipped`。`?` で `io::Error` に戻る)
- OS のエラーは書き換えず、`io::Error::raw_os_error()` で errno を取り出せる
- 仕様の「引数の誤り」は `io::ErrorKind::InvalidInput`、「一般のエラー」は `io::ErrorKind::Other`

Rust の口に固有のエラー:

| 状況 | エラー |
|---|---|
| 長いパスで、ソケットのディレクトリの実パスを引けない | そのエラー |
| `Options::base` の確かめに通った後、B を作れない | 作れなかったエラー |
| 相対パスの `bind_ex` の途中で cwd が変わった | `Other` |
| `bind_ex` が bind した直後、そこ (最後の要素が symlink なら辿った先) にソケットでない物がある。`socket_file()` が辿った先がソケットでない | `ENOTSOCK` |
| `bind_ex` が bind した直後に、覚える所 (仕様の [`remove`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#remove)) の実パスを引けない (ファイルが消された、元のパスを引き直す経路で途中の symlink が張り替わった先に何も無い等) | 実パスを引けなかったエラー (`ENOENT` 等) |
| `socket_file()` を bind の前や片付けた後に呼んだ | 綴りを辿れなかったエラー |
| `cleanup` / `remove` で E を消せない (既に無い時を除く) | `unlink` のエラー |

`ExError` の `route` と `skipped`:

- ディレクトリを先に引くので、`ENOENT` と `ENOTDIR` は経路を試す前に決まる (`route` は `None`、`skipped` は空)
- `Options::base` の確かめは経路の前に行い、外れたら `route` は `None`、`skipped` は空。確かめに通った後に B を作れなかった時は、`route` は `None`、`skipped` はそれまでに飛ばした経路
- 実際に bind / connect して失敗した経路は `route` に入る

## async ランタイム

戻り値は std の型なので、std のソケットを受け取れるランタイムならどれでも使える。tokio の例:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` は相手を待たず、後片付けも unlink だけなので、runtime の worker から直接呼べる。`bind` も相手を待たない (thread-cwd ではスレッドの終わりを待つが、bind してすぐ終わる)。`bind_ex` の listener も同じく渡せて、`SocketFile` は手元に残る。

## Rust に固有の注意

- **OpenBSD の Rust 1.99 以前の std はアドレスの最後の 1 バイトを落とす。** `local_addr()`・`peer_addr()`・`recv_from()` が返すパスの最後の 1 バイトが欠ける (std の不具合で、sockat に関係なく短いパスでも同じ。[rust-lang/rust#118925](https://github.com/rust-lang/rust/issues/118925)、Rust 1.100.0 で修正)。カーネルが返すアドレスは欠けていない。std の `recv_from` のアドレスへ DGRAM を返信するには Rust 1.100 以上が要る
- **Linux の std は空のパスの bind を abstract のアドレスに autobind する。** sockat は空のパスを std に渡さず `InvalidInput` にする ([空のパス](#空のパス))
- **std はパスで渡した abstract の名前 (先頭が NUL) を受け付けない。** `bind`・`bind_ex` は std に渡し、std の `InvalidInput` を返す。abstract の名前は std の `SocketAddr::from_abstract_name` と `bind_addr` で bind する。`resolve_for_bind` に渡した abstract の名前はそのまま返り、その `socket_file()` はファイルを持たない `SocketFile` を返す (`remove()` はファイルを消さない。仕様の [`remove`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#remove))

## 確かめていないこと

- Android は compile もしていない。Linux と同じ経路を使う。Android のアプリは seccomp の filter の下で動くので、スレッド cwd は使われない見込み
- NetBSD・OpenBSD・illumos・FreeBSD は CI の VM で試験を回す。peer の資格情報は確かめていない
- ブロックしない connect が backlog の満杯で返すエラーと、connect が呼び出しの中で終わることは、Linux と macOS でだけ確かめた
- macOS 以外の Apple の OS (iOS 等) は macOS と同じ経路を使うが、確かめたのは iOS の clippy だけ。`pthread_fchdir_np` があるか、bind が symlink を辿るかも確かめていない
- FreeBSD の connect で link-file が使えるか (connect は最後の要素の symlink を辿る前提)
