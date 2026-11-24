# sockat リファレンス

> [English](./REFERENCE.md) | 日本語

各関数と型の API の文書は [docs.rs](https://docs.rs/sockat) にある。このページは sockat-rs (crate `sockat`) の関数と型の一覧、Rust での形、Rust に固有の注意。使い方は [利用マニュアル](./MANUAL-ja.md) にある。

## 仕様

言語によらない振る舞いの正本は [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) で、sockat-rs はその Rust 実装。Rust で決めた所は [DR-0032](./decisions/DR-0032-resolve-through-persistent-base.md) にあり、判断の一覧は [設計の判断 (DR) の索引](./decisions/INDEX.md)。

言語によらない説明は仕様と pages にある:

- [長いパスで標準ライブラリと違う点](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスで標準ライブラリと違う点)
- [使ってはいけない書き方](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#使ってはいけない書き方)
- [置き場に残る物](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#置き場に残る物)
- [`sweep`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#sweep)
- [長いパスの経路と試す順番](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスの経路と試す順番)、[経路の作り方](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md)、[届く範囲の表](https://github.com/kawaz/sockat/blob/main/pages/reach/README-ja.md)
- [エラー](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#エラー)
- [言語ごとの注意](https://github.com/kawaz/sockat/blob/main/pages/langs-ja.md)

## 関数

標準ライブラリの関数の代わりに sockat の関数を使う。長さによらないパスを取り、標準ライブラリの型を返す。

| 関数 | 対応する標準ライブラリの関数 | 戻り値 |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listen 済み) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |
| `sockat::connect_nonblocking(path)` | (無し) | `io::Result<UnixStream>` (非ブロッキング) |
| `sockat::datagram::bind(path)` | `UnixDatagram::bind(path)` | `io::Result<UnixDatagram>` |
| `sockat::datagram::unbound()` | `UnixDatagram::unbound()` | `io::Result<UnixDatagram>` |
| `sockat::datagram::connect(&sock, path)` | `sock.connect(path)` | `io::Result<()>` |

後始末と診断が要る時は、それぞれの `_ex` を使う ([後始末と診断つきの _ex](#後始末と診断つきの-_ex))。綴りを自分で標準ライブラリや他のライブラリに渡す時は resolve を使う ([resolve](#resolve))。

- 標準ライブラリが受け付ける長さの絶対パスは標準ライブラリの関数をそのまま呼ぶ。何も足さず、環境変数も読まない (`connect_nonblocking` は標準ライブラリに無いので、標準ライブラリと同じアドレスで自前のソケットを connect する)
- bind (`bind`・`bind_ex`・`datagram::bind`・`datagram::bind_ex`) と resolve は、相対パスを絶対パスにしてから扱う。connect 系 (`connect`・`connect_nonblocking`・`datagram::connect` と各 `_ex`) の相対パスは、今まで通り相対のまま標準ライブラリに渡す。手順は仕様の [相対パスを絶対パスにする](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#相対パスを絶対パスにする)
- ソケットファイルは渡したパスにできる。listener を閉じても消えない (標準ライブラリと同じ)
- 自前で作るソケットは標準ライブラリと同じ設定 (close-on-exec、標準ライブラリが付ける OS (macOS / iOS、FreeBSD、NetBSD、DragonFly) では `SO_NOSIGPIPE`、標準ライブラリと同じ listen の backlog)

### connect と connect_nonblocking

- **`connect`** は標準ライブラリの `UnixStream::connect` の代わりに使う。ブロックする
- **`connect_nonblocking`** は async ランタイムで使う。ソケットを connect の前に非ブロッキングにし、そのまま返す。tokio の `UnixStream::from_std` にそのまま渡せる。相手の backlog が満杯なら待たずに失敗する (Linux は `EAGAIN`、macOS は `ECONNREFUSED`)。Unix domain の connect は呼び出しの中で終わるか失敗し、sockat の後片付けは unlink だけなので、待つものは無い

## 長いパスへの届き方

Rust の標準ライブラリが受け付ける長さは `sun_path` − 1 バイト。

| OS | 標準ライブラリにそのまま渡す長さ |
|---|---|
| Linux / Android / illumos | 107 バイトまで |
| macOS / iOS 等 Apple の OS / FreeBSD / NetBSD / OpenBSD | 103 バイトまで |

それより長いパスは、元のパスと同じ所を指す短い綴りでカーネルに渡す。置き場 B・D・E、経路、試す順番は仕様の [長いパスの経路と試す順番](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスの経路と試す順番) と [経路の作り方](https://github.com/kawaz/sockat/blob/main/docs/SPEC-routes-ja.md)、どこまで届くかは [届く範囲の表](https://github.com/kawaz/sockat/blob/main/pages/reach/README-ja.md) にある。sockat-rs は仕様のすべての経路を作り、OS ごとに仕様の [bind の順](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#bind-の順) と [connect の順](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#connect-の順) のとおりに試す。

- 長い絶対パスでも、途中の symlink を解いた実パスが標準ライブラリに収まれば、実パスを標準ライブラリに渡し、置き場に何も作らない (`Spelling` は `Original`)。この時 `Options::base` は見ない
- B の場所をアプリが決める時は [`Options::base`](#options) を渡す
- 通った経路と飛ばした経路は `_ex` の [診断](#診断) で分かる

### NUL を含むパス

先頭以外に NUL を含むパスは、長さによらず何も作らずに `InvalidInput` を返す。Linux / Android で先頭が NUL のパス (abstract namespace) は長さによらず標準ライブラリにそのまま渡す (標準ライブラリは `InvalidInput` を返す。abstract の名前は標準ライブラリの `SocketAddr::from_abstract_name` と `bind_addr` / `connect_addr` で扱う)。

### 空のパス

空のパスは、どの関数でも標準ライブラリに渡さず、何も作らずに `InvalidInput` を返す (`ExError` の `route` は `None`、`skipped` は空)。Linux の標準ライブラリは空のパスの bind を名前の無い abstract のアドレスに autobind する (macOS は `EINVAL`) が、それは渡したパスのソケットではないため。

## 綴りの性質 Spelling

`bind_ex`・`resolve_for_bind`・`resolve_for_connect` は、綴りを後で使えるかを `Spelling` で返す。分岐はこれで行う。値と属性の意味は仕様の [綴りの性質 `Spelling`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#綴りの性質-spelling)。

| 値 | 綴り | 経路 |
|---|---|---|
| `Original` | 元の絶対パス、相対パスを絶対にした綴り、長いパスの実パス | standard |
| `CwdRelative` | 元の相対パス。cwd を変えると別の所を指す。相対パスを絶対にしても、どの経路も使えない時だけ ([仕様の手順の 5](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#相対パスを絶対パスにする)) | standard |
| `Linked { .. }` | B・D・E を通り、元のパスと同じ所を指す | link-dir、link-file |
| `Dangling` | 片付け済みの E を含む。どこにも届かない | thread-cwd、at |

`Linked` の属性:

| 属性 | 型 | 中身 |
|---|---|---|
| `removable` | `bool` | 綴りで unlink すると実体が消えるか。link-dir は true、link-file は false (E が消える) |
| `until` | `Until` | `Until::Process` (procB: プロセスが終われば辿れない) か `Until::Cleanup` (B の実パス: E を片付けるまで) |
| `usable_cross_uid` | `bool` | 別の uid のプロセスが辿れるか |
| `usable_with_private_tmp` | `bool` | 自分か相手が `/tmp` を隔離していても (systemd の `PrivateTmp` 等) 相手が辿れるか |

`ForConnect` の `spelling` は `Original`・`CwdRelative`・`Linked` のどれか。`Linked` は link-file の綴りで `removable` は false、procB の綴りは `until` が `Until::Process`。

`Spelling` と `Spelling::Linked` は `#[non_exhaustive]`。match には知らない値を受ける `_` の腕と、`Linked { removable, .. }` の `..` が要る。知らない値は「綴りを当てにしない」と読む。

## Options

`_ex` と resolve は `sockat::Options` を取る。`Options::default()` は既定の関数と同じ。各項目の約束は仕様の [`Options`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#options)。使い方は利用マニュアルの [置き場を自分で決める](./manual/bind-ja.md#置き場を自分で決める)。

| 項目 | 中身 |
|---|---|
| `usable_cross_uid: bool` | bind の綴りを別の uid も辿れる物にする。満たせなければ thread-cwd / at (`Dangling`) に進む。bind 系だけが見て、connect 系は見ない |
| `base: Option<PathBuf>` | B そのものの場所 (例 `/tmp/my-app-1000/sockat`)。無ければ作り (mode 0711)、確かめに外れたらエラーを返し ([エラー](#エラー))、自動の置き場に落とさない。bind 系も connect 系も使う。相対パスを絶対にした綴りや長いパスの実パスが標準ライブラリに収まる時は見ない。この B は sockat 専用で、他の物を置かない。中の死んだ D は [`sweep_base`](#sweep-と-sweep_base) で消す |

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
| `Bound<S>` | `socket` (標準ライブラリの型)、`file: SocketFile`、`spelling: Spelling`、`route`、`base`、`skipped` |
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

`route`・`base`・`skipped` の名前と `reason` は診断用で、中身を約束しない (経路は増えたり変わったりする)。分岐は `Spelling` で行う。使い方は利用マニュアルの [どう届いたかを見る](./manual/connect-ja.md#どう届いたかを見る)。

## 片付け

置き場に何が残り、いつ消えるか (B・D・E、OS の一時ディレクトリの掃除、生きている D が消された時) は仕様の [置き場に残る物](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#置き場に残る物)。Rust では次の口で片付ける。

### SocketFile

`bind_ex` は、bind したソケットのファイルの実パスと `(st_dev, st_ino)` を bind の直後に覚えた `SocketFile` を返す。何を覚え、いつ消し、いつ消さないかは仕様の [`remove`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#remove)。使い方は利用マニュアルの [終わる時にソケットのファイルを消す](./manual/bind-ja.md#終わる時にソケットのファイルを消す)。

| メソッド | すること |
|---|---|
| `remove(&self) -> io::Result<()>` | ソケットのファイルを 1 回だけ消し、`cleanup` もする。置き換わっていた・既に無い時は消さずに `Ok(())`。失敗しても `cleanup` まで行い、ファイルの側のエラーを先に、無ければ `cleanup` のエラーを返す |
| `cleanup(&self) -> io::Result<()>` | 綴りが通る E を 1 回だけ消す。ソケットのファイルとソケットは残る |

- どちらも成功した後の呼び出しは何もしない。失敗した後の呼び出しはやり直す。複数のスレッドから同時に呼んでも、消す操作は同時に 1 つ (`ForBind` と、その `socket_file()` を何度か呼んで得た `SocketFile` どうしの間でも)
- bind の後に cwd を変えても、`remove()` は bind したファイルを消す (`bind_ex` の `SocketFile` も `ForBind::socket_file()` の `SocketFile` も)。cwd の確かめ (bind の前後で cwd が同じか) は `Spelling` が `CwdRelative` の時だけ
- Drop では何もしない
- `remove` は listener を閉じない。listener を閉じる前に `remove` を呼び、それから閉じる

### sweep と sweep_base

| 関数 | すること |
|---|---|
| `sockat::sweep() -> io::Result<()>` | 置き場 1〜3 の既定の名前の B (`/tmp/sockat-<uid>`、`$XDG_RUNTIME_DIR/sockat`、`$TMPDIR/sockat-<uid>`) の中の、持ち主の死んだ D を消す |
| `sockat::sweep_base(path) -> io::Result<()>` | `Options::base` に渡した B に同じことをする。その B を管理するアプリが呼ぶ |

何を消し何を消さないかは仕様の [`sweep`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#sweep)。`sweep_base` のエラーは、形の誤りが `InvalidInput`、symlink が `EPERM`、ディレクトリでなければ `ENOTDIR`、他人の物・group / other が書ける B・途中が他人に書ける時が `EPERM`。使い方は利用マニュアルの [置き場に残った物を片付ける](./manual/bind-ja.md#置き場に残った物を片付ける) と [置き場を自分で決める](./manual/bind-ja.md#置き場を自分で決める)。

## resolve

綴りを自分で標準ライブラリや他のライブラリに渡す人向け。綴りはただのパスなので、ソケットの型と操作を問わず使える。約束は仕様の [resolve](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#resolve)。使い方は利用マニュアルの [パスだけを解決する (resolve)](./manual/resolve-ja.md)。

| 関数 | 綴り | 戻り値 |
|---|---|---|
| `sockat::resolve_for_bind(path, options)` | link-dir の `B/D/E/<ファイル名>`、macOS と NetBSD では link-file の `B/D/E` も | `Result<ForBind, ExError>` (`path`、`spelling`、`route`、`base`、`skipped`、`cleanup()`、`socket_file()`) |
| `sockat::resolve_for_connect(path, options)` | link-file の `B/D/E` (ファイル名の長さによらない) | `Result<ForConnect, ExError>` (`path`、`spelling`、`route`、`skipped`、`cleanup()`) |

- 標準ライブラリが受け付ける長さの絶対パスはそのまま返す (`Original`)
- 相対パスは絶対の綴りで返す (絶対にしたパスか長いパスの実パスが標準ライブラリに収まれば `Original` で置き場に何も作らない。収まらなければ上の B・D・E の綴り)。どの綴りも使えない時だけ、標準ライブラリが受け付ける長さなら元の相対パスを返す (`CwdRelative`)
- 長い絶対パスも、実パスが標準ライブラリに収まれば実パスを返す (`Original`、置き場に何も作らない)
- 綴りが収まらない時 (thread-cwd・at でしか届かない長さ、B を作れない) は `ENAMETOOLONG`。その長さに届かせたい時は `bind` / `connect` を使う
- `ForBind::socket_file(&self) -> io::Result<SocketFile>` は、bind した後・listener を閉じる前に呼ぶ。返す `SocketFile` の `remove` はファイルを消して、`ForBind` と同じ E を片付ける。呼ばなければ消す口は無い ([仕様の `ForBind` の `SocketFile`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#forbind-の-socketfile))
- `ForConnect` の綴りは何度でも connect に使え、最後に `cleanup()` する
- `resolve_for_connect` は `usable_cross_uid` を見ない

## datagram

`sockat::datagram` は標準ライブラリの `UnixDatagram` の、パスを取る関数と `unbound` を写す。経路と順は stream と同じ。長いパスに bind した送り手には、綴りが辿れる間は受け手が `recv_from` のアドレスへ返信できる (`Spelling::Linked`)。OpenBSD では Rust 1.100 以上が要る ([Rust に固有の注意](#rust-に固有の注意))。使い方は利用マニュアルの [DGRAM を使う](./manual/datagram-ja.md)。

`send_to` はまだ無い ([仕様の未確定](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#未確定))。長いパスへ `send_to` したい時は、`resolve_for_connect` の綴りを標準ライブラリの `send_to` に渡す ([長いパスへ send_to する](./manual/datagram-ja.md#長いパスへ-send_to-する))。SEQPACKET 専用の関数は無い。自分で作った SEQPACKET のソケットに resolve の綴りを渡して使う ([SEQPACKET を使う](./manual/seqpacket-ja.md))。

## 長いパスで標準ライブラリと違う点

言語によらない違い (カーネルが返すアドレス、運用の道具での見え方、ディレクトリを先に引くこと、状態がアプリの置き場の外に増えること、生きている D が消されうる環境、`PrivateTmp` の相手) は仕様の [長いパスで標準ライブラリと違う点](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスで標準ライブラリと違う点)。Rust での形:

- 相対パスで bind すると、短くても、`local_addr()`・`peer_addr()`・`recv_from()` のアドレスは渡した相対パスでなく絶対パスになる。深い cwd では、短い相対パスでも置き場に B・D・E ができる (仕様の [長いパスで標準ライブラリと違う点](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#長いパスで標準ライブラリと違う点) の「相対パス:」の行)
- カーネルが返すアドレスは、listener と accept したストリームの `local_addr()`、connect した側の `peer_addr()`、DGRAM の `recv_from()` のアドレス。長いパスでは元のパスでなく bind の綴りになる。後で使えるかは `Spelling` で分かる
- listener を閉じた時は、標準ライブラリの `UnixListener` と同じく、`sockat::bind` の listener も `bind_ex` の listener もソケットのファイルを消さない。消すのは元のパスか `SocketFile::remove()`
- relative の途中で cwd が変わったままなら `Other` のエラー
- OpenBSD の Rust 1.99 以前の標準ライブラリは、アドレスの最後の 1 バイトを落とす ([Rust に固有の注意](#rust-に固有の注意))

## 使ってはいけない書き方

一覧となぜいけないかは仕様の [使ってはいけない書き方](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#使ってはいけない書き方)。Rust での形と代わり:

| 書き方 | Rust での形 | 代わりに |
|---|---|---|
| `Spelling` を見ずに、カーネルが返す綴りでソケットのファイルを消す・`stat` する | `std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())` | 元のパスで `std::fs::remove_file(path)`<br>`SocketFile::remove()` |
| 片付けた後やプロセスが終わった後に、古い綴りを使う | `cleanup()` の後に綴りで connect する | 綴りは `until` の間だけ使う |
| 相対パスで bind し、cwd を変えた後に、渡した相対パスの文字列で消す | `sockat::bind("app.sock")` の後の<br>`std::env::set_current_dir(..)` と<br>`std::fs::remove_file("app.sock")` | `SocketFile::remove()`<br>自分で絶対パスにする時は `std::path::absolute` (`..` を畳まない) |
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
| `CwdRelative` で bind する `bind_ex` の途中で cwd が変わった | `Other` |
| `bind_ex` が bind した直後、そこ (最後の要素が symlink なら辿った先) にソケットでない物がある。`socket_file()` が辿った先がソケットでない | `ENOTSOCK` |
| `bind_ex` が bind した直後に、覚える所 (仕様の [`remove`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#remove)) の実パスを引けない (ファイルが消された、元のパスを引き直す経路で途中の symlink が張り替わった先に何も無い等) | 実パスを引けなかったエラー (`ENOENT` 等) |
| `socket_file()` を bind の前や片付けた後に呼んだ | 綴りを辿れなかったエラー |
| `cleanup` / `remove` で E を消せない (既に無い時を除く) | `unlink` のエラー |

`ExError` の `route` と `skipped`:

- ディレクトリを先に引くので、`ENOENT` と `ENOTDIR` は経路を試す前に決まる (`route` は `None`、`skipped` は空)
- ソケットのディレクトリが無い相対パスの bind / resolve は、短くても `ENOENT` で、`route` は `None`
- `Options::base` の確かめは経路の前に行い、外れたら `route` は `None`、`skipped` は空。確かめに通った後に B を作れなかった時は、`route` は `None`、`skipped` はそれまでに飛ばした経路
- 実際に bind / connect して失敗した経路は `route` に入る

## async ランタイム

戻り値は標準ライブラリの型なので、標準ライブラリのソケットを受け取れるランタイムならどれでも使える。tokio の例は利用マニュアルの [tokio で待ち受ける](./manual/bind-ja.md#tokio-で待ち受ける) と [tokio でつなぐ](./manual/connect-ja.md#tokio-でつなぐ)。

`connect_nonblocking` は相手を待たず、後片付けも unlink だけなので、runtime の worker から直接呼べる。`bind` も相手を待たない (thread-cwd ではスレッドの終わりを待つが、bind してすぐ終わる)。`bind_ex` の listener も同じく渡せて、`SocketFile` は手元に残る。

## Rust に固有の注意

- **OpenBSD の Rust 1.99 以前の標準ライブラリはアドレスの最後の 1 バイトを落とす。** `local_addr()`・`peer_addr()`・`recv_from()` が返すパスの最後の 1 バイトが欠ける (標準ライブラリの不具合で、sockat に関係なく短いパスでも同じ。[rust-lang/rust#118925](https://github.com/rust-lang/rust/issues/118925)、Rust 1.100.0 で修正)。カーネルが返すアドレスは欠けていない。標準ライブラリの `recv_from` のアドレスへ DGRAM を返信するには Rust 1.100 以上が要る
- **Linux の標準ライブラリは空のパスの bind を abstract のアドレスに autobind する。** sockat は空のパスを標準ライブラリに渡さず `InvalidInput` にする ([空のパス](#空のパス))
- **標準ライブラリはパスで渡した abstract の名前 (先頭が NUL) を受け付けない。** `bind`・`bind_ex` は標準ライブラリに渡し、標準ライブラリの `InvalidInput` を返す。abstract の名前は標準ライブラリの `SocketAddr::from_abstract_name` と `bind_addr` で bind する。`resolve_for_bind` に渡した abstract の名前はそのまま返り、その `socket_file()` はファイルを持たない `SocketFile` を返す (`remove()` はファイルを消さない。仕様の [`remove`](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#remove))

- **cwd が PATH_MAX を超えると、相対パスの `bind_ex` は失敗する。** macOS は `ERANGE`、Linux は `ENAMETOOLONG` (`route` は `standard`、listener は閉じる)。ソケットのファイルを絶対の実パスで覚えられないため。既定の `bind` は通る (相対のまま標準ライブラリに渡し、`CwdRelative`)

## 確かめていないこと

- Android は compile もしていない。Linux と同じ経路を使う。Android のアプリは seccomp の filter の下で動くので、スレッド cwd は使われない見込み
- NetBSD・OpenBSD・illumos・FreeBSD は CI の VM で試験を回す。peer の資格情報は確かめていない
- ブロックしない connect が backlog の満杯で返すエラーと、connect が呼び出しの中で終わることは、Linux と macOS でだけ確かめた
- macOS 以外の Apple の OS (iOS 等) は macOS と同じ経路を使うが、確かめたのは iOS の clippy だけ。`pthread_fchdir_np` があるか、bind が symlink を辿るかも確かめていない
- FreeBSD の connect で link-file が使えるか (connect は最後の要素の symlink を辿る前提)
