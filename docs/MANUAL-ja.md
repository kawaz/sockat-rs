# sockat マニュアル

> [English](./MANUAL.md) | 日本語

sockat の使い方と、長いパスにどう届くか、長いパスで std と違う点のリファレンス。設計の判断と理由は [DR の一覧](./decisions/INDEX.md) にある (経路と試す順番の総論は [DR-0001](./decisions/DR-0001-long-socket-path-routes.md))。

言語によらない振る舞いの正本は [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) で、sockat-rs はその Rust 実装。

## 使い方

```toml
[dependencies]
sockat = "0.4"
```

std の `bind` / `connect` の代わりに `sockat::bind` / `sockat::connect` を使う。非同期のコードでは `sockat::connect_nonblocking` を使う。

| 関数 | 対応する std | 戻り値 |
|---|---|---|
| `sockat::bind(path)` | `UnixListener::bind(path)` | `io::Result<UnixListener>` (listen 済み) |
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |
| `sockat::connect_nonblocking(path)` | (無し) | `io::Result<UnixStream>` (非ブロッキング) |

この他に、よほど珍しい環境のための `connect_any` / `connect_any_nonblocking` が、feature `any` の後ろにある ([後述](#例外-connect_any))。

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
- NUL を含むパスは、長さによらず何も作らずに `InvalidInput` を返す ([NUL を含むパス](#nul-を含むパス))
- ソケットファイルは渡したパスにできる。listener を閉じても消えない (std と同じ)。消す時は元のパスで `std::fs::remove_file(path)` する (unlink には `sun_path` の上限が無い)
- 自前で作るソケットは std と同じ設定 (close-on-exec、std が付ける OS (macOS / iOS、FreeBSD、NetBSD、DragonFly) では `SO_NOSIGPIPE`、std と同じ listen の backlog)

## connect と connect_nonblocking

- **`connect`** は std の `UnixStream::connect` の代わりに使う。ブロックする
- **`connect_nonblocking`** は async ランタイムで使う。ソケットを connect の前に非ブロッキングにし、そのまま返す。tokio の `UnixStream::from_std` にそのまま渡せる。相手の backlog が満杯なら待たずに失敗する (Linux は `EAGAIN`、macOS は `ECONNREFUSED`)。Unix domain の connect は呼び出しの中で終わるか失敗するので、後で待つものは無い (Linux と macOS で確かめた)

どちらも fork の経路だけは試さない ([長いパスへの届き方](#長いパスへの届き方))。fork でしか届かない時は、パスが `sun_path` に入らない普通の connect と同じく `ENAMETOOLONG` を返す。普通の環境 (`/tmp` が書ける) では、ディレクトリの深さにもファイル名の長さにもよらず届く。Linux と Apple の OS では、`/tmp` が使えなくても [スレッド cwd](#スレッド-cwd) の経路で `connect_any` と同じ所まで届く ([どこから届かなくなるか](#どこから届かなくなるか))。

## 長いパスへの届き方

std が受け付ける長さ (標準に渡す長さ) までのパスは std にそのまま渡す。今の std ではどの OS でも、広げていない sockaddr に入る長さ (`sun_path` − 1、以下「sockaddr に入る長さ」) と同じ。

| OS | std にそのまま渡す長さ |
|---|---|
| Linux / Android / illumos | 107 バイトまで |
| macOS / iOS 等 Apple の OS / FreeBSD / NetBSD / OpenBSD | 103 バイトまで |

それを超えるパスは、OS ごとに次の順に試し、最初に使えた経路で bind / connect する。どの経路も呼び出し元のプロセスの cwd を変えないので、マルチスレッドのプログラムから使える。

### OS ごとに試す順番

| OS | 試す順番 |
|---|---|
| Linux / Android (`/proc` を使える) | [/proc](#proc) → [相対パス](#相対パス) → [symlink](#symlink) → [スレッド cwd](#スレッド-cwd) → [fork](#fork) |
| Linux / Android (`/proc` を使えない) | [相対パス](#相対パス) → [symlink](#symlink) → [スレッド cwd](#スレッド-cwd) → [fork](#fork) |
| FreeBSD | [bindat](#bindat) |
| macOS / iOS 等 Apple の OS の `bind` | [相対パス](#相対パス) → [symlink](#symlink) → [スレッド cwd](#スレッド-cwd) → [fork](#fork) |
| macOS / iOS 等 Apple の OS の `connect` 系 | [相対パス](#相対パス) → [広げた sockaddr](#広げた-sockaddr) → [symlink](#symlink) → [スレッド cwd](#スレッド-cwd) → [fork](#fork) |
| NetBSD / OpenBSD / illumos / その他の Unix | [相対パス](#相対パス) → [symlink](#symlink) → [fork](#fork) |
| Windows | 対象外 (Unix 以外では compile エラー) |

`/proc` を使える Linux / Android では、/proc の経路の綴りが収まる時は /proc の経路だけを使う。収まらない時 (ファイル名がおよそ 90 バイトを超える) に相対パスから先へ進み、symlink の経路では安全でない置き場も [fd で掴む形](#安全でない置き場-fd-で掴む形) で使う (connect 系だけ)。

スレッド cwd はどの関数も試す。fork は `bind` と `connect_any` 系だけが試す。`connect` / `connect_nonblocking` は fork の手前で `ENAMETOOLONG` を返す。

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

cwd からの相対パスが sockaddr に入る長さ以内なら、それを std に渡す。元のパスが絶対パスの時だけ使う。パスを文字列で書き直すだけで、cwd は変えない (`chdir` しない)。

- パスが cwd (`getcwd`、または cwd と同じディレクトリを指す `$PWD`) の下にあれば、前を削る。それで sockaddr に入る長さにならない時 (cwd の下に無い時も含む) は、両方を実パスに揃えて `..` で上る綴りも作る。短い方を使う
- 相対パスのディレクトリが元のディレクトリと `(st_dev, st_ino)` で一致しない時と、cwd が既に変わっている時は、エラーにせず、この経路は使えないとして次の経路に進む
- 相対パスが元のパスと同じディレクトリを指すことを確かめてから使い、bind / connect の後に cwd が変わっていないことも確かめる ([後述](#相対パスの経路は他のスレッドの-chdir-に弱い))

#### 広げた sockaddr

macOS / iOS 等 Apple の OS の connect 系だけ。XNU は `sun_path` が 104 バイトを超えるアドレスも、パスが 253 バイトまでなら受け付ける。元のパスが 253 バイト以内ならそれを、無理なら相対パスが 253 バイト以内ならそれを渡す。

`bind` は使わない。広げた形で bind したソケットのアドレスは `sockaddr_un` に入らず、`getsockname` が途中で切れたパスを返して、読む側 (CPython の asyncio の後始末、Go の `net.FileConn` 等) を壊すからである。bind の綴りは全部、sockaddr に入る長さ (103 バイト) で判定する ([DR-0024](./decisions/DR-0024-apple-bind-without-widened-sockaddr.md))。

#### symlink

一時ディレクトリに `sockat.XXXXXXXXXX` (乱数 10 文字) の symlink を作り、それを通して bind / connect して、すぐ消す。

| | symlink の先 | 渡す綴り | `/tmp` で届く範囲 |
|---|---|---|---|
| bind | ソケットのディレクトリ | `<置き場>/sockat.XXXXXXXXXX/<ファイル名>` | ファイル名が 84 バイト (`sun_path` が 104 の OS は 80) 以内 |
| connect | ソケットのファイル | `<置き場>/sockat.XXXXXXXXXX` | ディレクトリの深さにもファイル名の長さにもよらない |

- symlink の先は渡したパスそのものなので、元のパスと同じ場所に解決される
- 置き場の候補は、`TMPDIR` (空でない絶対パスの時)、`/tmp` の順。渡す綴りが sockaddr に入る長さ (カーネルが広げた sockaddr を受け付ける Apple の OS の connect 系では 253 バイト) に収まり、安全で、symlink を作れた最初の候補を使う。`TMPDIR` が深くて収まらない時は `/tmp` を使う
- **安全な置き場**は、他人が中の symlink を差し替えられないディレクトリで (ディレクトリでなければ安全でない)、次のどちらか。満たさない候補には何も作らず、次の候補に進む (`/proc` を使える Linux / Android の connect 系は、下の fd で掴む形で使う)
  - sticky bit が立っていて、持ち主が root か自分 (`/tmp` がこれ)
  - 持ち主が自分で、group と other に書き込み権限が無い (macOS の `$TMPDIR` がこれ)
- 置き場は symlink を辿って判定する (macOS の `/tmp` は `/private/tmp` への symlink)
- symlink を作れない時 (置き場が無い・書けない、サンドボックス、空いている名前が見つからない、乱数が取れない) も次の候補に進む

##### 安全でない置き場 (fd で掴む形)

`/proc` を使える Linux / Android の connect 系 (`connect`、`connect_nonblocking`、`connect_any` 系) だけ。安全でない置き場の候補 (sticky でない 0777 の `/tmp` 等) を飛ばさず、自分専用のディレクトリを作って名前でなく fd で辿る。

1. 何かを作る前に `/proc` を使えるか確かめる。使えなければ何も作らずに次の候補へ
2. 置き場に mode 0700 のディレクトリ `sockat.XXXXXXXXXX` を作る (名前の付け方は symlink と同じ)
3. それを `O_PATH | O_DIRECTORY | O_NOFOLLOW` で開き (ディレクトリに権限が無くても開ける)、ディレクトリで、持ち主が自分で、group と other に権限が無いことを確かめる。違えば (作ってから開くまでに他人が差し替えた) 使わず、消さない。通ったら `/proc/self/fd/<fd>` を通して 0700 に直す (umask が持ち主のビットを削っていても使えるように。fd が掴んだ物に効くので差し替えの心配が無い)
4. 開いた fd の中に、ソケットのファイルを指す symlink `s` を作り、`/proc/self/fd/<fd>/s` で connect する
5. connect が成功しても失敗しても、自分が作った `s` を消し、ディレクトリはその名前がまだ開いた物を指す時だけ消す。確かめた結果が違ったディレクトリは、中身も含めて触らない

- 他人がディレクトリを rename して別の物を置いても、fd で辿るので connect には効かず、置かれた物も消さない
- 綴りはおよそ 20 バイトで、ファイル名の長さにも置き場の深さにもよらない
- 候補の順は変わらない: `TMPDIR` を (安全なら symlink、安全でなければこの形で) 試してから `/tmp` に進む
- bind は使わない (綴り `/proc/self/fd/<fd>/s/<ファイル名>` は /proc の経路の綴りより長い)
- 使えない時 (ディレクトリを作れない・開けない、確かめた結果が違う、symlink を作れない) は次の候補に進む

#### スレッド cwd

Linux / Android と Apple の OS。この呼び出しのためだけにスレッドを 1 本作り、そのスレッドだけ cwd をソケットのディレクトリに移して、ファイル名だけで bind / connect する。スレッドは呼び出しと一緒に終わる。

| OS | スレッドの cwd の移し方 |
|---|---|
| Linux / Android | `unshare(CLONE_FS)` で cwd をそのスレッドだけのものにしてから、ディレクトリの fd に `fchdir` |
| Apple の OS | `pthread_fchdir_np` (そのスレッドだけの cwd を設定する libSystem の関数) でディレクトリの fd に移す |

- **どの関数も試す。** 呼び出し元のプロセスが bind / connect するので、サーバに見える peer の pid は呼び出し元のもの。fd の複製も無く、他のスレッドとプロセスの cwd は変わらない。`connect_any` 系はこの経路では `false` を返す
- ソケットは呼び出し元のスレッドが作り (`connect_nonblocking` 等はここで非ブロッキングにする)、bind の後の listen も呼び出し元のスレッドがする
- ファイル名と、ディレクトリの開き方・要る権限は [fork](#fork) の経路と同じ
- 使えない時は次の経路 (`bind` と `connect_any` 系は fork、`connect` / `connect_nonblocking` は `ENAMETOOLONG`) に進む。使えないのは次の時:
  - Linux で seccomp の filter が掛かっている (docker の既定も掛かっている)。filter には `unshare` をエラーでなくプロセスの kill (SIGSYS) で断るもの (systemd の `SystemCallFilter=` の既定の動作等) があるので、filter が掛かっていれば `unshare` を呼ばない
  - Linux で `unshare` が断られた
  - Apple の OS で `pthread_fchdir_np` が無い・失敗した (公開ヘッダに無い関数なので、実行時に探す)
  - スレッドを作れない

#### fork

ディレクトリを開いた fd に `fchdir` した子を fork し、子がファイル名だけで bind / connect して、すぐ終了する。fork の後もソケットは親と子で同じものを指すので、子の bind / connect は呼び出し元のソケットにそのまま効く。bind の後の listen は必ず呼び出し元がする。

- **`bind` と `connect_any` 系だけが試す。** `connect` / `connect_nonblocking` は試さずに `ENAMETOOLONG` を返す (ディレクトリを開いてから判定するので、ディレクトリが無ければ `ENOENT`)
- ファイル名は sockaddr に入る長さまで: Linux と illumos は 107 バイト、Apple の OS と NetBSD / OpenBSD 等の BSD は 103 バイト (Apple の OS の connect 系は、カーネルが広げた sockaddr を受け付ける時 253 バイト)。超えると `ENAMETOOLONG`
- ディレクトリの開き方と要る権限: macOS は `O_SEARCH` で開くので、通常の bind / connect と同じ権限 (検索、bind なら書き込みも) で足りる。Linux は /proc の経路のために `O_PATH` で開いた fd を使うので、読み取り権限は要らない。他の Unix は `O_RDONLY` で開くので、読み取り権限も要る
- bind は子が bind するだけで listen は呼び出し元なので、fork の経路でも相手から見える副作用が無い。クライアントに見えるサーバの peer (macOS の `LOCAL_PEERPID`、Linux の `SO_PEERCRED`) は、send の前から呼び出し元 ([DR-0025](./decisions/DR-0025-fork-bind-listen-by-parent.md))。fork しない版の bind は無い
- 子の区間は async-signal-safe な呼び出し (`fchdir`、`bind` / `connect`、`_exit`) だけなので、マルチスレッドのプロセスから呼べる
- 副作用がある: [connect した時の peer の pid](#fork-の経路で-connect-した時の副作用) (相手から見える。connect だけ)、[相手から見えない副作用](#fork-の経路の相手から見えない副作用) (bind でも connect でも)

### 経路の失敗の扱い

経路が実際に bind / connect して失敗した時 (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等) は、そのエラーを返して次の経路を試さない。次へ進むのは、その経路自体が使えない時 (長さが足りない、`/proc` を使えない、相対パスのディレクトリが元と一致しない・cwd が既に変わっている、カーネルが広げた sockaddr を受け付けない、置き場が安全でない (fd で掴む形の無い時)、symlink が作れない、fd で掴む形でディレクトリを作れない・確かめた結果が違う、スレッド cwd の経路が使えない) だけ。symlink の置き場も同じで、ある置き場を通した bind / connect が失敗したら、次の置き場では試さない。

### どこから届かなくなるか

std にそのまま渡せない長さのパスが `ENAMETOOLONG` になる時 (ディレクトリがある前提。ディレクトリが無ければ `ENOENT`)。

| OS | `bind` | `connect` / `connect_nonblocking` | `connect_any` 系 |
|---|---|---|---|
| Linux / Android (`/proc` あり) | ファイル名が 107 バイトを超える | ファイル名が 107 バイトを超え、symlink の経路も使えない (置き場の安全は問わない)<br>スレッド cwd の経路が使えない時: ファイル名が `/proc` の綴りに収まらず、相対パスが 107 バイトを超え、symlink の経路も使えない (置き場の安全は問わない) | ファイル名が 107 バイトを超え、symlink の経路も使えない (置き場の安全は問わない) |
| Linux / Android (`/proc` 無し) | ファイル名が 107 バイトを超える | ファイル名が 107 バイトを超え、symlink の経路も使えない<br>スレッド cwd の経路が使えない時: 相対パスが 107 バイトを超え、symlink の経路も使えない | ファイル名が 107 バイトを超え、symlink の経路も使えない |
| FreeBSD | ファイル名が 103 バイトを超える | ファイル名が 103 バイトを超える | ファイル名が 103 バイトを超える |
| macOS / iOS 等 Apple の OS | ファイル名が 103 バイトを超える | ファイル名が 253 バイトを超え、symlink の経路も使えない<br>スレッド cwd の経路が使えない時: 元のパスも相対パスも 253 バイトを超え、symlink の経路も使えない | ファイル名が 253 バイトを超え、symlink の経路も使えない |
| NetBSD / OpenBSD / その他の BSD | ファイル名が 103 バイトを超える | 相対パスが 103 バイトを超え、symlink の経路も使えない | ファイル名が 103 バイトを超え、symlink の経路も使えない |
| illumos | ファイル名が 107 バイトを超える | 相対パスが 107 バイトを超え、symlink の経路も使えない | ファイル名が 107 バイトを超え、symlink の経路も使えない |

- Apple の OS の connect 系の 253 バイトは、カーネルが広げた sockaddr を受け付ける時。断った時は 103 バイト。bind は広げた sockaddr を使わないので、いつも 103 バイト
- スレッド cwd の経路を使える Linux / Apple の OS では、`connect` 系も `connect_any` 系と同じ所まで届く
- connect で symlink の経路が使えないのは、安全で symlink を作れる置き場が無い時 (`TMPDIR` も `/tmp` も使えないサンドボックス等)。`/tmp` が使えれば長さは足りる
- `/proc` を使える Linux では、置き場が安全でなくても、そこにディレクトリを作れれば connect は同じく届く ([fd で掴む形](#安全でない置き場-fd-で掴む形))
- PATH_MAX (macOS 1024、Linux 4096) を超えるパスは `ENAMETOOLONG`。ディレクトリを開く等でフルパスを渡すからで、sockat の責務外

### NUL を含むパス

NUL を含むパスは、長さによらず、何も作らずに `InvalidInput` を返す。カーネルはパスを最初の NUL までしか読まないので、通すと別の場所にソケットができうるからである。std も NUL を含むパスを `InvalidInput` で断るので、短いパスでは std と同じ種類のエラーになる (文言は sockat のもの)。

Linux / Android で先頭が NUL のパスは abstract namespace の名前でファイルではないので、sockat の対象外で、長さによらず std にそのまま渡す。std は `Path` の中の NUL をどれも断るので、結果は std と同じ `InvalidInput` になる。abstract namespace のソケットは、std の `SocketAddr::from_abstract_name` と `UnixListener::bind_addr` / `UnixStream::connect_addr` で扱う。

## 例外: connect_any

`connect_any` / `connect_any_nonblocking` は、`connect` が上の表で `ENAMETOOLONG` になるよほど珍しい環境で、fork の副作用を受け入れてでもつなぎたい時だけのもの。普通は使わない。

要るのは、安全で symlink を作れる置き場が無く、さらに次のどれかの時だけ:

- NetBSD / OpenBSD / illumos 等、スレッド cwd の経路の無い OS
- seccomp の filter が掛かった Linux (docker の既定、systemd の `SystemCallFilter=` 等) で、`/proc` が無いか、置き場にディレクトリも作れない時 (`/proc` を使えれば安全でない置き場も [fd で掴む形](#安全でない置き場-fd-で掴む形) で使う)
- `pthread_fchdir_np` の無い Apple の OS

| 関数 | 戻り値 |
|---|---|
| `sockat::connect_any(path)` | `io::Result<AnyConnection>` (ブロックする) |
| `sockat::connect_any_nonblocking(path)` | `io::Result<AnyConnection>` (非ブロッキング) |

既定では無効で、feature `any` を有効にした時だけ使える: `sockat = { version = "0.4", features = ["any"] }`

`AnyConnection` は公開フィールドの struct で、`stream` (std の `UnixStream`) と `forked` (`bool`) を持つ ([DR-0026](./decisions/DR-0026-any-connection-and-feature-any.md))。

```rust
let sockat::AnyConnection { stream, forked } = sockat::connect_any(path)?;
```

試す順番は `connect` と同じで、他の経路 ([スレッド cwd](#スレッド-cwd) を含む) が全部使えない時に最後に [fork](#fork) の経路で connect する。`forked` は、fork の経路を通った時に `true`、通らなかった時は `false`。`true` の時は下の副作用が乗っているので、ログに残す・警告するなどに使える。FreeBSD では fork しないので、常に `false` になる。

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
| symlink | もう無い symlink を通したパス |
| スレッド cwd、fork、bindat | ファイル名だけ |

元のパスは自分で持っておき、それを使う。

OpenBSD では、103 バイト以内の短いパス (sockat は std にそのまま渡す) でも、std の `local_addr()` がパスの末尾 1 バイトを欠いて返す (CI で観測)。sockat ではなく std / OS の振る舞いで、std の `UnixListener::bind` を直接使っても同じになる。

### 名前で終わらないパスは InvalidInput

std にそのまま渡せない長さのパスで、最後がファイル名でないもの (`dir/`、`dir/.`、`dir/..`、`..`) は、何も作らずに `InvalidInput` を返す。ディレクトリとファイル名に分けられないからである。それより短いパスは std にそのまま渡すので、カーネルのエラーになる (例えば `dir/` の bind は macOS でも Linux でも `EADDRINUSE`)。

### fork の経路の、相手から見えない副作用

fork の経路 (`bind` と `connect_any` 系) では、fork の性質として次が起きる。どれも相手 (サーバ / クライアント) からは見えない。

- 子は終了するまでプロセスの全 fd の複製を持つ。その瞬間に他のスレッドが閉じたソケットやパイプは、子が終わるまで開いたままになる (相手が EOF を見るのが少し遅れる。閉じた listener の backlog にさらに接続が入りうる)
- 呼び出し元のプロセスに `SIGCHLD` が届く
- `pthread_atfork` で登録された handler が走る
- 監視ツール (プロセスの一覧、監査ログ等) に短命の子が見える
- 他のスレッドの `waitpid(-1)` が子を刈り取りうる。その時はソケットの状態から結果を読む (失敗は `Other`)

### ディレクトリは呼ぶ少し前に引かれる

/proc・相対パス・bindat・スレッド cwd・fork の経路は、ディレクトリを先に引いて (開く、または実パスにする) から、ファイル名で bind / connect する。その間に途中のディレクトリが rename されたり symlink が張り替えられたりすると、引いた時点のディレクトリにソケットができる (connect ならそこを見る)。広げた sockaddr と symlink の経路は元のパスをそのままカーネルに解決させるので、この点は std と同じに振る舞う。

### 相対パスの経路は、他のスレッドの chdir に弱い

sockat 自身は cwd を変えない。相対パスを作る時に、それが同じディレクトリを指すことを確かめ、bind / connect の後に cwd が変わっていないことも確かめる。他のスレッドが間に cwd を変えていたら、結果を捨ててエラー (`Other`) を返す (できたかもしれないソケットファイルは、場所が分からないので消さない)。変えて戻した場合は見えない。bind / connect の間に cwd を変えるプログラムはそもそもプロセス中の全ての相対パスと競合していて、sockat はそれを安全にはせず、見える場合を検出するだけである。

### symlink が残ることがある

symlink を作ってから消すまでの間にプロセスが死ぬと、`TMPDIR` か `/tmp` の下に `sockat.XXXXXXXXXX` (ソケットのディレクトリかファイルを指す symlink、または fd で掴む形のディレクトリ) が残る。名前が `sockat.` で始まるので、sockat の残骸と分かる。fd で掴む形のディレクトリは、作った後に開けなかった時 (fd が尽きた等) も、自分の物か確かめられないので残る。

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
| NUL を含むパス (長さによらない。Linux / Android で先頭が NUL のパスは std にそのまま渡す) | `InvalidInput` |
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

`connect_nonblocking` は相手を待たないので、runtime の worker から直接呼べる。`bind` も相手を待たない (bind と listen はすぐ返る。スレッド cwd の経路ではスレッドの、fork の経路では子の終了を待つが、どちらも bind / connect してすぐ終わる)。

## 確かめていないこと

- Android は compile もしていない。Linux と同じ経路を使う。Android のアプリは seccomp の filter の下で動くので、スレッド cwd の経路は使われない見込み (確かめていない)
- NetBSD・OpenBSD・illumos は CI の VM で試験を回しているが、fork の経路で connect した時の peer の資格情報 (pid) の振る舞いと、fork の経路で bind した時にクライアントに見えるサーバの peer は確かめていない
- ブロックしない connect が backlog の満杯で返すエラーと、connect が呼び出しの中で終わることは、Linux と macOS でだけ確かめた
- macOS 以外の Apple の OS (iOS 等) は macOS と同じ経路を使うが、確かめたのは iOS の clippy だけ。`pthread_fchdir_np` があるかも確かめていない (無ければスレッド cwd の経路は使えないとして次へ進む)
