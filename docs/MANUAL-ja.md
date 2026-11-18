# sockat マニュアル

> [English](./MANUAL.md) | 日本語

sockat の使い方と、長いパスにどう届くか、長いパスで std と違う点のリファレンス。設計の判断と理由は [DR の一覧](./decisions/INDEX.md) にある (経路と試す順番の総論は [DR-0027](./decisions/DR-0027-own-directory-and-fallback-routes.md))。

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
| `sockat::bind_ex(path)` | (無し) | `io::Result<(UnixListener, SocketFile)>` ([後述](#ソケットのファイルを消す-bind_ex)) |
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
- ソケットファイルは渡したパスにできる。listener を閉じても消えない (std と同じ)。消す時は元のパスで `std::fs::remove_file(path)` するか、`bind_ex` を使う
- 自前で作るソケットは std と同じ設定 (close-on-exec、std が付ける OS (macOS / iOS、FreeBSD、NetBSD、DragonFly) では `SO_NOSIGPIPE`、std と同じ listen の backlog)

## connect と connect_nonblocking

- **`connect`** は std の `UnixStream::connect` の代わりに使う。ブロックする
- **`connect_nonblocking`** は async ランタイムで使う。ソケットを connect の前に非ブロッキングにし、そのまま返す。tokio の `UnixStream::from_std` にそのまま渡せる。相手の backlog が満杯なら待たずに失敗する (Linux は `EAGAIN`、macOS は `ECONNREFUSED`)。Unix domain の connect は呼び出しの中で終わるか失敗するので、後で待つものは無い (Linux と macOS で確かめた)

どちらも fork の経路だけは試さない ([長いパスへの届き方](#長いパスへの届き方))。fork でしか届かない時は、パスが `sun_path` に入らない普通の connect と同じく `ENAMETOOLONG` を返す。普通の環境 (`/tmp` が書ける) では、ディレクトリの深さにもファイル名の長さにもよらず届く ([どこから届かなくなるか](#どこから届かなくなるか))。

## 長いパスへの届き方

std が受け付ける長さ (標準に渡す長さ) までのパスは std にそのまま渡す。今の std ではどの OS でも、広げていない sockaddr に入る長さ (`sun_path` − 1、以下「sockaddr に入る長さ」) と同じ。

| OS | std にそのまま渡す長さ |
|---|---|
| Linux / Android / illumos | 107 バイトまで |
| macOS / iOS 等 Apple の OS / FreeBSD / NetBSD / OpenBSD | 103 バイトまで |

それを超えるパスは、まず自分専用のディレクトリを通す [中心の経路](#中心の経路) で、それが使えない時は connect 系だけ [予備の経路](#予備の経路-connect-系) で届く。最初に使えた経路で bind / connect する。どの経路も呼び出し元のプロセスの cwd も呼び出し元のスレッドの cwd も変えないので、マルチスレッドのプログラムから使える。

### OS ごとに試す順番

| OS | `bind` | `connect` / `connect_nonblocking` |
|---|---|---|
| Linux / Android | D の中で スレッド cwd → /proc 版 → [fork](#fork) ([中心の経路](#中心の経路)) | D の中でスレッド cwd → /proc 版、次に [/proc](#proc) → [D の中のファイルへの symlink](#d-の中のファイルへの-symlink) → [symlink](#symlink) |
| Linux / Android (`/proc` 無し) | D の中でスレッド cwd → fork | D の中でスレッド cwd、次に [相対パス](#相対パス) → symlink |
| macOS / iOS 等 Apple の OS | D の中でスレッド cwd → fork | D の中でスレッド cwd、次に 相対パス → [広げた sockaddr](#広げた-sockaddr) → symlink |
| FreeBSD | D の中で bindat | D の中で connectat、次に [connectat](#connectat) |
| NetBSD / OpenBSD / illumos / その他の Unix | D の中で fork | 相対パス → symlink |
| Windows | 対象外 (Unix 以外では compile エラー) | |

- `connect_any` 系は `connect` と同じ順に試し、最後に [fork](#fork) で connect する ([例外: connect_any](#例外-connect_any))
- 最初にソケットのディレクトリを `stat` で引く。無ければ `ENOENT` (ファイル名の長さによらない)
- 経路が実際に bind / connect して失敗した時 (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等) は、そのエラーを返して次を試さない。次へ進むのは、その経路自体が使えない時だけ

### 中心の経路

自分専用のディレクトリ D を作って fd で掴み、その中に乱数の名前の symlink R (ソケットのディレクトリの realpath を指す) を作り、D の fd を基準に `R/<ファイル名>` で bind / connect する。終わったら R と D を消す。

1. D の置き場に、mode 0700 のディレクトリ `sockat.XXXXXXXXXX` (英数 10 文字の乱数) を作る
2. それを名前を辿らずに (`O_NOFOLLOW`) 開き、ディレクトリで、持ち主が自分で、group と other に権限が無いことを確かめる。違えば (作ってから開くまでに他人が差し替えた) 使わず、消さない。通ったら 0700 にそろえる (umask が持ち主のビットを削っていても使えるように)
3. D の中に R を作る
4. 下の手段の、使える最初のもので `R/<ファイル名>` を bind / connect する
5. 成功でも失敗でも、R を消し、D はその名前がまだ開いた物を指す時だけ消す

| 手段 | OS | やり方 |
|---|---|---|
| スレッド cwd | Linux / Android、Apple の OS | この呼び出しのためだけのスレッドで、そのスレッドだけ cwd を D に移す (Linux は `unshare(CLONE_FS)` してから `fchdir`、Apple の OS は `pthread_fchdir_np`)。スレッドは呼び出しと一緒に終わる |
| /proc 版 | `/proc` を使える Linux / Android | `/proc/self/fd/<D の fd>/R/<ファイル名>` を std に渡す |
| bindat | FreeBSD | `bindat(2)` / `connectat(2)` に D の fd を渡す |
| fork | 全部 (bind だけ) | [fork](#fork) の子が D に `fchdir` して bind する |

- **D の置き場**は `TMPDIR` (未設定なら `/tmp`。相対パスでもよい)、`/tmp`、ソケットのディレクトリ (名前は `.sockat.XXXXXXXXXX`) の順。最初に D を作れた所を使う。置き場は安全 (sticky 等) でなくても深くてもよい (D は fd で掴み、綴りに置き場が出ない)
- 置き場を開けない、D を作れない、確かめた結果が違う、0700 にそろえられない、R を作れない時は、次の置き場に進む
- **どこにも D を作れない時**、bind はソケットのディレクトリに R (`.sockat.XXXXXXXXXX`) を作り、[fork](#fork) の子がソケットのディレクトリに `fchdir` して `.sockat.XXXXXXXXXX/<ファイル名>` で bind する。connect 系は予備の経路に進む
- 綴りは `R/<ファイル名>` で、R の 11 バイトの分、ファイル名は sockaddr に入る長さより短くなる (Linux / illumos 96 バイト、Apple の OS / BSD 92 バイトまで。/proc 版はおよそ 79 バイトまで、D 無しの fork は 88 / 84 バイトまで)
- スレッド cwd は**どの関数も試す**。呼び出し元のプロセスが bind / connect するので、サーバに見える peer の pid は呼び出し元のもの。fd の複製も無い。`connect_any` 系はこの経路では `forked` が `false`
- スレッド cwd を使えないのは次の時で、次の手段に進む:
  - Linux で seccomp の filter が掛かっている (docker の既定も掛かっている)。filter には `unshare` をエラーでなくプロセスの kill (SIGSYS) で断るもの (systemd の `SystemCallFilter=` の既定の動作等) があるので、filter が掛かっていれば `unshare` を呼ばない
  - Linux で `unshare` が断られた
  - Apple の OS で `pthread_fchdir_np` が無い・失敗した (公開ヘッダに無い関数なので、実行時に探す)
  - スレッドを作れない
- D は Linux では `O_PATH`、Apple の OS では `O_SEARCH`、他の OS では `O_RDONLY` で開く。0700 にそろえるのは、`/proc` を使える Linux は `/proc/self/fd/<fd>` の `chmod`、`/proc` の無い Linux は名前で `O_RDONLY` で開き直して同じ物か照合してから `fchmod`、他は `fchmod`
- 中心の経路はソケットのディレクトリを開かず、R を通して検索権限だけで届く。読み取り権限の無いディレクトリでも、通常の bind / connect と同じ権限で届く

### 予備の経路 (connect 系)

中心の経路が使えない時 (使える手段が無い、D をどこにも作れない、ファイル名が `R/` を足すと収まらない) に、connect 系だけが試す。connect した側のソケットは bind されず綴りが残らないので、綴りに乱数の部品を挟まない。

#### /proc

Linux / Android。ソケットのディレクトリを `O_PATH` で開き、`/proc/self/fd/<fd>/<ファイル名>` を std に渡す。綴り全体が 107 バイトまでなので、ファイル名はおよそ 90 バイトまで (fd 番号の桁数で変わる)。

#### D の中のファイルへの symlink

`/proc` を使える Linux / Android。中心の経路と同じく D を作って確かめ、その中にソケットのファイルを指す symlink `s` を作り、`/proc/self/fd/<D の fd>/s` で connect する。綴りはおよそ 20 バイトで、ファイル名の長さにもディレクトリの深さにもよらない。D の置き場は中心の経路と同じ (安全でなくてもよい)。

#### 相対パス

cwd からの相対パスが sockaddr に入る長さ以内なら、それを std に渡す。元のパスが絶対パスの時だけ使う。パスを文字列で書き直すだけで、cwd は変えない (`chdir` しない)。

- パスが cwd (`getcwd`、または cwd と同じディレクトリを指す `$PWD`) の下にあれば、前を削る。それで sockaddr に入る長さにならない時 (cwd の下に無い時も含む) は、両方を実パスに揃えて `..` で上る綴りも作る。短い方を使う
- 相対パスのディレクトリが元のディレクトリと `(st_dev, st_ino)` で一致しない時と、cwd が既に変わっている時は、この経路は使えないとして次に進む
- connect の後に cwd が変わっていないことも確かめる ([後述](#相対パスの経路は他のスレッドの-chdir-に弱い))

#### 広げた sockaddr

Apple の OS。XNU は `sun_path` が 104 バイトを超えるアドレスも、パスが 253 バイトまでなら受け付ける。元のパスが 253 バイト以内ならそれを、無理なら相対パスが 253 バイト以内ならそれを渡す。カーネルが受け付けない (`EINVAL` / `ENAMETOOLONG`) 時は次に進む。

`bind` は使わない。広げた形で bind したソケットのアドレスは `sockaddr_un` に入らず、`getsockname` が途中で切れたパスを返して、読む側 (CPython の asyncio の後始末、Go の `net.FileConn` 等) を壊すからである。

#### symlink

一時ディレクトリにソケットのファイルを指す `sockat.XXXXXXXXXX` の symlink を作り、それを通して connect して、すぐ消す。

- 渡す綴りは `<置き場>/sockat.XXXXXXXXXX`。`/tmp` なら 22 バイトで、ディレクトリの深さにもファイル名の長さにもよらない
- 置き場の候補は、`TMPDIR` (空でない絶対パスの時)、`/tmp` の順。綴りが sockaddr に入る長さに収まり、安全で、symlink を作れた最初の候補を使う。`TMPDIR` が深くて収まらない時は `/tmp` を使う
- **安全な置き場**は、他人が中の symlink を差し替えられないディレクトリで (ディレクトリでなければ安全でない)、次のどちらか。満たさない候補には何も作らず、次の候補に進む
  - sticky bit が立っていて、持ち主が root か自分 (`/tmp` がこれ)
  - 持ち主が自分で、group と other に書き込み権限が無い (macOS の `$TMPDIR` がこれ)
- 置き場は symlink を辿って判定する (macOS の `/tmp` は `/private/tmp` への symlink)
- symlink を作れない時 (置き場が無い・書けない、サンドボックス、空いている名前が見つからない、乱数が取れない) も次の候補に進む

#### connectat

FreeBSD。ソケットのディレクトリを開き、`connectat(2)` でファイル名を渡す。ファイル名は 103 バイトまで。

### fork

ディレクトリの fd に `fchdir` した子を fork し、子が綴りで bind / connect して、すぐ終了する。fork の後もソケットは親と子で同じものを指すので、子の bind / connect は呼び出し元のソケットにそのまま効く。bind の後の listen は必ず呼び出し元がする。

| 要求 | 子が `fchdir` する先 | 綴り |
|---|---|---|
| `bind` | D | `R/<ファイル名>` |
| `bind` (D をどこにも作れない時) | ソケットのディレクトリ | `.sockat.XXXXXXXXXX/<ファイル名>` |
| `connect_any` 系 (予備の経路の後) | ソケットのディレクトリ | ファイル名 |

- **`bind` と `connect_any` 系だけが試す。** `connect` / `connect_nonblocking` は試さずに `ENAMETOOLONG` を返す
- `connect_any` 系のファイル名は sockaddr に入る長さまで (Apple の OS では、カーネルが広げた sockaddr を受け付ける時 253 バイト)
- ソケットのディレクトリを開くのは `connect_any` 系と D 無しの fork だけ。macOS は `O_SEARCH`、Linux は `O_PATH` で開くので通常の bind / connect と同じ権限で足り、他の Unix は `O_RDONLY` で開くので読み取り権限も要る (開けなければこの経路は使えない)
- bind は子が bind するだけで listen は呼び出し元なので、fork の経路でも相手から見える副作用が無い。クライアントに見えるサーバの peer (macOS の `LOCAL_PEERPID`、Linux の `SO_PEERCRED`) は、send の前から呼び出し元 ([DR-0025](./decisions/DR-0025-fork-bind-listen-by-parent.md))。fork しない版の bind は無い
- 子の区間は async-signal-safe な呼び出し (`fchdir`、`bind` / `connect`、`_exit`) だけなので、マルチスレッドのプロセスから呼べる
- 副作用がある: [connect した時の peer の pid](#fork-の経路で-connect-した時の副作用) (相手から見える。connect だけ)、[相手から見えない副作用](#fork-の経路の相手から見えない副作用) (bind でも connect でも)

### どこから届かなくなるか

std にそのまま渡せない長さのパスが `ENAMETOOLONG` になる時 (ディレクトリがあり、D を作れる前提。ディレクトリが無ければ `ENOENT`)。

| OS | `bind` | `connect` / `connect_nonblocking` | `connect_any` 系 |
|---|---|---|---|
| Linux / Android (`/proc` あり) | ファイル名が 96 バイトを超える | 届く (D の中のファイルへの symlink) | 届く |
| Linux / Android (`/proc` 無し) | ファイル名が 96 バイトを超える | 中心の経路が使えず (ファイル名が 96 バイトを超えるかスレッド cwd が使えない)、相対パスが 107 バイトを超え、symlink の経路も使えない | ファイル名が 107 バイトを超え、symlink の経路も使えない |
| FreeBSD | ファイル名が 92 バイトを超える | ファイル名が 103 バイトを超える | ファイル名が 103 バイトを超える |
| macOS / iOS 等 Apple の OS | ファイル名が 92 バイトを超える | 中心の経路が使えず (ファイル名が 92 バイトを超えるかスレッド cwd が使えない)、元のパスも相対パスも 253 バイトを超え、symlink の経路も使えない | ファイル名が 253 バイトを超え、symlink の経路も使えない |
| NetBSD / OpenBSD / その他の BSD | ファイル名が 92 バイトを超える | 相対パスが 103 バイトを超え、symlink の経路も使えない | ファイル名が 103 バイトを超え、symlink の経路も使えない |
| illumos | ファイル名が 96 バイトを超える | 相対パスが 107 バイトを超え、symlink の経路も使えない | ファイル名が 107 バイトを超え、symlink の経路も使えない |

- Linux でスレッド cwd が使えない時 (seccomp の filter 等)、bind はファイル名およそ 79 バイトまで /proc 版、その先 96 バイトまでは fork
- D をどこにも作れない時、bind は D 無しの fork でファイル名 88 バイト (Apple の OS / BSD は 84) まで届く (ソケットのディレクトリに R を作れる時)。connect 系は予備の経路の分だけ届く (D の中のファイルへの symlink は使えない)
- Apple の OS の connect 系の 253 バイトは、カーネルが広げた sockaddr を受け付ける時。断った時は 103 バイト
- connect で symlink の経路が使えないのは、安全で symlink を作れる置き場が無い時 (`TMPDIR` も `/tmp` も使えないサンドボックス等)。`/tmp` が使えれば長さは足りる
- PATH_MAX (macOS 1024、Linux 4096) を超えるパスは `ENAMETOOLONG`。ディレクトリを引く等でフルパスを渡すからで、sockat の責務外

### NUL を含むパス

NUL を含むパスは、長さによらず、何も作らずに `InvalidInput` を返す。カーネルはパスを最初の NUL までしか読まないので、通すと別の場所にソケットができうるからである。std も NUL を含むパスを `InvalidInput` で断るので、短いパスでは std と同じ種類のエラーになる (文言は sockat のもの)。

Linux / Android で先頭が NUL のパスは abstract namespace の名前でファイルではないので、sockat の対象外で、長さによらず std にそのまま渡す。std は `Path` の中の NUL をどれも断るので、結果は std と同じ `InvalidInput` になる。abstract namespace のソケットは、std の `SocketAddr::from_abstract_name` と `UnixListener::bind_addr` / `UnixStream::connect_addr` で扱う。

## ソケットのファイルを消す: bind_ex

`bind_ex` は `bind` と同じ経路で bind し、listener と、ソケットのファイルを後で消す口 `SocketFile` を返す。

```rust
let (listener, file) = sockat::bind_ex(path)?;
// listener を使う (tokio 等に渡してもよい)
drop(listener);
file.discard()?;
```

- `bind_ex` は bind が通った直後、経路の中で listener を返す前に、ソケットのファイルの realpath と `(st_dev, st_ino)` を覚える (中心の経路では D と R が残っている間に、bind と同じ綴りで引く)。覚えられなければ listener を閉じ、ファイルは消さずにエラーを返す
- `SocketFile::discard()` は、その realpath にある物の `(st_dev, st_ino)` が同じ時だけ消す。別の物 (後から同じパスに bind された別のソケット、ファイル、symlink) は消さず、何も無ければ何もしない。どちらも `Ok(())`
- `discard()` は listener を閉じない。listener は std の `UnixListener` で、tokio 等へ渡せる。閉じるのは listener を持っている側で、閉じてから `discard()` する
- `SocketFile` を drop しても何もしない
- 短いパス (std にそのまま渡す) でも同じ
- 限界: 確かめてから消すまでと、bind してから覚えるまでの間に、ソケットのディレクトリに書ける者がファイルをすり替えるのは防げない (inode を指定して消す手段が POSIX に無い)。消されたファイルの inode 番号が同じデバイスの新しいファイルに使い回されると、そのファイルを bind したものと見なす

## 例外: connect_any

`connect_any` / `connect_any_nonblocking` は、`connect` が上の表で `ENAMETOOLONG` になるよほど珍しい環境で、fork の副作用を受け入れてでもつなぎたい時だけのもの。普通は使わない。

要るのは、中心の経路を connect に使えず、予備の経路も届かない時だけ:

- 中心の経路を connect に使えない: NetBSD / OpenBSD / illumos 等 (スレッド cwd が無い)、seccomp の filter が掛かり `/proc` の無い Linux、`pthread_fchdir_np` の無い Apple の OS
- 予備の経路も届かない: 安全で symlink を作れる置き場が無く、相対パスにも (Apple の OS は広げた sockaddr にも) 収まらない

| 関数 | 戻り値 |
|---|---|
| `sockat::connect_any(path)` | `io::Result<AnyConnection>` (ブロックする) |
| `sockat::connect_any_nonblocking(path)` | `io::Result<AnyConnection>` (非ブロッキング) |

既定では無効で、feature `any` を有効にした時だけ使える: `sockat = { version = "0.4", features = ["any"] }`

`AnyConnection` は公開フィールドの struct で、`stream` (std の `UnixStream`) と `forked` (`bool`) を持つ ([DR-0026](./decisions/DR-0026-any-connection-and-feature-any.md))。

```rust
let sockat::AnyConnection { stream, forked } = sockat::connect_any(path)?;
```

試す順番は `connect` と同じで、他の経路が全部使えない時に最後に [fork](#fork) の経路で connect する。`forked` は、fork の経路を通った時に `true`、通らなかった時は `false`。`true` の時は下の副作用が乗っているので、ログに残す・警告するなどに使える。FreeBSD では connectat がファイル名 103 バイトまで届くので、fork の経路まで来ることは無い。

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
| スレッド cwd、fork、bindat (D の中) | `R/<ファイル名>` |
| /proc 版 | `/proc/self/fd/<fd>/R/<ファイル名>` |
| D 無しの fork | `.sockat.XXXXXXXXXX/<ファイル名>` |

R (と `.sockat.XXXXXXXXXX`) は bind の後に消えているので、返ってきた綴りでファイルを消す・`stat` する・`chmod` しても、何にも当たらない (fd の番号が使い回されても、どの cwd から解決しても)。ソケット自体にも当たらないので、元のパスは自分で持っておき、それを使う (消すなら `bind_ex` も使える)。同じ綴りは `Debug` の表示や、フレームワークのログ・接続元の情報にも出る。

OpenBSD では、103 バイト以内の短いパス (sockat は std にそのまま渡す) でも、std の `local_addr()` がパスの末尾 1 バイトを欠いて返す (CI で観測)。sockat ではなく std / OS の振る舞いで、std の `UnixListener::bind` を直接使っても同じになる。

### bind できるファイル名は connect より短い

bind の綴りには R の 11 バイトが入るので、bind できるファイル名は Linux / illumos で 96 バイト、Apple の OS / BSD で 92 バイトまで。connect は予備の経路で、ふつうはファイル名の長さによらず届く。

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

R はソケットのディレクトリの realpath を、R を作った時に指す。予備の経路の /proc・相対パス・connectat と、`connect_any` 系の fork は、ディレクトリを先に引いて (開く、または実パスにする) からファイル名で connect する。その間に途中のディレクトリが rename されたり symlink が張り替えられたりすると、引いた時点のディレクトリにソケットができる (connect ならそこを見る)。広げた sockaddr と symlink の経路は元のパスをそのままカーネルに解決させるので、この点は std と同じに振る舞う。

### 相対パスの経路は、他のスレッドの chdir に弱い

sockat 自身は cwd を変えない。相対パスを作る時に、それが同じディレクトリを指すことを確かめ、connect の後に cwd が変わっていないことも確かめる。他のスレッドが間に cwd を変えていたら、結果を捨てて (connect したソケットを閉じて) エラー (`Other`) を返す。変えて戻した場合は見えない。connect の間に cwd を変えるプログラムはそもそもプロセス中の全ての相対パスと競合していて、sockat はそれを安全にはせず、見える場合を検出するだけである。

### 残るものがある

呼び出しの途中でプロセスが死ぬと、`TMPDIR` か `/tmp` に `sockat.XXXXXXXXXX` (D か symlink) が、ソケットのディレクトリに `.sockat.XXXXXXXXXX` (D か R) が残る。名前で sockat の残骸と分かる。D を `O_RDONLY` で開く OS (Linux と Apple の OS 以外) では、umask が持ち主の読み取りを削っていると、作った D を開けず確かめられないので、触らずに残す。

## エラー

| 状況 | エラー |
|---|---|
| ディレクトリが無い | `ENOENT` |
| ディレクトリの所にディレクトリでない物がある | `ENOTDIR` |
| 長いパスで、ソケットのディレクトリの実パス (realpath) を引けない | そのエラー (例: cwd が消された `./<名前>` は `ENOENT`) |
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

`connect_nonblocking` は相手を待たないので、runtime の worker から直接呼べる。`bind` も相手を待たない (bind と listen はすぐ返る。スレッド cwd の経路ではスレッドの、fork の経路では子の終了を待つが、どちらも bind / connect してすぐ終わる)。`bind_ex` の listener も同じく渡せて、`SocketFile` は手元に残る。

## 確かめていないこと

- Android は compile もしていない。Linux と同じ経路を使う。Android のアプリは seccomp の filter の下で動くので、スレッド cwd は使われない見込み (確かめていない)
- NetBSD・OpenBSD・illumos・FreeBSD は CI の VM で試験を回している。fork の経路で connect した時の peer の資格情報 (pid) の振る舞いと、fork の経路で bind した時にクライアントに見えるサーバの peer は確かめていない
- ブロックしない connect が backlog の満杯で返すエラーと、connect が呼び出しの中で終わることは、Linux と macOS でだけ確かめた
- macOS 以外の Apple の OS (iOS 等) は macOS と同じ経路を使うが、確かめたのは iOS の clippy だけ。`pthread_fchdir_np` があるかも確かめていない (無ければスレッド cwd は使えないとして次へ進む)
