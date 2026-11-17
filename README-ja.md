# sockat

> [English](./README.md) | 日本語

`sun_path` の上限 (macOS 104 / Linux 108 バイト、NUL 込み) を超えるパスでも、Unix domain socket を bind / connect できる Rust のライブラリ。

```rust
let listener = sockat::bind("/とても/深い/ディレクトリ/.../server.sock")?;
let stream = sockat::connect("/とても/深い/ディレクトリ/.../server.sock")?;
```

戻り値は std の `UnixListener` / `UnixStream` そのもの。100 バイト以内のパスは std にそのまま渡すので、短いパスでは std と同じに振る舞う。

## なぜ要るか

std の `UnixListener::bind` / `UnixStream::connect` は `sun_path` に収まらないパスを断る。ファイルシステムには上限が無いのに、深い一時ディレクトリ (macOS の `$TMPDIR` はそれだけで 50 バイト前後、試験の枠組みやサンドボックスがさらに掘る) にソケットを置くとすぐに当たる。上限が効くのは `bind(2)` / `connect(2)` に渡す引数の長さなので、sockat は短い別の綴りでカーネルに渡す。

## 長いパスへの届き方

| プラットフォーム | 方法 |
|---|---|
| Linux / Android | ディレクトリを `O_PATH` で開き、`/proc/self/fd/<fd>/<名前>` で渡す (`/proc` が無ければ、ディレクトリに `fchdir` した子を fork) |
| macOS | 順に試す: cwd からの相対パス → 253 バイトまで入る広げた sockaddr → `TMPDIR` か `/tmp` の短い symlink 経由 → ディレクトリに `fchdir` した子を fork |
| FreeBSD | `bindat(2)` / `connectat(2)` |
| 他の Unix | ディレクトリに `fchdir` した子を fork |
| Windows | 対象外 |

どれも呼び出し元のプロセスの cwd を変えず、マルチスレッドのプログラムから使える。ソケットファイルは常に渡したパスにできる。

## 長いパスで std と違う点

- `local_addr()` / `peer_addr()` は元のパスを返さない (実際にカーネルに渡した綴りを返す)。元のパスは自分で持っておく
- fork の経路では、サーバから見える connect 側の pid が子になる (macOS ではクライアントが send するまで、`/proc` の無い Linux ではずっと)
- 詳しくは [docs/MANUAL-ja.md](./docs/MANUAL-ja.md)

## async ランタイム

依存は libc だけで、ランタイムには依存しない。tokio なら std の型から変換する:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;
```

## ドキュメント

- [docs/MANUAL-ja.md](./docs/MANUAL-ja.md) — 使い方、エラー、注意点
- [docs/decisions/](./docs/decisions/INDEX.md) — 設計の判断 (DR)

## ライセンス

MIT License, Yoshiaki Kawazu (@kawaz)
