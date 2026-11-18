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
- `connect` も fork の手前の経路 (一時ディレクトリの短い symlink、Linux と Apple の OS ではスレッドだけ cwd を移す経路) までは試すので、普通の環境では長いパスでも届く

## なぜ要るか

std の `bind` / `connect` は `sun_path` に収まらないパスを断る。ファイルシステムには上限が無いのに、深い一時ディレクトリ (macOS の `$TMPDIR` はそれだけで 50 バイト前後、試験の枠組みやサンドボックスがさらに掘る) にソケットを置くとすぐに当たる。上限が効くのは `bind(2)` / `connect(2)` に渡す引数の長さなので、sockat は同じ場所を指す短い綴りでカーネルに渡す。

## 長いパスへの届き方

std が受け付ける長さ (`sun_path` − 1) のパスは std にそのまま渡す。

| OS | std にそのまま渡す長さ |
|---|---|
| Linux / Android / illumos | 107 バイトまで |
| macOS / iOS / FreeBSD / NetBSD / OpenBSD | 103 バイトまで |

それを超えるパスは、OS ごとに次の順に試し、最初に使えた経路で bind / connect する。

| OS | 試す順番 |
|---|---|
| Linux / Android (`/proc` を使える) | /proc → 相対パス → symlink → スレッド cwd → fork (/proc の綴りが収まる時は /proc だけ) |
| Linux / Android (`/proc` を使えない) | 相対パス → symlink → スレッド cwd → fork |
| FreeBSD | bindat |
| macOS / iOS 等 Apple の OS の `bind` | 相対パス → symlink → スレッド cwd → fork |
| macOS / iOS 等 Apple の OS の `connect` 系 | 相対パス → 広げた sockaddr → symlink → スレッド cwd → fork |
| NetBSD / OpenBSD / illumos / その他の Unix | 相対パス → symlink → fork |
| Windows | 対象外 (compile エラー) |

- **/proc**: ディレクトリを開き、`/proc/self/fd/<fd>/<ファイル名>` で渡す。`/proc` が無い時と、ファイル名がこれに収まらない時 (およそ 90 バイト超) は使えない
- **bindat**: `bindat(2)` / `connectat(2)` で、ディレクトリの fd を基準にファイル名で渡す
- **相対パス**: cwd からの相対パスが std にそのまま渡せる長さなら、それで渡す。パスを文字列で書き直すだけで、cwd は変えない
- **広げた sockaddr**: macOS のカーネルが受け付ける、253 バイトまで入る sockaddr で渡す。connect 系だけが使う (広げた形で bind したソケットのアドレスは途中で切れ、読む側を壊すので、bind は使わない)
- **symlink**: `TMPDIR` か `/tmp` に短い symlink を作り、それを通して渡す。bind はソケットのディレクトリを、connect はソケットのファイルを指す。他人が差し替えられない置き場にだけ作り、すぐ消す。`/proc` を使える Linux の connect は、安全でない置き場にも自分専用のディレクトリを作り、fd で掴んでその中の symlink を通す
- **スレッド cwd**: 使い捨てのスレッドだけ cwd をディレクトリに移し (Linux は `unshare(CLONE_FS)` と `fchdir`、Apple の OS は `pthread_fchdir_np`)、ファイル名だけで bind / connect する。副作用が無いので、どの関数も試す
- **fork**: ディレクトリに `fchdir` した子を fork し、子がファイル名だけで bind / connect する。**`bind` と `connect_any` 系だけが試す**。bind の後の listen は呼び出し元がするので、bind は相手から見える副作用が無い

どの経路も呼び出し元のプロセスの cwd を変えないので、マルチスレッドのプログラムから使える。

`connect` / `connect_nonblocking` は fork しない。fork でしか届かない時は、パスが `sun_path` に入らない普通の connect と同じく `ENAMETOOLONG` を返す。そうなるのは、`/proc` の綴りにも相対パス (Apple の OS では広げた sockaddr も) にも収まらず、symlink を作れる置き場も無く (`/proc` を使える Linux 以外では安全な置き場)、スレッド cwd の経路も使えない時 (NetBSD / OpenBSD / illumos、seccomp の filter が掛かった Linux、`pthread_fchdir_np` の無い Apple の OS) だけである。`/tmp` が使えれば、`connect` はディレクトリの深さにもファイル名の長さにもよらず symlink の経路で届く。各経路の長さの上限と、どこから `connect` では届かなくなるかは [docs/MANUAL-ja.md](./docs/MANUAL-ja.md#長いパスへの届き方) にある。

## 例外: `connect_any`

`connect_any` / `connect_any_nonblocking` は、上の `ENAMETOOLONG` に当たるよほど珍しい環境で、fork の副作用を受け入れてでもつなぎたい時だけのもの。他の経路が使えない時は、fork した子に connect させ (子はすぐ終了する)、fork の経路を通ったかも返す (`AnyConnection { stream, forked }`、通った時に `forked` が `true`)。ログに残す・警告するなどに使える。

既定では無効で、feature `any` を有効にした時だけ使える: `sockat = { version = "0.4", features = ["any"] }`

fork の経路で connect すると、サーバに見える peer の pid は、もう終了したプロセス (fork した子) の pid になる。詳しくは [docs/MANUAL-ja.md](./docs/MANUAL-ja.md#例外-connect_any)。

## 長いパスで std と違う点

- `local_addr()` / `peer_addr()` は元のパスを返さず、bind の時にカーネルに渡した綴り (相対パス、`/proc/self/fd/...`、もう無い symlink を通したパス等) を返す。元のパスは自分で持っておく
- std にそのまま渡せない長さのパスで、名前で終わらないもの (`dir/`、`dir/.`、`..` で終わるもの) は `InvalidInput` になる
- その他 (fork の副作用、ディレクトリを先に引くこと、symlink が残りうること) は [docs/MANUAL-ja.md](./docs/MANUAL-ja.md#長いパスで-std-と違う点)

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
