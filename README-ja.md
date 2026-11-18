# sockat

> [English](./README.md) | 日本語

`sun_path` の上限 (NUL 込みで macOS と BSD は 104 バイト、Linux は 108 バイト) を超えるパスでも、Unix domain socket を bind / connect できる Rust のライブラリ。

```rust
let listener = sockat::bind("/とても/深い/ディレクトリ/.../server.sock")?;
let stream = sockat::connect("/とても/深い/ディレクトリ/.../server.sock")?;
```

## 使い方

std の `UnixListener::bind` / `UnixStream::connect` の代わりに `sockat::bind` / `sockat::connect` を使う。非同期のコードでは、待たずに非ブロッキングの stream を返す `sockat::connect_nonblocking` を使う。

- 戻り値は std の `UnixListener` / `UnixStream` そのもの
- 100 バイト以内のパスは std にそのまま渡すので、短いパスでは std と同じに振る舞う
- 長いパスでも、ソケットファイルは渡したパスにできる
- `connect` も一時ディレクトリの短い symlink を経由する経路までは試すので、普通の環境では長いパスでも届く

## なぜ要るか

std の `bind` / `connect` は `sun_path` に収まらないパスを断る。ファイルシステムには上限が無いのに、深い一時ディレクトリ (macOS の `$TMPDIR` はそれだけで 50 バイト前後、試験の枠組みやサンドボックスがさらに掘る) にソケットを置くとすぐに当たる。上限が効くのは `bind(2)` / `connect(2)` に渡す引数の長さなので、sockat は同じ場所を指す短い綴りでカーネルに渡す。

## 長いパスへの届き方

100 バイト以内のパスは、どの OS でも std にそのまま渡す。それを超えるパスは、OS ごとに次の順に試し、最初に使えた経路で bind / connect する。

| OS | 試す順番 |
|---|---|
| Linux / Android (`/proc` を使える) | /proc |
| Linux / Android (`/proc` を使えない) | 相対パス → symlink → fork |
| FreeBSD | bindat |
| macOS | 相対パス → 広げた sockaddr → symlink → fork |
| NetBSD / OpenBSD / illumos / その他の Unix | 相対パス → symlink → fork |
| Windows | 対象外 (compile エラー) |

- **/proc**: ディレクトリを開き、`/proc/self/fd/<fd>/<ファイル名>` で渡す。`/proc` が無い時と、ファイル名がこれに収まらない時 (およそ 90 バイト超) は使えない
- **bindat**: `bindat(2)` / `connectat(2)` で、ディレクトリの fd を基準にファイル名で渡す
- **相対パス**: cwd からの相対パスが 100 バイト以内なら、それで渡す。パスを文字列で書き直すだけで、cwd は変えない
- **広げた sockaddr**: macOS のカーネルが受け付ける、253 バイトまで入る sockaddr で渡す
- **symlink**: `TMPDIR` か `/tmp` に短い symlink を作り、それを通して渡す。bind はソケットのディレクトリを、connect はソケットのファイルを指す。他人が差し替えられない置き場にだけ作り、すぐ消す
- **fork**: ディレクトリに `fchdir` した子を fork し、子がファイル名だけで bind / connect する。**`bind` と `connect_any` 系だけが試す**

どの経路も呼び出し元のプロセスの cwd を変えないので、マルチスレッドのプログラムから使える。

`connect` / `connect_nonblocking` は fork しない。fork でしか届かない時は、パスが `sun_path` に入らない普通の connect と同じく `ENAMETOOLONG` を返す。そうなるのは、`/proc` の綴りにも相対パス (macOS では広げた sockaddr も) にも収まらず、安全に symlink を作れる置き場も無い時だけである。`/tmp` が使えれば、`connect` はディレクトリの深さにもファイル名の長さにもよらず symlink の経路で届く。各経路の長さの上限と、どこから `connect` では届かなくなるかは [docs/MANUAL-ja.md](./docs/MANUAL-ja.md#長いパスへの届き方) にある。

## 例外: `connect_any`

`connect_any` / `connect_any_nonblocking` は、上の `ENAMETOOLONG` に当たるよほど珍しい環境で、fork の副作用を受け入れてでもつなぎたい時だけのもの。他の経路が使えない時は、fork した子に connect させ (子はすぐ終了する)、fork の経路を通ったかも返す (`(UnixStream, bool)`、通った時に `true`)。ログに残す・警告するなどに使える。

fork の経路で connect すると、サーバに見える peer の pid は、もう終了したプロセス (fork した子) の pid になる。詳しくは [docs/MANUAL-ja.md](./docs/MANUAL-ja.md#例外-connect_any)。

## 長いパスで std と違う点

- `local_addr()` / `peer_addr()` は元のパスを返さず、bind の時にカーネルに渡した綴り (相対パス、`/proc/self/fd/...`、もう無い symlink を通したパス等) を返す。元のパスは自分で持っておく
- 100 バイトを超えるパスで、名前で終わらないもの (`dir/`、`dir/.`、`..` で終わるもの) は `InvalidInput` になる
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
