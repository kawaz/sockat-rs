# sockat

> [English](./README.md) | 日本語

`sun_path` の上限 (NUL 込みで macOS と BSD は 104 バイト、Linux は 108 バイト) を超えるパスでも、Unix domain socket を bind / connect できる Rust のライブラリ。

言語によらない振る舞いの正本は [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) で、sockat-rs はその Rust 実装。

```rust
let listener = sockat::bind("/とても/深い/ディレクトリ/.../server.sock")?;
let stream = sockat::connect("/とても/深い/ディレクトリ/.../server.sock")?;
```

## 使い方

std の `UnixListener::bind` / `UnixStream::connect` の代わりに `sockat::bind` / `sockat::connect` を使う。非同期のコードでは、待たずに非ブロッキングの stream を返す `sockat::connect_nonblocking` を使う。

- 戻り値は std の `UnixListener` / `UnixStream` そのもの
- std が受け付ける長さのパスは std にそのまま渡すので、std と同じに振る舞う
- 長いパスでも、ソケットファイルは渡したパスにできる
- `connect` も fork-direct の手前の経路までは試すので、普通の環境では長いパスでも届く
- ソケットファイルを後で消したい時は `sockat::bind_ex` を使う。listener と、bind したファイルだけを消す `SocketFile` を返す

## なぜ要るか

std の `bind` / `connect` は `sun_path` に収まらないパスを断る。ファイルシステムには上限が無いのに、深い一時ディレクトリ (macOS の `$TMPDIR` はそれだけで 50 バイト前後、試験の枠組みやサンドボックスがさらに掘る) にソケットを置くとすぐに当たる。上限が効くのは `bind(2)` / `connect(2)` に渡す引数の長さなので、sockat は同じ場所を指す短い綴りでカーネルに渡す。

## 長いパスへの届き方

std が受け付ける長さ (`sun_path` − 1) のパスは std にそのまま渡す。

| OS | std にそのまま渡す長さ |
|---|---|
| Linux / Android / illumos | 107 バイトまで |
| macOS / iOS / FreeBSD / NetBSD / OpenBSD | 103 バイトまで |

それを超えるパスの bind は**D を通す経路**で届く。自分専用のディレクトリ D (`sockat.XXXXXXXXXX`、mode 0700) を作って fd で掴み、その中に乱数の名前の symlink R (ソケットのディレクトリを指す) を作り、D の fd を基準に `R/<ファイル名>` で bind して、R と D を消す。カーネルが覚える綴りは `R/<ファイル名>` で、R はもう無いので、後でどこで解決されても何にも当たらない。経路の名前は [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) の「経路の名前」と同じ。

| OS | bind (この順) |
|---|---|
| Linux / Android | thread-cwd → proc → fork |
| macOS / iOS 等 Apple の OS | thread-cwd → fork |
| FreeBSD | at |
| NetBSD / OpenBSD / illumos / その他の Unix | fork |
| Windows | 対象外 (compile エラー) |

- **thread-cwd**: 使い捨てのスレッドだけ cwd を D に移す (Linux は `unshare(CLONE_FS)` と `fchdir`、Apple の OS は `pthread_fchdir_np`)。副作用が無いので、どの関数も試す。seccomp の filter が掛かった Linux では使わない
- **proc**: `/proc/self/fd/<D の fd>/R/<ファイル名>` で渡す
- **at**: `bindat` / `connectat` に D の fd を渡す
- **fork**: D に `fchdir` した子を fork し、子が bind する。bind の後の listen は呼び出し元がするので、相手から見える副作用が無い
- D は `TMPDIR`、`/tmp`、ソケットのディレクトリ (`.sockat.XXXXXXXXXX`) の順に、作れた所に作る。置き場は安全でなくても深くてもよい (fd で掴む)。どこにも作れなければ、bind はソケットのディレクトリに R を作り、同じ順 (thread-cwd-sockdir → proc-sockdir → fork-sockdir、FreeBSD は at-sockdir) で試す

connect は、書き込み (D・R・symlink を作る) もスレッドも要らない経路を先に試し、D を通す経路 (thread-cwd・proc・at、D に置く symlink を通す proc-held) と置き場の symlink をその後に試す。前の経路で届けば D も symlink も作らない。connect の綴りはカーネルに残らないので、D を通す経路の他は綴りにランダムな部品を挟まない。D は 1 回の呼び出しで 1 度だけ作り、D を使う経路で共有する。

| OS | connect (この順) |
|---|---|
| Linux / Android (`/proc` あり) | proc-direct → thread-cwd → proc → proc-held → symlink |
| Linux / Android (`/proc` 無し) | relative → thread-cwd → symlink |
| macOS / iOS 等 Apple の OS | relative → widened → thread-cwd → symlink |
| FreeBSD | at-direct → at |
| NetBSD / OpenBSD / illumos / その他の Unix | relative → symlink |

