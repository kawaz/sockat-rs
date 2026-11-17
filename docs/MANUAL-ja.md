# sockat マニュアル

> [English](./MANUAL.md) | 日本語

sockat の使い方と、長いパスで std と違う点のリファレンス。設計の判断と理由は [DR-0001](./decisions/DR-0001-long-socket-path-routes.md) にある。

## 使い方

```toml
[dependencies]
sockat = "0.1"
```

関数は 2 つだけ。

| 関数 | 対応する std | 戻り値 |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listen 済み) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |

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

- パスが 100 バイト以内なら std の関数をそのまま呼ぶ
- ソケットファイルは渡したパスにできる。listener を閉じても消えない (std と同じ)。消す時は元のパスで `std::fs::remove_file(path)` する (unlink には `sun_path` の上限が無い)
- 自前で作るソケットは std と同じ設定 (close-on-exec、macOS では `SO_NOSIGPIPE`、std と同じ listen の backlog)

## 長いパスへの届き方

100 バイトを超えるパスは、プラットフォームごとに次の方法で届く。どれも呼び出し元のプロセスの cwd を変えない。

### Linux / Android

ディレクトリを `O_PATH` で開き、`/proc/self/fd/<fd>/<ファイル名>` を std に渡す。ディレクトリ部分の長さに上限は無い。`/proc` が要る。

### macOS

次の順に試し、最初に使えたもので bind / connect する。

1. **cwd からの相対パス** (100 バイト以内なら)。パスが cwd (`getcwd`、または cwd と同じディレクトリを指す `$PWD`) の下にあれば前を削る。そうでなければ両方を実パスに揃えて `..` で上る綴りを作る
2. **広げた sockaddr** (253 バイトまで)。XNU は `sun_path` が 104 バイトを超えるアドレスを 255 バイトまで受け付ける。元のパス、無理なら相対パス
3. **短い symlink 経由**。`${TMPDIR:-/tmp}/sockat.XXXXXXXXXX` → ソケットのディレクトリ、の symlink を作り、それを通したパスで bind / connect して、すぐ消す
4. **fork**。ディレクトリの fd に `fchdir` した子が、ファイル名だけで bind / connect する

ある方法が実際に失敗した時 (`ENOENT`、`EADDRINUSE` 等) は、そのエラーを返して次を試さない。次へ進むのは、その方法自体が使えない時 (長さが足りない、symlink が作れない等) だけ。

### FreeBSD

`bindat(2)` / `connectat(2)` で、ディレクトリの fd を基準にファイル名で bind / connect する。

### その他の Unix

fork (macOS の 4 と同じ)。

### Windows

対象外。Unix 以外では compile エラーになる。

## 長いパスで std と違う点

### カーネルが返すアドレスは元のパスではない

`local_addr()` (listener と accept したストリーム) と `peer_addr()` (connect したストリーム) は、実際にカーネルに渡した綴りを返す。

| 経路 | 返るもの |
|---|---|
| Linux | `/proc/self/fd/<fd>/<名前>` (fd はもう閉じている) |
| 相対パス | 相対パス |
| 広げた sockaddr | 途中で切れたパス |
| symlink | もう無い symlink を通したパス |
| fork / bindat | ファイル名だけ |

元のパスは自分で持っておき、それを使う。

### fork の経路では、connect 側の pid が子に見える

fork の経路 (macOS の 4、その他の Unix) で connect すると、サーバから見える peer は短命の子になる。macOS の `LOCAL_PEERPID` は子の pid を返し、クライアントが send した後はクライアントの pid を返す。uid / gid はどちらでもクライアントのもの。Linux は fork しない。

### fork した子は一瞬、全 fd の複製を持つ

fork の性質として、子は終了するまでプロセスの全 fd の複製を持つ。その瞬間に他のスレッドが閉じたソケットやパイプは、子が終わるまで開いたままになる (相手が EOF を見るのが少し遅れる。閉じた listener がさらに接続を受けうる)。fork の経路は、macOS ではディレクトリを `O_SEARCH` で開くので、通常の bind / connect と同じ権限 (検索、bind なら書き込みも) で足りる。他の Unix の fork と FreeBSD の bindat は `O_RDONLY` で開くので、ディレクトリの読み取り権限も要る。

### ディレクトリは呼ぶ少し前に引かれる

Linux・相対パス・bindat・fork の経路は、ディレクトリを先に引いて (開く、または実パスにする) から、ファイル名で bind / connect する。その間に途中のディレクトリが rename されたり symlink が張り替えられたりすると、引いた時点のディレクトリにソケットができる。

### 相対パスの経路は cwd に依存する (macOS)

相対パスを作る前に、それが同じディレクトリを指すことを確かめ、bind / connect の後に cwd が変わっていないことも確かめる。他のスレッドが間に cwd を変えていたら、結果を捨ててエラーを返す (できたかもしれないソケットファイルは、場所が分からないので消さない)。変えて戻した場合は見えない。bind / connect の間に cwd を変えるプログラムはそもそもプロセス中の全ての相対パスと競合していて、sockat はそれを安全にはせず、見える場合を検出するだけである。

### symlink が残ることがある (macOS)

symlink を作ってから消すまでの間にプロセスが死ぬと、`${TMPDIR:-/tmp}/sockat.XXXXXXXXXX` (ソケットのディレクトリを指す) が残る。名前が `sockat.` で始まるので、sockat の残骸と分かる。`TMPDIR` が相対パスの時は symlink の経路を使わない。

## エラー

| 状況 | エラー |
|---|---|
| ディレクトリが無い | `ENOENT` |
| connect 先が無い | `ENOENT` |
| bind 先に既にファイルがある | `EADDRINUSE` |
| 誰も listen していないソケットファイル | `ECONNREFUSED` |
| ファイル名だけでどの経路にも収まらない (Linux でおよそ 90、macOS で 253 バイト超) | `ENAMETOOLONG` |
| 名前で終わらないパス (`dir/`、`dir/.`、`..`)、NUL を含むパス | `InvalidInput` |
| 相対パスの経路の途中で cwd が変わった (macOS) | `Other` |
| Linux で `/proc` が無い | `/proc` が要ると言うエラー |

エラーは `std::io::Error` で、OS のエラーは `raw_os_error()` で errno を取れる。

## async ランタイム

戻り値は std の型なので、std のソケットを受け取れるランタイムならどれでも使える。tokio の例:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect(&path)?;
stream.set_nonblocking(true)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`bind` / `connect` は std と同じくブロックする。ローカルの `connect(2)` は listener の backlog が満杯でない限りすぐ返るが、それも runtime の worker から外したいなら `spawn_blocking` の中で呼ぶ。
