# sockat マニュアル

> [English](./MANUAL.md) | 日本語

sockat の使い方と、長いパスにどう届くか、長いパスで std と違う点のリファレンス。設計の判断と理由は [DR-0001](./decisions/DR-0001-long-socket-path-routes.md) にある。

言語によらない振る舞いの正本は [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) で、sockat-rs はその Rust 実装。

## 使い方

```toml
[dependencies]
sockat = "0.2"
```

std の `bind` / `connect` の代わりに `sockat::bind` / `sockat::connect` を使う。非同期のコードでは `sockat::connect_nonblocking` を使う。

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

- std が受け付ける長さ ([OS ごとの値](#長いパスへの届き方)) のパスは std の関数をそのまま呼ぶ (`connect_nonblocking` は std に無いので、std と同じアドレスで自前のソケットを connect する)
- ソケットファイルは渡したパスにできる。listener を閉じても消えない (std と同じ)。消す時は元のパスで `std::fs::remove_file(path)` する (unlink には `sun_path` の上限が無い)
- 自前で作るソケットは std と同じ設定 (close-on-exec、std が付ける OS (macOS / iOS、FreeBSD、NetBSD、DragonFly) では `SO_NOSIGPIPE`、std と同じ listen の backlog)

## connect と connect_nonblocking

- **`connect`** は std の `UnixStream::connect` の代わりに使う。ブロックする
- **`connect_nonblocking`** は async ランタイムで使う。ソケットを connect の前に非ブロッキングにし、そのまま返す。tokio の `UnixStream::from_std` にそのまま渡せる。相手の backlog が満杯なら待たずに失敗する (Linux は `EAGAIN`、macOS は `ECONNREFUSED`)。Unix domain の connect は呼び出しの中で終わるか失敗するので、後で待つものは無い (Linux と macOS で確かめた)

どちらも fork の経路だけは試さない ([長いパスへの届き方](#長いパスへの届き方))。fork でしか届かない時は、パスが `sun_path` に入らない普通の connect と同じく `ENAMETOOLONG` を返す。普通の環境 (`/tmp` が書ける) では、ディレクトリの深さにもファイル名の長さにもよらず届く ([どこから届かなくなるか](#どこから届かなくなるか))。

## 長いパスへの届き方

std が受け付ける長さ (`sun_path` − 1) のパスは std にそのまま渡す。

| OS | std にそのまま渡す長さ |
|---|---|
| Linux / Android / illumos | 107 バイトまで |
| macOS / iOS / FreeBSD / NetBSD / OpenBSD | 103 バイトまで |

それを超えるパスは、OS ごとに次の順に試し、最初に使えた経路で bind / connect する。どの経路も呼び出し元のプロセスの cwd を変えないので、マルチスレッドのプログラムから使える。

### OS ごとに試す順番

| OS | 試す順番 |
|---|---|
| Linux / Android (`/proc` を使える) | [/proc](#proc) |
| Linux / Android (`/proc` を使えない) | [相対パス](#相対パス) → [symlink](#symlink) → [fork](#fork) |
| FreeBSD | [bindat](#bindat) |
| macOS | [相対パス](#相対パス) → [広げた sockaddr](#広げた-sockaddr) → [symlink](#symlink) → [fork](#fork) |
| NetBSD / OpenBSD / illumos / その他の Unix | [相対パス](#相対パス) → [symlink](#symlink) → [fork](#fork) |
| Windows | 対象外 (Unix 以外では compile エラー) |

fork は `bind` と `connect_any` 系だけが試す。`connect` / `connect_nonblocking` は fork の手前で `ENAMETOOLONG` を返す。

### 各経路

#### /proc

Linux / Android。ディレクトリを `O_PATH` で開き、`/proc/self/fd/<fd>/<ファイル名>` を std に渡す。カーネルがこれを開いたディレクトリとして解決するので、ソケットはそのディレクトリにできる。

- ディレクトリ部分の長さに上限は無い。`/proc/self/fd/<fd>/<ファイル名>` 全体は 107 バイトまでなので、ファイル名はおよそ 90 バイトまで (fd 番号の桁数で変わる)
- `O_PATH` で開くので、ディレクトリの読み取り権限は要らない
- 使えない時は他の経路を順に試す。使える時はこの経路だけを使う。使えないのは次の時:
  - `/proc/self/fd/<fd>` を引けない (`/proc` を mount していない chroot や最小のコンテナ)
  - ファイル名が上の長さに収まらない

#### bindat

FreeBSD。`bindat(2)` / `connectat(2)` で、ディレクトリの fd を基準にファイル名で bind / connect する。

- ディレクトリ部分の長さに上限は無い。ファイル名は 103 バイトまで。超えると `ENAMETOOLONG`
- ディレクトリを `O_RDONLY` で開くので、ディレクトリの読み取り権限も要る

#### 相対パス

cwd からの相対パスが std にそのまま渡せる長さなら、それを std に渡す。元のパスが絶対パスの時だけ使う。パスを文字列で書き直すだけで、cwd は変えない (`chdir` しない)。

- パスが cwd (`getcwd`、または cwd と同じディレクトリを指す `$PWD`) の下にあれば、前を削る。それで std にそのまま渡せる長さにならない時 (cwd の下に無い時も含む) は、両方を実パスに揃えて `..` で上る綴りも作る。短い方を使う
- 相対パスが元のパスと同じディレクトリを指すことを確かめてから使い、bind / connect の後に cwd が変わっていないことも確かめる ([後述](#相対パスの経路は他のスレッドの-chdir-に弱い))

#### 広げた sockaddr

macOS。XNU は `sun_path` が 104 バイトを超えるアドレスも、パスが 253 バイトまでなら受け付ける。元のパスが 253 バイト以内ならそれを、無理なら相対パスが 253 バイト以内ならそれを渡す。

#### symlink

一時ディレクトリに `sockat.XXXXXXXXXX` (乱数 10 文字) の symlink を作り、それを通して bind / connect して、すぐ消す。

| | symlink の先 | 渡す綴り | `/tmp` で届く範囲 |
|---|---|---|---|
| bind | ソケットのディレクトリ | `<置き場>/sockat.XXXXXXXXXX/<ファイル名>` | ファイル名が 84 バイト (`sun_path` が 104 の OS は 80、macOS は 230) 以内 |
| connect | ソケットのファイル | `<置き場>/sockat.XXXXXXXXXX` | ディレクトリの深さにもファイル名の長さにもよらない |

- symlink の先は渡したパスそのものなので、元のパスと同じ場所に解決される
- 置き場の候補は、`TMPDIR` (空でない絶対パスの時)、`/tmp` の順。渡す綴りが std にそのまま渡せる長さ (macOS は広げた sockaddr で 253 バイト) に収まり、安全で、symlink を作れた最初の候補を使う。`TMPDIR` が深くて収まらない時は `/tmp` を使う
- **安全な置き場**は、他人が中の symlink を差し替えられないディレクトリで、次のどちらか。満たさない候補には何も作らず、次の候補に進む
  - sticky bit が立っていて、持ち主が root か自分 (`/tmp` がこれ)
  - 持ち主が自分で、group と other に書き込み権限が無い (macOS の `$TMPDIR` がこれ)
- 置き場は symlink を辿って判定する (macOS の `/tmp` は `/private/tmp` への symlink)
- symlink を作れない時 (置き場が無い・書けない、サンドボックス、空いている名前が見つからない、乱数が取れない) も次の候補に進む

#### fork

ディレクトリを開いた fd に `fchdir` した子を fork し、子がファイル名だけで bind / connect して、すぐ終了する。fork の後もソケットは親と子で同じものを指すので、子の bind / connect は呼び出し元のソケットにそのまま効く。bind の後の listen は呼び出し元がする。

- **`bind` と `connect_any` 系だけが試す。** `connect` / `connect_nonblocking` は試さずに `ENAMETOOLONG` を返す (ディレクトリを開いてから判定するので、ディレクトリが無ければ `ENOENT`)
- ファイル名は `sun_path` に入る長さまで: macOS は広げた sockaddr で 253 バイト、Linux と illumos は 107 バイト、NetBSD / OpenBSD 等の BSD は 103 バイト。超えると `ENAMETOOLONG`
- ディレクトリの開き方と要る権限: macOS は `O_SEARCH` で開くので、通常の bind / connect と同じ権限 (検索、bind なら書き込みも) で足りる。Linux は /proc の経路のために `O_PATH` で開いた fd を使うので、読み取り権限は要らない。他の Unix は `O_RDONLY` で開くので、読み取り権限も要る
- bind は子が bind するだけで listen は呼び出し元なので、fork の経路でも相手から見える副作用が無い。fork しない版の bind は無い
- 子の区間は async-signal-safe な呼び出し (`fchdir`、`bind` / `connect`、`_exit`) だけなので、マルチスレッドのプロセスから呼べる
- 副作用がある: [connect した時の peer の pid](#fork-の経路で-connect-した時の副作用)、[子が一瞬持つ fd の複製](#fork-した子は一瞬全-fd-の複製を持つ)

### 経路の失敗の扱い

経路が実際に bind / connect して失敗した時 (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等) は、そのエラーを返して次の経路を試さない。次へ進むのは、その経路自体が使えない時 (長さが足りない、`/proc` を使えない、カーネルが広げた sockaddr を受け付けない、置き場が安全でない、symlink が作れない) だけ。symlink の置き場も同じで、ある置き場を通した bind / connect が失敗したら、次の置き場では試さない。

### どこから届かなくなるか

std にそのまま渡せない長さのパスが `ENAMETOOLONG` になる時。

| OS | `bind` | `connect` / `connect_nonblocking` | `connect_any` 系 |
|---|---|---|---|
| Linux / Android (`/proc` あり) | ファイル名が 107 バイトを超える | ファイル名が `/proc` の綴りに収まらず、相対パスが 107 バイトを超え、symlink の経路も使えない | ファイル名が 107 バイトを超え、symlink の経路も使えない |
| Linux / Android (`/proc` 無し) | ファイル名が 107 バイトを超える | 相対パスが 107 バイトを超え、symlink の経路も使えない | ファイル名が 107 バイトを超え、symlink の経路も使えない |
| FreeBSD | ファイル名が 103 バイトを超える | ファイル名が 103 バイトを超える | ファイル名が 103 バイトを超える |
| macOS | ファイル名が 253 バイトを超える | 元のパスも相対パスも 253 バイトを超え、symlink の経路も使えない | ファイル名が 253 バイトを超え、symlink の経路も使えない |
| NetBSD / OpenBSD / その他の BSD | ファイル名が 103 バイトを超える | 相対パスが 103 バイトを超え、symlink の経路も使えない | ファイル名が 103 バイトを超え、symlink の経路も使えない |
| illumos | ファイル名が 107 バイトを超える | 相対パスが 107 バイトを超え、symlink の経路も使えない | ファイル名が 107 バイトを超え、symlink の経路も使えない |

- connect で symlink の経路が使えないのは、安全で symlink を作れる置き場が無い時 (`TMPDIR` も `/tmp` も使えないサンドボックス等)。`/tmp` が使えれば長さは足りる

## 例外: connect_any

`connect_any` / `connect_any_nonblocking` は、`connect` が上の表で `ENAMETOOLONG` になるよほど珍しい環境で、fork の副作用を受け入れてでもつなぎたい時だけのもの。普通は使わない。

| 関数 | 戻り値 |
|---|---|
| `sockat::connect_any(path)` | `io::Result<(UnixStream, bool)>` (ブロックする) |
| `sockat::connect_any_nonblocking(path)` | `io::Result<(UnixStream, bool)>` (非ブロッキング) |

試す順番は `connect` と同じで、他の経路が全部使えない時に最後に [fork](#fork) の経路で connect する。`bool` は、fork の経路を通った時に `true`、通らなかった時は `false`。`true` の時は下の副作用が乗っているので、ログに残す・警告するなどに使える。FreeBSD では fork しないので、常に `false` になる。

`connect_any_nonblocking` は fork の前にソケットを非ブロッキングにするので、子の connect も待たない。

### fork の経路で connect した時の副作用

**サーバに見える peer の pid は、もう終了したプロセス (fork した子) の pid になる。** ソケット自体は呼び出し元が持っていて、普通に使える。

- サーバに見える uid / gid は呼び出し元と同じ
- macOS (`LOCAL_PEERPID`) は、呼び出し元が send すると呼び出し元の pid に変わる。Linux (`SO_PEERCRED`) は終了した子の pid のまま変わらない
- サーバがその pid からプロセスの情報 (実行ファイル、ppid を辿って親を見る等) を引こうとしても、プロセスはもう無いので引けない
- 終了した pid の番号は、後で別のプロセスに再利用されうる。再利用されていれば、サーバは無関係なプロセスの情報を掴む
- なので、この pid は呼び出し元にもサーバにも使い道が無い。`connect_any` は pid の数値は返さず、fork の経路を通ったかだけを返す
- NetBSD・OpenBSD・illumos での peer の資格情報の振る舞いは確かめていない

## 長いパスで std と違う点

### カーネルが返すアドレスは元のパスではない

`local_addr()` (listener と accept したストリーム) と `peer_addr()` (connect したストリーム) は、listener を bind した時にカーネルに渡した綴りを返す。connect した側がどの経路を通ったかは関係ない。

| bind の経路 | 返るもの |
|---|---|
| /proc | `/proc/self/fd/<fd>/<ファイル名>` (fd はもう閉じている) |
| 相対パス | 相対パス |
| 広げた sockaddr | 途中で切れたパス |
| symlink | もう無い symlink を通したパス |
| fork、bindat | ファイル名だけ |

元のパスは自分で持っておき、それを使う。

OpenBSD では、103 バイト以内の短いパス (sockat は std にそのまま渡す) でも、std の `local_addr()` がパスの末尾 1 バイトを欠いて返す (CI で観測)。sockat ではなく std / OS の振る舞いで、std の `UnixListener::bind` を直接使っても同じになる。

### 名前で終わらないパスは InvalidInput

std にそのまま渡せない長さのパスで、最後がファイル名でないもの (`dir/`、`dir/.`、`dir/..`、`..`) は、何も作らずに `InvalidInput` を返す。ディレクトリとファイル名に分けられないからである。それより短いパスは std にそのまま渡すので、カーネルのエラーになる (例えば `dir/` の bind は macOS でも Linux でも `EADDRINUSE`)。

### fork した子は一瞬、全 fd の複製を持つ

fork の経路 (`bind` と `connect_any` 系) では、fork の性質として、子は終了するまでプロセスの全 fd の複製を持つ。その瞬間に他のスレッドが閉じたソケットやパイプは、子が終わるまで開いたままになる (相手が EOF を見るのが少し遅れる。閉じた listener の backlog にさらに接続が入りうる)。

### ディレクトリは呼ぶ少し前に引かれる

/proc・相対パス・bindat・fork の経路は、ディレクトリを先に引いて (開く、または実パスにする) から、ファイル名で bind / connect する。その間に途中のディレクトリが rename されたり symlink が張り替えられたりすると、引いた時点のディレクトリにソケットができる (connect ならそこを見る)。広げた sockaddr と symlink の経路は元のパスをそのままカーネルに解決させるので、この点は std と同じに振る舞う。

### 相対パスの経路は、他のスレッドの chdir に弱い

sockat 自身は cwd を変えない。相対パスを作る時に、それが同じディレクトリを指すことを確かめ、bind / connect の後に cwd が変わっていないことも確かめる。他のスレッドが間に cwd を変えていたら、結果を捨ててエラー (`Other`) を返す (できたかもしれないソケットファイルは、場所が分からないので消さない)。変えて戻した場合は見えない。bind / connect の間に cwd を変えるプログラムはそもそもプロセス中の全ての相対パスと競合していて、sockat はそれを安全にはせず、見える場合を検出するだけである。

### symlink が残ることがある

symlink を作ってから消すまでの間にプロセスが死ぬと、`TMPDIR` か `/tmp` の下に `sockat.XXXXXXXXXX` (ソケットのディレクトリかファイルを指す) が残る。名前が `sockat.` で始まるので、sockat の残骸と分かる。

## エラー

| 状況 | エラー |
|---|---|
| ディレクトリが無い | `ENOENT` |
| connect 先が無い | `ENOENT` |
| bind 先に既にファイルがある | `EADDRINUSE` |
| 誰も listen していないソケットファイル | `ECONNREFUSED` |
| backlog が満杯 | Linux はブロックしない connect で `EAGAIN` (ブロックする connect は空くまで待つ)、macOS は `ECONNREFUSED` |
| どの経路にも収まらない ([どこから届かなくなるか](#どこから届かなくなるか))。`connect` / `connect_nonblocking` では、fork の経路でしか届かない時も | `ENAMETOOLONG` |
| std にそのまま渡せない長さのパスで、名前で終わらない (`dir/`、`dir/.`、`..` 等) | `InvalidInput` |
| NUL を含むパス | `InvalidInput` |
| 相対パスの経路の途中で cwd が変わった | `Other` |
| fork した子が正常に終了しなかった。または子の終了状態を他で刈り取られ (`SIGCHLD` を `SIG_IGN` にしている、他のスレッドの `waitpid(-1)`)、ソケットが bind / connect されていなかった | `Other` |

エラーは `std::io::Error` で、OS のエラーは `raw_os_error()` で errno を取れる。

macOS では、相手の backlog が満杯の時も connect は (ブロックするかによらず) `ECONNREFUSED` を返すので、`ECONNREFUSED` だけでは「誰も listen していない残骸のソケットファイル」と判定できない。`ECONNREFUSED` を見てソケットファイルを消すような処理は、macOS では生きている listener のソケットファイルを消しうる。これはカーネルの振る舞いで、std の connect でも同じ (Linux は満杯なら、ブロックしない connect は `EAGAIN`、ブロックする connect は空くまで待つ)。

## async ランタイム

戻り値は std の型なので、std のソケットを受け取れるランタイムならどれでも使える。tokio の例:

```rust
let listener = sockat::bind(&path)?;
listener.set_nonblocking(true)?;
let listener = tokio::net::UnixListener::from_std(listener)?;

let stream = sockat::connect_nonblocking(&path)?;
let stream = tokio::net::UnixStream::from_std(stream)?;
```

`connect_nonblocking` は相手を待たないので、runtime の worker から直接呼べる。`bind` も相手を待たない (bind と listen はすぐ返る。fork の経路では子の終了を待つが、子は bind してすぐ終わる)。

## 確かめていないこと

- Android は compile もしていない。Linux と同じ経路を使う
- NetBSD・OpenBSD・illumos は CI の VM で試験を回しているが、fork の経路で connect した時の peer の資格情報 (pid) の振る舞いは確かめていない
- ブロックしない connect が backlog の満杯で返すエラーと、connect が呼び出しの中で終わることは、Linux と macOS でだけ確かめた
- macOS 以外の Apple の OS (iOS 等) は macOS と同じ経路を使うが、確かめたのは iOS の clippy だけ