- **proc-direct**: `/proc/self/fd/<ディレクトリの fd>/<ファイル名>` で渡す
- **relative**: cwd からの相対パスが std にそのまま渡せる長さなら、それで渡す。パスを文字列で書き直すだけで、cwd は変えない
- **widened**: macOS のカーネルが受け付ける、253 バイトまで入る sockaddr で渡す (広げた形で bind したソケットのアドレスは途中で切れ、読む側を壊すので、bind は使わない)
- **at-direct**: `connectat` にソケットのディレクトリの fd とファイル名を渡す
- **proc-held**: D の中にソケットのファイルを指す symlink を作り、`/proc/self/fd/<D の fd>/s` で渡す
- **symlink**: `TMPDIR` か `/tmp` の、他人が差し替えられない置き場に、ソケットのファイルを指す短い symlink を作り、それを通して渡して、すぐ消す

`connect_any` 系は、それでも届かない時に最後に fork する (fork-direct)。どの経路も呼び出し元のプロセスとスレッドの cwd を変えないので、マルチスレッドのプログラムから使える。

`connect` / `connect_nonblocking` は fork しない。fork でしか届かない時は、パスが `sun_path` に入らない普通の connect と同じく `ENAMETOOLONG` を返す。そうなるのは、D を通す経路を connect に使えず (NetBSD / OpenBSD / illumos、seccomp の filter が掛かり `/proc` の無い Linux、`pthread_fchdir_np` の無い Apple の OS)、D を通さない経路も届かない時だけである。`/tmp` が使えれば、`connect` はディレクトリの深さにもファイル名の長さにもよらず届く。各経路の長さの上限と、どこから届かなくなるかは [docs/MANUAL-ja.md](./docs/MANUAL-ja.md#長いパスへの届き方) にある。

## 例外: any 系

`bind_any` / `connect_any` / `connect_any_nonblocking` は、既定の経路が全部使えないよほど珍しい環境で、何かを失ってでも届かせたい時だけのもの。

- `bind_any`: `bind` と同じ順の後、最後に置き場 (`XDG_RUNTIME_DIR`、`TMPDIR`、`/tmp`) にソケットのディレクトリを指す symlink を作って bind する。共有の置き場 (`/tmp` 等) では、カーネルが返すアドレスの名前を他の利用者が作り直せる
- `connect_any` / `connect_any_nonblocking`: `connect` と同じ順の後、最後に fork した子に connect させる (子はすぐ終了する)。サーバに見える peer の pid は、もう終了した子の pid になる
- 戻り値 (`AnyListener` / `AnyConnection`) は std の listener / stream と、失ったものの並び `losses` (`respelled` / `forked` / `recreatable-spelling` / `cwd-relative`)、通った経路の名前 `route`、試して使えなかった経路の並び `skipped` (名前と理由) を持つ。失敗した時の `AnyError` も `skipped` を持ち、`io::Error` に `?` で戻る (errno はそのまま)。`route` と `skipped` は診断用で、中身を約束しない

既定では無効で、feature `any` を有効にした時だけ使える: `sockat = { version = "0.4", features = ["any"] }`。詳しくは [docs/MANUAL-ja.md](./docs/MANUAL-ja.md#例外-any-系)。

## 長いパスで std と違う点

- `local_addr()` / `peer_addr()` は元のパスを返さず、bind の時にカーネルに渡した綴り (`R/<ファイル名>`、`/proc/self/fd/<fd>/R/<ファイル名>` 等) を返す。R はもう無いので何にも当たらない。元のパスは自分で持っておく
- bind できるファイル名は R の分だけ短い (Linux / illumos 96 バイト、Apple の OS / BSD 92 バイトまで)
- std にそのまま渡せない長さのパスで、名前で終わらないもの (`dir/`、`dir/.`、`..` で終わるもの) は `InvalidInput` になる
- その他 (fork の副作用、ディレクトリを先に引くこと、途中でプロセスが死ぬと D や R が残りうること) は [docs/MANUAL-ja.md](./docs/MANUAL-ja.md#長いパスで-std-と違う点)

## ソケットのファイルを消す

`bind_ex` は listener と `SocketFile` を返す。`SocketFile::discard()` は、bind の直後に覚えたソケットのファイルを、そのパスの `(st_dev, st_ino)` が同じ時だけ消す (後から同じパスに bind された別のソケットは消さない)。listener は std の型のままで tokio 等へ渡せ、閉じるのは利用者。

```rust
let (listener, file) = sockat::bind_ex(&path)?;
// ...
drop(listener);
file.discard()?;
```

## async ランタイム

依存は libc だけで、ランタイムには依存しない。tokio なら std の型から変換する:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

## ドキュメント

- [docs/MANUAL-ja.md](./docs/MANUAL-ja.md) — 使い方、経路の詳細、std と違う点、エラー
- [docs/decisions/](./docs/decisions/INDEX.md) — 設計の判断 (DR)

## ライセンス

MIT License, Yoshiaki Kawazu (@kawaz)
