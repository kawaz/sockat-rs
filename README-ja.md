# sockat

> [English](./README.md) | 日本語

`sun_path` の上限 (macOS 104 / Linux 108 バイト、NUL 込み) を超えるパスでも、Unix domain socket を bind / connect できる Rust のライブラリ。

```rust
let listener = sockat::bind("/とても/深い/ディレクトリ/.../server.sock")?;
let stream = sockat::connect("/とても/深い/ディレクトリ/.../server.sock")?;
```

戻り値は std の `UnixListener` / `UnixStream` そのもの。100 バイト以内のパスは std にそのまま渡すので、短いパスでは std と同じに振る舞う。

基本は std の `bind` / `connect` の代わりに `sockat::bind` / `sockat::connect` を使えばよい。非同期のコードでは、待たずに非ブロッキングの stream を返す `sockat::connect_nonblocking` を使う。`connect` も一時ディレクトリの短い symlink を経由する経路までは試すので、普通の環境では長いパスでも届く。

## なぜ要るか

std の `UnixListener::bind` / `UnixStream::connect` は `sun_path` に収まらないパスを断る。ファイルシステムには上限が無いのに、深い一時ディレクトリ (macOS の `$TMPDIR` はそれだけで 50 バイト前後、試験の枠組みやサンドボックスがさらに掘る) にソケットを置くとすぐに当たる。上限が効くのは `bind(2)` / `connect(2)` に渡す引数の長さなので、sockat は短い別の綴りでカーネルに渡す。

## 長いパスへの届き方

| プラットフォーム | 方法 |
|---|---|
| Linux / Android | ディレクトリを `O_PATH` で開き、`/proc/self/fd/<fd>/<名前>` で渡す (`/proc` が無ければ、他の Unix と同じ順) |
| FreeBSD | `bindat(2)` / `connectat(2)` |
| macOS | 順に試す: cwd からの相対パス → 253 バイトまで入る広げた sockaddr → `TMPDIR` か `/tmp` の短い symlink 経由 → ディレクトリに `fchdir` した子を fork |
| 他の Unix (NetBSD、OpenBSD、illumos 等) | 順に試す: cwd からの相対パス → `TMPDIR` か `/tmp` の短い symlink 経由 → ディレクトリに `fchdir` した子を fork |
| Windows | 対象外 |

どれも呼び出し元のプロセスの cwd を変えず、マルチスレッドのプログラムから使える。ソケットファイルは常に渡したパスにできる。symlink は他人が差し替えられない置き場 (`/tmp` のような sticky なディレクトリか、自分専用のディレクトリ) にだけ作る。

fork は `bind` と、次の節の `connect_any` 系だけが使う。`bind` の子は bind するだけで listen は親がするので、listener に fork の副作用は残らない。

## 例外: `connect_any`

`connect` / `connect_nonblocking` は fork しない。fork でしか届かない時は、パスが `sun_path` に入らない普通の connect と同じく `ENAMETOOLONG` を返す。

`connect_any` / `connect_any_nonblocking` は、それでも届かないよほど珍しい環境 (`/proc` も安全な一時ディレクトリも無い Linux、dirfd 基準の connect も使える一時ディレクトリも無い Unix 等、またはファイル名が symlink の経路にも収まらないほど長い時) で、fork の副作用を受け入れてでもつなぎたい時だけのもの。最後は fork した子に connect させて (子はすぐ終了する)、fork の経路を通ったかも返す (`(UnixStream, bool)`、通った時に `true`)。ログに残す・警告するなどに使える。

## 長いパスで std と違う点

- `local_addr()` / `peer_addr()` は元のパスを返さない (実際にカーネルに渡した綴りを返す)。元のパスは自分で持っておく
- `connect_any` 系が fork の経路を通った時は、サーバに見える peer の pid が、もう終了したプロセス (fork した子) の pid になる。ソケット自体は呼び出し元が持っていて普通に使える。uid / gid は呼び出し元と同じ。macOS は呼び出し元が send すると呼び出し元の pid に変わる。その pid からはプロセスの情報も ppid も引けず、番号は別のプロセスに再利用されうる
- 詳しくは [docs/MANUAL-ja.md](./docs/MANUAL-ja.md)

## async ランタイム

依存は libc だけで、ランタイムには依存しない。tokio なら std の型から変換する (`connect_nonblocking` は待たず、非ブロッキングの stream を返す):

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

## ドキュメント

- [docs/MANUAL-ja.md](./docs/MANUAL-ja.md) — 使い方、エラー、注意点
- [docs/decisions/](./docs/decisions/INDEX.md) — 設計の判断 (DR)

## ライセンス

MIT License, Yoshiaki Kawazu (@kawaz)
