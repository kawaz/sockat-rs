# sockat マニュアル

> [English](./MANUAL.md) | 日本語

sockat の使い方と、長いパスで std と違う点のリファレンス。設計の判断と理由は [DR-0001](./decisions/DR-0001-long-socket-path-routes.md) にある。

## 使い方

```toml
[dependencies]
sockat = "0.2"
```

基本は std の `bind` / `connect` の代わりに `sockat::bind` / `sockat::connect` を使えばよい。非同期のコードでは `sockat::connect_nonblocking` を使う。

| 関数 | 対応する std | 戻り値 |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listen 済み) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |
| `sockat::connect_nonblocking(path)` | (無し) | `io::Result<UnixStream>` (非ブロッキング) |

この他に、よほど珍しい環境のための `connect_any` / `connect_any_nonblocking` がある ([後述](#例外-connect_any))。

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

## connect と connect_nonblocking

- **`connect`** は std の `UnixStream::connect` の代わりに使う。ブロックする
- **`connect_nonblocking`** は async ランタイムで使う。ソケットを connect の前に非ブロッキングにし、そのまま返す。tokio の `UnixStream::from_std` にそのまま渡せる。相手の backlog が満杯なら待たずに失敗する (Linux は `EAGAIN`、macOS は `ECONNREFUSED`)。Unix domain の connect は呼び出しの中で終わるので、後で待つものは無い

どちらも、一時ディレクトリ (`TMPDIR` か `/tmp`) の短い symlink を経由する経路までは試す。なので普通の環境 (`/tmp` が書ける) では、ファイル名が極端に長くない限り、ディレクトリがどれだけ深くても届く (下の「OS ごとの届き方」)。それ以上の手 (fork) は使わず、届かない時はパスが `sun_path` に入らない普通の connect と同じく `ENAMETOOLONG` を返す。

`bind` は最後は fork でやり切る。子は bind するだけで listen は親がするので、listener に fork の副作用は残らない (子が一瞬 fd の複製を持つことは後述)。

## 例外: connect_any

`connect_any` / `connect_any_nonblocking` は、`connect` でも届かないよほど珍しい環境 (`/proc` も安全な一時ディレクトリも無い Linux、dirfd 基準の connect も使える一時ディレクトリも無い Unix 等、またはファイル名が symlink の経路にも収まらないほど長い時) で、fork の副作用を受け入れてでもつなぎたい時だけのもの。普通は使わない。

| 関数 | 戻り値 |
|---|---|
| `sockat::connect_any(path)` | `io::Result<(UnixStream, bool)>` (ブロックする) |
| `sockat::connect_any_nonblocking(path)` | `io::Result<(UnixStream, bool)>` (非ブロッキング) |

他の経路が全部使えない時、ディレクトリに `fchdir` した子を fork して、子に connect させる。`bool` は、fork の経路を通った時に `true`、通らなかった時は `false`。`true` の時は下の副作用が乗っているので、ログに残す・警告するなどに使える。

### fork の経路で connect した時の副作用

**サーバに見える peer の pid は、もう終了したプロセス (fork した子) の pid になる。** ソケット自体は呼び出し元が持っていて、普通に使える。

- サーバに見える uid / gid は呼び出し元と同じ
- macOS (`LOCAL_PEERPID`) は、呼び出し元が send すると呼び出し元の pid に変わる。Linux (`SO_PEERCRED`) は終了した子の pid のまま変わらない
- サーバがその pid からプロセスの情報 (実行ファイル、ppid を辿って親を見る等) を引こうとしても、プロセスはもう無いので引けない。番号が再利用されていれば、無関係なプロセスの情報を掴む
- 終了した pid の番号は、後で別のプロセスに再利用されうる
- なので、この pid は呼び出し元にもサーバにも使い道が無い。`connect_any` は pid の数値は返さず、fork の経路を通ったかだけを返す
- NetBSD・OpenBSD・illumos では確かめていない

## OS ごとの届き方

100 バイトを超えるパスが、OS ごとにどの経路まで `connect` で (fork せずに) 届き、どんな珍しい場合に fork (`connect_any` 系、`bind`) が要るか。

| OS | `connect` で届く経路 | fork が要る (`connect` では `ENAMETOOLONG` になる) 時 |
|---|---|---|
| Linux / Android (`/proc` あり) | `/proc/self/fd/<fd>/<名前>`。ディレクトリの深さによらない | 無い (fork しない)。ファイル名がおよそ 90 バイトを超えると、どの関数でも `ENAMETOOLONG` |
| FreeBSD | `bindat(2)` / `connectat(2)`。ディレクトリの深さによらない | 無い (fork しない) |
| macOS | 相対パス (100 バイトまで) → 広げた sockaddr (253 バイトまで) → symlink (253 バイトまで) | ファイル名が 230 バイトを超え、元のパスも相対パスも 253 バイトを超える時。安全で symlink を作れる置き場が無い時 |
| NetBSD / OpenBSD / illumos / その他の Unix、`/proc` の無い Linux / Android | 相対パス (100 バイトまで) → symlink (100 バイトまで) | ファイル名が 77 バイトを超え、相対パスも 100 バイトを超える時。安全で symlink を作れる置き場が無い時 |
| Windows | 対象外 (compile エラー) | |

symlink の経路は `/tmp/sockat.XXXXXXXXXX/<ファイル名>` (ファイル名の前が 23 バイト) で渡すので、`/tmp` が使えればディレクトリの深さによらず届く。fork が要るのは、ファイル名が長い時と、`TMPDIR` も `/tmp` も使えない時 (サンドボックス等) だけである。

## 長いパスへの届き方

どの経路も呼び出し元のプロセスの cwd を変えない。

### Linux / Android

ディレクトリを `O_PATH` で開き、`/proc/self/fd/<fd>/<ファイル名>` を std に渡す。ディレクトリ部分の長さに上限は無い。`/proc` がある時はこれ以外を試さない。

`/proc` が無い時 (`/proc` を mount していない chroot や最小のコンテナ) は、次の「その他の Unix」と同じ順に試す。`/proc` の経路で実際に失敗した時は、他の経路でやり直さずにそのエラーを返す。

### FreeBSD

`bindat(2)` / `connectat(2)` で、ディレクトリの fd を基準にファイル名で bind / connect する。

### macOS、NetBSD、OpenBSD、illumos、その他の Unix

次の順に試し、最初に使えたもので bind / connect する。

1. **cwd からの相対パス** (100 バイト以内なら)。パスが cwd (`getcwd`、または cwd と同じディレクトリを指す `$PWD`) の下にあれば前を削る。そうでなければ両方を実パスに揃えて `..` で上る綴りを作る
2. **広げた sockaddr** (macOS だけ、253 バイトまで)。XNU は `sun_path` が 104 バイトを超えるアドレスを 255 バイトまで受け付ける。元のパス、無理なら相対パス
3. **短い symlink 経由**。`<置き場>/sockat.XXXXXXXXXX` → ソケットのディレクトリ、の symlink を作り、それを通したパスで bind / connect して、すぐ消す。置き場は `TMPDIR` (絶対パスの時)、`/tmp` の順に試し、symlink を通したパスが収まり (macOS は 253、他は 100 バイトまで)、安全で、symlink を作れた最初の場所を使う (`TMPDIR` が深くて短くならない時は `/tmp` を使う)
   - **安全な置き場**は、他人が中の symlink を差し替えられないディレクトリ: sticky bit が立っていて持ち主が root か自分 (`/tmp` がこれ)、または持ち主が自分で group と other に書き込み権限が無い (macOS の `$TMPDIR` がこれ)。symlink は辿って判定する (macOS の `/tmp` は `/private/tmp` への symlink)。満たさない置き場には何も作らず、次の候補に進む
4. **fork**。ディレクトリの fd に `fchdir` した子が、ファイル名だけで bind / connect する。`bind` と `connect_any` 系だけが試し、`connect` / `connect_nonblocking` は試さずに `ENAMETOOLONG` を返す

ある方法が実際に失敗した時 (`ENOENT`、`EADDRINUSE` 等) は、そのエラーを返して次を試さない。次へ進むのは、その方法自体が使えない時 (長さが足りない、置き場が安全でない、symlink が作れない等) だけ。symlink の置き場も同じで、ある置き場を通した bind / connect が失敗したら、次の置き場では試さない。

### Windows

対象外。Unix 以外では compile エラーになる。

## 長いパスで std と違う点

### カーネルが返すアドレスは元のパスではない

`local_addr()` (listener と accept したストリーム) と `peer_addr()` (connect したストリーム) は、実際にカーネルに渡した綴りを返す。

| 経路 | 返るもの |
|---|---|
| Linux の `/proc` | `/proc/self/fd/<fd>/<名前>` (fd はもう閉じている) |
| 相対パス | 相対パス |
| 広げた sockaddr | 途中で切れたパス |
| symlink | もう無い symlink を通したパス |
| fork / bindat | ファイル名だけ |

OpenBSD では、100 バイト以内の短いパス (sockat は std にそのまま渡す) でも、std の `local_addr()` がパスの末尾 1 バイトを欠いて返す (CI で観測)。sockat ではなく std / OS の振る舞いで、std の `UnixListener::bind` を直接使っても同じになる。

元のパスは自分で持っておき、それを使う。

### fork した子は一瞬、全 fd の複製を持つ

fork の経路 (`bind` と `connect_any` 系) では、fork の性質として、子は終了するまでプロセスの全 fd の複製を持つ。その瞬間に他のスレッドが閉じたソケットやパイプは、子が終わるまで開いたままになる (相手が EOF を見るのが少し遅れる。閉じた listener がさらに接続を受けうる)。fork の経路は、macOS ではディレクトリを `O_SEARCH` で開くので、通常の bind / connect と同じ権限 (検索、bind なら書き込みも) で足りる。Linux の fork は `/proc` の経路と同じく `O_PATH` で開いたディレクトリに入るので、検索権限で足りる。他の Unix の fork と FreeBSD の bindat は `O_RDONLY` で開くので、ディレクトリの読み取り権限も要る。

### ディレクトリは呼ぶ少し前に引かれる

`/proc`・相対パス・bindat・fork の経路は、ディレクトリを先に引いて (開く、または実パスにする) から、ファイル名で bind / connect する。その間に途中のディレクトリが rename されたり symlink が張り替えられたりすると、引いた時点のディレクトリにソケットができる。

### 相対パスの経路は cwd に依存する

相対パスを作る前に、それが同じディレクトリを指すことを確かめ、bind / connect の後に cwd が変わっていないことも確かめる。他のスレッドが間に cwd を変えていたら、結果を捨ててエラーを返す (できたかもしれないソケットファイルは、場所が分からないので消さない)。変えて戻した場合は見えない。bind / connect の間に cwd を変えるプログラムはそもそもプロセス中の全ての相対パスと競合していて、sockat はそれを安全にはせず、見える場合を検出するだけである。

### symlink が残ることがある

symlink を作ってから消すまでの間にプロセスが死ぬと、`TMPDIR` か `/tmp` の下に `sockat.XXXXXXXXXX` (ソケットのディレクトリを指す) が残る。名前が `sockat.` で始まるので、sockat の残骸と分かる。`TMPDIR` が相対パスの時は `TMPDIR` には作らない (`/tmp` は使う)。

## エラー

| 状況 | エラー |
|---|---|
| ディレクトリが無い | `ENOENT` |
| connect 先が無い | `ENOENT` |
| bind 先に既にファイルがある | `EADDRINUSE` |
| 誰も listen していないソケットファイル | `ECONNREFUSED` |
| backlog が満杯 (ブロックしない connect、Linux) | `EAGAIN` |
| `connect` / `connect_nonblocking` で、fork の経路でしか届かない | `ENAMETOOLONG` |
| ファイル名だけでどの経路にも収まらない (Linux でおよそ 90 (`/proc` が無い時は 107)、macOS で 253、他の Unix で 103 バイト超) | `ENAMETOOLONG` |
| 名前で終わらないパス (`dir/`、`dir/.`、`..`)、NUL を含むパス | `InvalidInput` |
| 相対パスの経路の途中で cwd が変わった | `Other` |

エラーは `std::io::Error` で、OS のエラーは `raw_os_error()` で errno を取れる。

## async ランタイム

戻り値は std の型なので、std のソケットを受け取れるランタイムならどれでも使える。tokio の例:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` は待たないので、runtime の worker から直接呼べる。`bind` は待つことが無い (bind と listen はすぐ返る)。
