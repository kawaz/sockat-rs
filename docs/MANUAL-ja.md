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
| `sockat::connect(path)` | `UnixStream::connect(path)` | `io::Result<UnixStream>` |
| `sockat::connect_nonblocking(path)` | (無し) | `io::Result<UnixStream>` (非ブロッキング) |
| `sockat::bind_ex(path, last_resort)` | (無し) | `Result<ExListener, ExError>` (listener と、ソケットのファイルを消す `SocketFile`・失ったもの・経路。[後述](#後始末と診断つきの-_ex)) |
| `sockat::connect_ex(path, last_resort)` | (無し) | `Result<ExStream, ExError>` (stream と、失ったもの・経路) |
| `sockat::connect_nonblocking_ex(path, last_resort)` | (無し) | 同上 (非ブロッキング) |

`last_resort` は `sockat::LastResort::Avoid` (既定の関数と同じ経路) か `sockat::LastResort::Allow` (何かを失ってでも届かせる最終手段も試す)。

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

どちらも [fork-cwd-direct](#fork-cwd-direct) は試さない。fork-cwd-direct でしか届かない時は、パスが `sun_path` に入らない普通の connect と同じく `ENAMETOOLONG` を返す。普通の環境 (`/tmp` が書ける) では、ディレクトリの深さにもファイル名の長さにもよらず届く ([どこから届かなくなるか](#どこから届かなくなるか))。

## 長いパスへの届き方

std が受け付ける長さ (標準に渡す長さ) までのパスは std にそのまま渡す ([standard](#standard))。今の std ではどの OS でも、広げていない sockaddr に入る長さ (`sun_path` − 1、以下「sockaddr に入る長さ」) と同じ。

| OS | std にそのまま渡す長さ |
|---|---|
| Linux / Android / illumos | 107 バイトまで |
| macOS / iOS 等 Apple の OS / FreeBSD / NetBSD / OpenBSD | 103 バイトまで |

それを超えるパスは、bind はソケットのファイルを指す symlink ([symlink-file](#symlink-file)。bind が symlink を辿る OS) か、自分専用のディレクトリ D を通す経路で、それが使えない時は [D 無しの形](#d-無しの形) で届く。connect は、書き込み (D・R・symlink を作る) もスレッドも要らない [connect の経路](#connect-の経路) を先に試し、D を通す経路と symlink をその後に試す。最初に使えた経路で bind / connect する。どの経路も呼び出し元のプロセスの cwd も呼び出し元のスレッドの cwd も変えないので、マルチスレッドのプログラムから使える。経路の名前は [sockat の仕様](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) の「経路の名前」と同じ。

### OS ごとに試す順番

| OS | `bind` | `connect` / `connect_nonblocking` |
|---|---|---|
| Linux / Android | [thread-cwd](#thread-cwd) → [proc](#proc) → [fork-cwd](#fork-cwd) | [proc-direct](#proc-direct) → thread-cwd → proc → [proc-held](#proc-held) → [symlink-file](#symlink-file) |
| Linux / Android (`/proc` 無し) | thread-cwd → fork-cwd | [relative](#relative) → thread-cwd → symlink-file |
| macOS / iOS 等 Apple の OS | symlink-file → thread-cwd → fork-cwd | relative → [widened](#widened) → thread-cwd → symlink-file |
| FreeBSD | [at](#at) | [at-direct](#at-direct) → at |
| NetBSD / その他の Unix | symlink-file → fork-cwd | relative → symlink-file |
| OpenBSD / illumos | fork-cwd | relative → symlink-file |
| Windows | 対象外 (Unix 以外では compile エラー) | |

- 長いパスの前に [standard](#standard) が試され、長さで使えない
- connect は、書き込みもスレッドも要らない経路 (proc-direct・relative・widened・at-direct) を先に、D を作る経路 (thread-cwd・proc・proc-held・at) とスレッドを立てる経路 (thread-cwd)、symlink を作る経路をその後に試す。前の経路で届けば D も symlink も作らない
- `bind` の symlink-file は、bind が symlink を辿らないと確かめた OS (Linux / Android、FreeBSD、OpenBSD、illumos) では試さない。辿ると確かめたのは macOS と NetBSD。他の OS では、辿るかを実行時に見分ける ([symlink-file](#symlink-file))
- `bind` は D をどこにも作れない時、D を通す経路と同じ順を [D 無しの形](#d-無しの形) (`-sockdir`) で試す
- `_ex` は `LastResort::Avoid` なら既定の関数と同じ順に試す。`LastResort::Allow` なら、その後に最終手段を試す: `bind_ex` は [symlink-dir](#symlink-dir)、`connect_ex` 系は [fork-cwd-direct](#fork-cwd-direct) ([後始末と診断つきの _ex](#後始末と診断つきの-_ex))
- 最初にソケットのディレクトリを `stat` で引く。無ければ `ENOENT` (ファイル名の長さによらない)
- 経路が実際に bind / connect して失敗した時 (`ENOENT`、`EADDRINUSE`、`ECONNREFUSED` 等) は、そのエラーを返して次を試さない。次へ進むのは、その経路自体が使えない時だけ

### standard

std にそのまま渡す。パスが std にそのまま渡す長さ以下の時だけ使える。

### D を通す経路

自分専用のディレクトリ D を作って fd で掴み、その中に乱数の名前の symlink R (ソケットのディレクトリの realpath を指す) を作り、D の fd を基準に `R/<ファイル名>` で bind / connect する。終わったら R と D を消す。[thread-cwd](#thread-cwd)・[at](#at)・[proc](#proc)・[fork-cwd](#fork-cwd) がこの形。connect の [proc-held](#proc-held) も、R の代わりにソケットのファイルを指す `s` を置いて同じ D を使う。

1. D の置き場に、mode 0700 のディレクトリ `sockat.XXXXXXXXXX` (英数 10 文字の乱数) を作る
2. それを名前を辿らずに (`O_NOFOLLOW`) 開き、ディレクトリで、持ち主が自分で、group と other に権限が無いことを確かめる。違えば (作ってから開くまでに他人が差し替えた) 使わず、消さない。通ったら 0700 にそろえる (umask が持ち主のビットを削っていても使えるように)
3. D の中に R を作る
4. 使える最初の経路で `R/<ファイル名>` を bind / connect する
5. 成功でも失敗でも、R を消し、D はその名前がまだ開いた物を指す時だけ消す

- D は 1 回の呼び出しで 1 度だけ作り、D を使う経路で共有する (thread-cwd が断られた後の proc、proc の綴りに収まらなかった後の proc-held は、同じ D を使う)。どこにも作れなければ、後の D を使う経路はどれも飛ばし、作り直さない
- **D の置き場**は `TMPDIR` (未設定なら `/tmp`。相対パスでもよい)、`/tmp`、ソケットのディレクトリ (名前は `.sockat.XXXXXXXXXX`) の順。最初に D を作れた所を使う。置き場は安全 (sticky 等) でなくても深くてもよい (D は fd で掴み、綴りに置き場が出ない)
- 置き場を開けない、D を作れない、確かめた結果が違う、0700 にそろえられない、R (proc-held が最初に D を求めた時は `s`) を作れない時は、次の置き場に進む
- 作った直後の D を開けない時 (umask が持ち主のビットを削り、Apple の OS は検索、Linux と Apple の OS 以外は読み取りの権限が無い) は、名前で引いて 2 の条件を満たせば消し、次の置き場に進む。名前で確かめてから消すまでに差し替えられても、空のディレクトリしか消えない
- 綴りは `R/<ファイル名>` で、R の 11 バイトの分、ファイル名は sockaddr に入る長さより短くなる (Linux / illumos 96 バイト、Apple の OS / BSD 92 バイトまで。proc はおよそ 79 バイトまで)。macOS と NetBSD の bind は [symlink-file](#symlink-file) が先に届くので、この上限は効かない
- D は Linux では `O_PATH`、Apple の OS では `O_SEARCH`、他の OS では `O_RDONLY` で開く。0700 にそろえるのは、`/proc` を使える Linux は `/proc/self/fd/<fd>` の `chmod`、`/proc` の無い Linux は名前で `O_RDONLY` で開き直して同じ物か照合してから `fchmod`、他は `fchmod`
- D を通す経路はソケットのディレクトリを開かず (開くのは D をソケットのディレクトリに作る時だけ)、R を通して検索権限だけで届く。読み取り権限の無いディレクトリでも、通常の bind / connect と同じ権限で届く

#### thread-cwd

Linux / Android、Apple の OS。この呼び出しのためだけのスレッドで、そのスレッドだけ cwd を D に移し (Linux は `unshare(CLONE_FS)` してから `fchdir`、Apple の OS は `pthread_fchdir_np`)、`R/<ファイル名>` で bind / connect する。スレッドは呼び出しと一緒に終わる。

- **どの関数も試す**。呼び出し元のプロセスが bind / connect するので、サーバに見える peer の pid は呼び出し元のもの。fd の複製も無い
- 使えないのは次の時で、次の経路に進む:
  - Linux で seccomp の filter が掛かっている (docker の既定も掛かっている)。filter には `unshare` をエラーでなくプロセスの kill (SIGSYS) で断るもの (systemd の `SystemCallFilter=` の既定の動作等) があるので、filter が掛かっていれば `unshare` を呼ばない
  - Linux で `unshare` が断られた
  - Apple の OS で `pthread_fchdir_np` が無い・失敗した (公開ヘッダに無い関数なので、実行時に探す)
  - スレッドを作れない

#### at

FreeBSD。`bindat(2)` / `connectat(2)` に D の fd と `R/<ファイル名>` を渡す。スレッドを使わない。

#### proc

`/proc` を使える Linux / Android。スレッドを使わず `/proc/self/fd/<D の fd>/R/<ファイル名>` を std に渡す。綴り全体が 107 バイトまでなので、ファイル名はおよそ 79 バイトまで (fd 番号の桁数で変わる)。D を作る前に見積もりで確かめ、D を開いた後に D の fd で確かめ直す。収まらなければこの経路を飛ばし、D は片付けずに後の経路 (bind は fork-cwd、connect は proc-held) と共有する。`/proc` を使えるかは `/` を開いた fd で確かめる (ソケットのディレクトリは開かない)。

#### fork-cwd

bind だけ。[fork の子](#fork-の子) が D に `fchdir` して `R/<ファイル名>` で bind する。thread-cwd・proc が使えない (`/proc` の綴りに収まらない時を含む) 時に試す。

### D 無しの形

D をどこにも作れない時の bind が使う。ソケットのディレクトリに R (`.sockat.XXXXXXXXXX`) を作り、D の代わりにソケットのディレクトリの fd を基準に `.sockat.XXXXXXXXXX/<ファイル名>` で bind する。経路は D を通す経路と同じ順 (thread-cwd-sockdir (FreeBSD は at-sockdir) → proc-sockdir `/proc/self/fd/<fd>/.sockat.XXXXXXXXXX/<ファイル名>` → fork-cwd-sockdir)。終わったら R を消す。

- 綴りは 19 バイト + ファイル名 (ファイル名は Linux / illumos 88、Apple の OS / BSD 84 バイトまで。proc-sockdir はおよそ 72 バイトまで)
- ソケットのディレクトリを開けない、R も作れない時は `ENAMETOOLONG` (`LastResort::Allow` の `bind_ex` はこの後に [symlink-dir](#symlink-dir) を試す)
- connect は D 無しの形を使わず、[OS ごとに試す順番](#os-ごとに試す順番) の次の経路に進む

### connect の経路

connect だけが使う経路。connect した側のソケットは bind されず綴りが残らないので、綴りに乱数の部品を挟まない。proc-direct・relative・widened・at-direct は書き込みもスレッドも要らないので、connect の順の先頭に置く。

#### proc-direct

`/proc` を使える Linux / Android。connect の順の最初。ソケットのディレクトリを `O_PATH` で開き、`/proc/self/fd/<fd>/<ファイル名>` を std に渡す。綴り全体が 107 バイトまでなので、ファイル名はおよそ 90 バイトまで (fd 番号の桁数で変わる)。ソケットのディレクトリを開けない、`/proc/self/fd/<fd>` を引けない時は次の経路に進む。

#### proc-held

`/proc` を使える Linux / Android。[proc](#proc) の次。D (前の経路が作っていればその D、無ければ作って確かめる) の中にソケットのファイルを指す symlink `s` を作り、`/proc/self/fd/<D の fd>/s` で connect する。綴りはおよそ 20 バイトで、ファイル名の長さにもディレクトリの深さにもよらない。D の置き場は D を通す経路と同じ (安全でなくてもよい)。ソケットのディレクトリは開かない。

#### relative

`/proc` の無い Linux / Android、Apple の OS、NetBSD / OpenBSD / illumos / その他の Unix の connect の順の最初。cwd からの相対パスが sockaddr に入る長さ以内なら、それを std に渡す。元のパスが絶対パスの時だけ使う。パスを文字列で書き直すだけで、cwd は変えない (`chdir` しない)。

- パスが cwd (`getcwd`、または cwd と同じディレクトリを指す `$PWD`) の下にあれば、前を削る。それで sockaddr に入る長さにならない時 (cwd の下に無い時も含む) は、両方を実パスに揃えて `..` で上る綴りも作る。短い方を使う
- 相対パスのディレクトリが元のディレクトリと `(st_dev, st_ino)` で一致しない時と、cwd が既に変わっている時は、この経路は使えないとして次に進む
- connect の後に cwd が変わっていないことも確かめる ([後述](#relative-は他のスレッドの-chdir-に弱い))

#### widened

Apple の OS。relative の次。XNU は `sun_path` が 104 バイトを超えるアドレスも、パスが 253 バイトまでなら受け付ける。元のパスが 253 バイト以内ならそれを、無理なら相対パスが 253 バイト以内ならそれを渡す。カーネルが受け付けない (`EINVAL` / `ENAMETOOLONG`) 時は次に進み、[fork-cwd-direct](#fork-cwd-direct) も広げた sockaddr を使わない。

`bind` は使わない。広げた形で bind したソケットのアドレスは `sockaddr_un` に入らず、`getsockname` が途中で切れたパスを返して、読む側 (CPython の asyncio の後始末、Go の `net.FileConn` 等) を壊すからである。

#### symlink-file

置き場に、ソケットのファイルを指す `sockat.XXXXXXXXXX` の symlink を作り、その綴り `<置き場>/sockat.XXXXXXXXXX` で bind / connect して、すぐ消す。綴りにファイル名が出ないので、ファイル名の長さにもディレクトリの深さにもよらない (`/tmp` なら 22 バイト)。

- connect: connect の順の最後 (FreeBSD 以外)
- bind: standard の次 (Linux / Android、FreeBSD、OpenBSD、illumos 以外)。bind が最後の要素の symlink を辿る OS でだけ元のパスにソケットができる (macOS と NetBSD で確かめた。Linux / Android、FreeBSD、OpenBSD、illumos は辿らないので試さない。他の OS は実行時に見分ける)。bind が失敗した時は元のパスを `lstat` し、物があればそのエラー (`EADDRINUSE` 等) を返して次を試さない。無いか確かめられなければ、カーネルが辿らなかったとみなして次の経路に進む (本当の失敗なら次の経路でも同じ失敗になり、そこで返る)
- bind でカーネルが返すアドレスは symlink の綴り。その綴りで消しても、最後の要素の symlink は辿られず置き場の中の項目が消えるだけ (共有の sticky な置き場では他人の項目は消せない) なので、失うのは `respelled` だけ
- symlink の先はソケットのファイルの綴り (相対パスなら cwd を前に付ける)

#### symlink-dir

`LastResort::Allow` の `bind_ex` だけが、既定の bind の経路が全部使えない時に試す。置き場に、ソケットのディレクトリの realpath を指す `sockat.XXXXXXXXXX` の symlink を作り、`<置き場>/sockat.XXXXXXXXXX/<ファイル名>` で bind して、すぐ消す。綴りが `R/<ファイル名>` より長いので、ファイル名が長すぎて `bind` が届かない時には役に立たない。届かせるのは、D も D 無しの形も使えない時。

#### symlink の置き場

| | symlink-file | symlink-dir |
|---|---|---|
| 置き場の候補 | `TMPDIR` (空でない絶対パスの時)、`/tmp` の順 | `XDG_RUNTIME_DIR`、`TMPDIR` (どちらも空でない絶対パスの時)、`/tmp` の順。同じ綴りは 1 回 |
| 試し方 | 安全な置き場を候補の順に | 持ち主だけが書ける置き場を候補の順に、無ければ共有の置き場を候補の順に |

- **持ち主だけが書ける置き場**: 持ち主が自分で、group と other に書き込み権限が無い (macOS の `$TMPDIR`、Linux の `$XDG_RUNTIME_DIR` がこれ)
- **共有の置き場**: sticky bit が立っていて、持ち主が root か自分 (`/tmp` がこれ)。誰でも中に作れるので、symlink-dir で bind した綴りは、symlink を消した後に他の利用者が作り直せる (`recreatable-spelling`)
- **安全な置き場**: どちらか。他人が中の symlink を差し替えられない (ディレクトリでなければどちらでもない)
- 置き場は symlink を辿って判定する (macOS の `/tmp` は `/private/tmp` への symlink)
- 綴りが sockaddr に入る長さに収まらない、条件を満たさない、symlink を作れない (置き場が無い・書けない、サンドボックス、空いている名前が見つからない、乱数が取れない) 候補には何も作らず、次の候補に進む。`TMPDIR` が深くて収まらない時は `/tmp` を使う

#### at-direct

FreeBSD。connect の順の最初。ソケットのディレクトリを開き、`connectat(2)` でファイル名を渡す。ファイル名は 103 バイトまで。ソケットのディレクトリを開けない時は [at](#at) に進む。

#### fork-cwd-direct

`LastResort::Allow` の `connect_ex` 系だけが、connect の順が全部使えない時に最後に試す。[fork の子](#fork-の子) がソケットのディレクトリに `fchdir` してファイル名で connect する。Apple の OS で、カーネルが広げた sockaddr を断っていなければ、103 バイトを超えるファイル名は広げた sockaddr で渡す。

- `connect` / `connect_nonblocking` はここまで来ると `ENAMETOOLONG` を返す
- ファイル名は sockaddr に入る長さまで (Apple の OS では、カーネルが広げた sockaddr を受け付ける時 253 バイト)

### fork の子

[fork-cwd](#fork-cwd)・fork-cwd-sockdir・[fork-cwd-direct](#fork-cwd-direct) が使う。ディレクトリの fd に `fchdir` した子を fork し、子が綴りで bind / connect して、すぐ終了する。fork の後もソケットは親と子で同じものを指すので、子の bind / connect は呼び出し元のソケットにそのまま効く。bind の後の listen は必ず呼び出し元がする。

| 経路 | 子が `fchdir` する先 | 綴り |
|---|---|---|
| fork-cwd | D | `R/<ファイル名>` |
| fork-cwd-sockdir | ソケットのディレクトリ | `.sockat.XXXXXXXXXX/<ファイル名>` |
| fork-cwd-direct | ソケットのディレクトリ | ファイル名 |

- ソケットのディレクトリは macOS は `O_SEARCH`、Linux は `O_PATH` で開くので通常の bind / connect と同じ権限で足り、他の Unix は `O_RDONLY` で開くので読み取り権限も要る (開けなければその経路は使えない)
- bind は子が bind するだけで listen は呼び出し元なので、fork の経路でも相手から見える副作用が無い。クライアントに見えるサーバの peer (macOS の `LOCAL_PEERPID`、Linux の `SO_PEERCRED`) は、send の前から呼び出し元 ([DR-0025](./decisions/DR-0025-fork-bind-listen-by-parent.md))。fork しない版の bind は無い
- 子の区間は async-signal-safe な呼び出し (`fchdir`、`bind` / `connect`、`_exit`) だけなので、マルチスレッドのプロセスから呼べる
- 副作用がある: [connect した時の peer の pid](#fork-cwd-direct-で-connect-した時の副作用) (相手から見える。fork-cwd-direct だけ)、[相手から見えない副作用](#fork-の経路の相手から見えない副作用) (fork の経路すべて)

### どこから届かなくなるか

std にそのまま渡せない長さのパスが `ENAMETOOLONG` になる時 (ディレクトリがあり、D を作れる前提。ディレクトリが無ければ `ENOENT`)。

| OS | `bind` | `connect` / `connect_nonblocking` | `LastResort::Allow` の `connect_ex` 系 |
|---|---|---|---|
| Linux / Android (`/proc` あり) | ファイル名が 96 バイトを超える | 届く (proc-held) | 届く |
| Linux / Android (`/proc` 無し) | ファイル名が 96 バイトを超える | relative が 107 バイトを超え、thread-cwd が使えず (ファイル名が 96 バイトを超えるかスレッド cwd が使えない)、symlink-file も使えない | ファイル名が 107 バイトを超え、symlink-file も使えない |
| FreeBSD | ファイル名が 92 バイトを超える | ファイル名が 103 バイトを超える | ファイル名が 103 バイトを超える |
| macOS | 届く (symlink-file)。symlink-file が使えない時はファイル名が 92 バイトを超える | 元のパスも相対パスも 253 バイトを超え、thread-cwd が使えず (ファイル名が 92 バイトを超えるかスレッド cwd が使えない)、symlink-file も使えない | ファイル名が 253 バイトを超え、symlink-file も使えない |
| iOS 等 macOS 以外の Apple の OS | ファイル名が 92 バイトを超える | macOS と同じ | macOS と同じ |
| NetBSD | 届く (symlink-file)。symlink-file が使えない時はファイル名が 92 バイトを超える | relative が 103 バイトを超え、symlink-file も使えない | ファイル名が 103 バイトを超え、symlink-file も使えない |
| OpenBSD / その他の BSD | ファイル名が 92 バイトを超える | relative が 103 バイトを超え、symlink-file も使えない | ファイル名が 103 バイトを超え、symlink-file も使えない |
| illumos | ファイル名が 96 バイトを超える | relative が 107 バイトを超え、symlink-file も使えない | ファイル名が 107 バイトを超え、symlink-file も使えない |

- bind の欄で symlink-file を数えているのは、bind が symlink を辿ると確かめた macOS と NetBSD だけ。Linux・FreeBSD・OpenBSD・illumos は辿らないので試さない。他の OS は辿るかを確かめていないので、欄は symlink-file を数えていない (辿れば届く)
- Linux でスレッド cwd が使えない時 (seccomp の filter 等)、bind はファイル名およそ 79 バイトまで proc、その先 96 バイトまでは fork-cwd
- D をどこにも作れない時、bind は D 無しの形でファイル名 88 バイト (Apple の OS / BSD は 84) まで届く (ソケットのディレクトリを開けて R を作れる時)。connect 系は connect の順のうち D を使わない経路 (proc-direct・relative・widened・symlink-file・at-direct) の分だけ届く (proc-held も使えない)
- `LastResort::Allow` の `bind_ex` の symlink-dir の綴りは D を通す経路の綴りより長いので、`bind` より長いファイル名には届かない。届くのは、D も D 無しの形も使えない時。綴りが 103 / 107 バイトに収まる置き場が無い時も `ENAMETOOLONG`
- Apple の OS の connect 系の 253 バイトは、カーネルが広げた sockaddr を受け付ける時。断った時は 103 バイト
- connect で symlink-file が使えないのは、安全で symlink を作れる置き場が無い時 (`TMPDIR` も `/tmp` も使えないサンドボックス等)。`/tmp` が使えれば長さは足りる
- PATH_MAX (macOS 1024、Linux 4096) を超えるパスは `ENAMETOOLONG`。ディレクトリを引く等でフルパスを渡すからで、sockat の責務外

### NUL を含むパス

NUL を含むパスは、長さによらず、何も作らずに `InvalidInput` を返す。カーネルはパスを最初の NUL までしか読まないので、通すと別の場所にソケットができうるからである。std も NUL を含むパスを `InvalidInput` で断るので、短いパスでは std と同じ種類のエラーになる (文言は sockat のもの)。

Linux / Android で先頭が NUL のパスは abstract namespace の名前でファイルではないので、sockat の対象外で、長さによらず std にそのまま渡す。std は `Path` の中の NUL をどれも断るので、結果は std と同じ `InvalidInput` になる。abstract namespace のソケットは、std の `SocketAddr::from_abstract_name` と `UnixListener::bind_addr` / `UnixStream::connect_addr` で扱う。

## 後始末と診断つきの _ex

`bind_ex` / `connect_ex` / `connect_nonblocking_ex` は、既定の関数と同じ経路で bind / connect し、どう届いたか (通った経路、飛ばした経路) と何を失ったかを返す。`bind_ex` は加えて、ソケットのファイルを後で消す口 `SocketFile` を返す。引数で選べるのは、何かを失ってでも届かせる最終手段を使うか (`LastResort`) の 1 つだけ。

| 関数 | `LastResort::Avoid` (既定) | `LastResort::Allow` の最終手段 | 戻り値 |
|---|---|---|---|
| `sockat::bind_ex(path, last_resort)` | `bind` と同じ順 | 最後に [symlink-dir](#symlink-dir) | `Result<ExListener, ExError>` |
| `sockat::connect_ex(path, last_resort)` | `connect` と同じ順 | 最後に [fork-cwd-direct](#fork-cwd-direct) | `Result<ExStream, ExError>` (ブロックする) |
| `sockat::connect_nonblocking_ex(path, last_resort)` | 同上 | 同上 | `Result<ExStream, ExError>` (非ブロッキング) |

```rust
use sockat::LastResort;

let bound = sockat::bind_ex(path, LastResort::Avoid)?;
// bound.listener を使う (tokio 等に渡してもよい)
bound.file.discard()?;
drop(bound.listener);
```

- `LastResort::Avoid` が `Default`。最終手段は、`bind` / `connect` がよほど珍しい環境で `ENAMETOOLONG` になり、それでも届かせたい時だけのもの。`LastResort::Allow` を渡すことが、失うものを受け入れる意思表示になる
- symlink-dir は `respelled` を失い、共有の置き場 (`/tmp` 等) では `recreatable-spelling` も失う。fork-cwd-direct は `forked` を失う (サーバに見える peer の pid が、もう終了した子の pid になる。[下](#fork-cwd-direct-で-connect-した時の副作用))。それぞれの中身と避け方は [失うもの](#失うもの)
- bind の fork-cwd と symlink-file は既定の経路で、最終手段ではない

戻り値は公開フィールドの struct で、`#[non_exhaustive]` (後でフィールドが足されうるので、分解には `..` が要る):

| フィールド | 中身 |
|---|---|
| `listener` (`ExListener`) / `stream` (`ExStream`) | std の `UnixListener` / `UnixStream` |
| `file` (`ExListener` だけ) | `SocketFile` ([下](#socketfile)) |
| `losses` | 失ったものの並び (`Vec<Loss>`。何も失わなければ空)。分岐に使う。`Loss` は `#[non_exhaustive]` の enum で、値は [失うもの](#失うもの) の表 |
| `route` | 通った経路の名前 (`&'static str`。例: `"thread-cwd"`、`"symlink-file"`、`"fork-cwd-direct"`) |
| `skipped` | 試して使えなかった経路の並び (`Vec<Skipped>`、試した順)。`Skipped` は `name` (経路の名前) と `reason` (使えなかった理由) を持つ |

- `route` と `skipped` は診断用で、中身を約束しない (経路は増えたり名前が変わったりする)
- `route` より前に試した経路はすべて使えなかったもの。長いパスでは最初が `standard`
- `skipped` に入るのは、その呼び出しで試す順の列 ([OS ごとに試す順番](#os-ごとに試す順番) の、その OS と状態の行。`LastResort::Allow` ならその最後に最終手段) にある経路だけ。例えば `/proc` を使えない Linux では、proc・proc-sockdir・proc-direct・proc-held は入らない

失敗した時は `ExError` を返す。`error` (`io::Error`)、`route`、`skipped` を持つ。

- `route` は実際に bind / connect して失敗した経路の名前。短いパスで std が失敗した時は `"standard"`。どの経路も使えなかった `ENAMETOOLONG` と、経路の前に断った時 (NUL を含む、名前で終わらない、ソケットのディレクトリが無い・ディレクトリでない・実パスを引けない) は `None`
- 経路の前に断った時は `skipped` も空。長さを判定するのはソケットのディレクトリを引いた後なので、`standard` もまだ試していない
- `From<ExError> for io::Error` で中の `io::Error` をそのまま返すので、`io::Result` を返す関数の中で `?` が使え、`raw_os_error()` で errno も取れる。`Display` は中の `io::Error` と同じ

```rust
use sockat::{ExStream, LastResort, Loss};

let ExStream { stream, losses, route, .. } = sockat::connect_ex(path, LastResort::Allow)?;
if losses.contains(&Loss::Forked) {
    eprintln!("{route} で connect した (fork した)");
}
```

`connect_ex` 系の fork-cwd-direct が要るのは、connect の順のどの経路も届かない時だけ:

- D を通す経路を connect に使えない: NetBSD / OpenBSD / illumos 等 (スレッド cwd が無い)、seccomp の filter が掛かり `/proc` の無い Linux、`pthread_fchdir_np` の無い Apple の OS
- D を通さない経路も届かない: 安全で symlink を作れる置き場が無く、相対パスにも (Apple の OS は広げた sockaddr にも) 収まらない

FreeBSD では at-direct がファイル名 103 バイトまで届くので、fork-cwd-direct まで来ることは無い。`connect_nonblocking_ex` は fork の前にソケットを非ブロッキングにするので、子の connect も待たない。

### SocketFile

- `bind_ex` は bind が通った直後、経路の中で listener を返す前に、ソケットのファイルの realpath と `(st_dev, st_ino)` を覚える (D を通す経路と D 無しの形では R が残っている間に、bind と同じ綴りで引く。symlink-file と symlink-dir ではソケットのディレクトリの realpath とファイル名で引く)。覚えられなければ listener を閉じ、ファイルは消さずにエラーを返す
- 相対パスでは、bind と realpath を引くことの前と両方の後でプロセスの cwd の `(st_dev, st_ino)` を比べ、違えば覚えられない時と同じに扱う (エラーは `Other`)。他のスレッドが間に cwd を変えると bind した所と覚える所が別の cwd の下になり、`discard()` が同じ名前の無関係なファイルを消しうるため。変えて戻した場合は見えない
- `SocketFile::discard()` は、その realpath にある物の `(st_dev, st_ino)` が同じ時だけ消す。別の物 (後から同じパスに bind された別のソケット、ファイル、symlink) は消さず、何も無ければ (途中のディレクトリが消された・ディレクトリでない物に置き換わった時も) 何もしない。どちらも `Ok(())`。それ以外の理由で確かめられない時 (`EACCES` 等) と消そうとして失敗した時はエラー (消せたか分からないのを `Ok(())` にしない)
- `discard()` は listener を閉じない。listener は std の `UnixListener` で、tokio 等へ渡せる。閉じるのは listener を持っている側。`discard()` は listener を閉じる前に呼ぶ。約束するのは「listener を開いている間に置き換えられた物は消さない」まで。listener を閉じた後に消して作り直された物は、ext4 や BSD の UFS / FFS では inode 番号が使い回されて見分けられず、消しうる (元のパスで消すのと同じ)
- `SocketFile` を drop しても何もしない
- 短いパス (std にそのまま渡す) でも同じ
- 限界: 確かめてから消すまでと、bind してから覚えるまでの間に、ソケットのディレクトリに書ける者がファイルをすり替えるのは防げない (inode を指定して消す手段が POSIX に無い)。消されたファイルの inode 番号が同じデバイスの新しいファイルに使い回されると、そのファイルを bind したものと見なす

### fork-cwd-direct で connect した時の副作用

**サーバに見える peer の pid は、もう終了したプロセス (fork した子) の pid になる。** ソケット自体は呼び出し元が持っていて、普通に使える。

- サーバに見える uid / gid は呼び出し元と同じ
- macOS (`LOCAL_PEERPID`) は、呼び出し元が send すると呼び出し元の pid に変わる。Linux (`SO_PEERCRED`) は終了した子の pid のまま変わらない
- サーバがその pid からプロセスの情報 (実行ファイル、ppid を辿って親を見る等) を引こうとしても、プロセスはもう無いので引けない
- 終了した pid の番号は、後で別のプロセスに再利用されうる。再利用されていれば、サーバは無関係なプロセスの情報を掴む
- なので、この pid は呼び出し元にもサーバにも使い道が無い。`connect_ex` は pid の数値は返さない
- NetBSD・OpenBSD・illumos での peer の資格情報の振る舞いは確かめていない

## 長いパスで std と違う点

### カーネルが返すアドレスは元のパスではない

`local_addr()` (listener と accept したストリーム) と `peer_addr()` (connect したストリーム) は、listener を bind した時にカーネルに渡した綴りを返す。connect した側がどの経路を通ったかは関係ない。

| bind の経路 | 返るもの |
|---|---|
| symlink-file | `<置き場>/sockat.XXXXXXXXXX` |
| thread-cwd、fork-cwd、at | `R/<ファイル名>` |
| proc | `/proc/self/fd/<fd>/R/<ファイル名>` |
| thread-cwd-sockdir、fork-cwd-sockdir、at-sockdir | `.sockat.XXXXXXXXXX/<ファイル名>` |
| proc-sockdir | `/proc/self/fd/<fd>/.sockat.XXXXXXXXXX/<ファイル名>` |
| symlink-dir (`LastResort::Allow` の `bind_ex`) | `<置き場>/sockat.XXXXXXXXXX/<ファイル名>` |

R (と `.sockat.XXXXXXXXXX`、置き場の symlink) は bind の後に消えているので、返ってきた綴りでファイルを消す・`stat` する・`chmod` しても、普段は何にも当たらない (fd の番号が使い回されても)。ただし綴りは他の利用者に見えうるので、作り直されると当たりうる ([respelled](#respelled)、共有の置き場の symlink-dir は [recreatable-spelling](#recreatable-spelling))。symlink-file の綴りで消しても、置き場の中の項目 (symlink の名前) が消えるだけ。ソケット自体にも当たらないので、元のパスは自分で持っておき、それを使う (消すなら `bind_ex` も使える。[使ってはいけない書き方](#使ってはいけない書き方))。同じ綴りは `Debug` の表示や、フレームワークのログ・接続元の情報にも出る。

illumos では、connect したストリームの `peer_addr()` が、listener を bind した綴りでなく、connect した時にカーネルに渡した綴りを返す (CI で観測。symlink-file で connect して `<置き場>/sockat.XXXXXXXXXX` が返った)。illumos の `getpeername` の振る舞いで、元のパスでない点は他の OS と同じ。

OpenBSD では、103 バイト以内の短いパス (sockat は std にそのまま渡す) でも、std の `local_addr()` がパスの末尾 1 バイトを欠いて返す (CI で観測)。sockat ではなく std / OS の振る舞いで、std の `UnixListener::bind` を直接使っても同じになる。

### bind できるファイル名は connect より短い

D を通す経路の綴りには R の 11 バイトが入るので、それで bind できるファイル名は Linux / illumos で 96 バイト、Apple の OS / BSD で 92 バイトまで。macOS と NetBSD の bind は symlink-file で、ファイル名の長さによらず届く。connect は R を挟まない経路 (proc-held、symlink-file 等) で、ふつうはファイル名の長さによらず届く。

### 名前で終わらないパスは InvalidInput

std にそのまま渡せない長さのパスで、最後がファイル名でないもの (`dir/`、`dir/.`、`dir/..`、`..`) は、何も作らずに `InvalidInput` を返す。ディレクトリとファイル名に分けられないからである。それより短いパスは std にそのまま渡すので、カーネルのエラーになる (例えば `dir/` の bind は macOS でも Linux でも `EADDRINUSE`)。

### fork の経路の、相手から見えない副作用

fork の経路 (fork-cwd・fork-cwd-sockdir・fork-cwd-direct) では、fork の性質として次が起きる。どれも相手 (サーバ / クライアント) からは見えない。

- 子は終了するまでプロセスの全 fd の複製を持つ。その瞬間に他のスレッドが閉じたソケットやパイプは、子が終わるまで開いたままになる (相手が EOF を見るのが少し遅れる。閉じた listener の backlog にさらに接続が入りうる)
- 呼び出し元のプロセスに `SIGCHLD` が届く
- `pthread_atfork` で登録された handler が走る
- 監視ツール (プロセスの一覧、監査ログ等) に短命の子が見える
- 他のスレッドの `waitpid(-1)` が子を刈り取りうる。その時はソケットの状態から結果を読む (失敗は `Other`)

### ディレクトリは呼ぶ少し前に引かれる

R と symlink-dir はソケットのディレクトリの realpath を、作った時に指す。proc-direct・relative・at-direct・fork-cwd-direct は、ディレクトリを先に引いて (開く、または実パスにする) からファイル名で connect する。その間に途中のディレクトリが rename されたり symlink が張り替えられたりすると、引いた時点のディレクトリにソケットができる (connect ならそこを見る)。widened と symlink-file は元のパスをそのままカーネルに解決させるので、この点は std と同じに振る舞う。

D をどこにも作れず、ソケットのディレクトリに R (`.sockat.XXXXXXXXXX`) を作って bind する時、そのディレクトリが sticky でなく他人も書けると、R を作ってから bind するまでに他人が R を差し替え、別のディレクトリに bind しうる (同じ名前が既にあれば `EADDRINUSE` で、上書きはしない)。

### relative は、他のスレッドの chdir に弱い

sockat 自身は cwd を変えない。相対パスを作る時に、それが同じディレクトリを指すことを確かめ、connect の後に cwd が変わっていないことも確かめる。他のスレッドが間に cwd を変えていたら、結果を捨てて (connect したソケットを閉じて) エラー (`Other`) を返す。変えて戻した場合は見えない。connect の間に cwd を変えるプログラムはそもそもプロセス中の全ての相対パスと競合していて、sockat はそれを安全にはせず、見える場合を検出するだけである。

### 残るものがある

呼び出しの途中でプロセスが死ぬと、`XDG_RUNTIME_DIR`・`TMPDIR`・`/tmp` に `sockat.XXXXXXXXXX` (D か symlink) が、ソケットのディレクトリに `.sockat.XXXXXXXXXX` (D か R) が残る。名前で sockat の残骸と分かる。

## 失うもの

経路を通ったことで、std と比べて何を失うか ([sockat の仕様の「失うもの」](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#失うもの))。`_ex` はこの名前を `losses` で返す。`bind` / `connect` / `connect_nonblocking` も、通った経路で同じものを失う (返さないだけ)。

| 名前 | `Loss` | 失うもの | 付く時 |
|---|---|---|---|
| [respelled](#respelled) | `Respelled` | カーネルが返すアドレスが元のパスでない | bind が standard 以外の経路を通った |
| [cwd-relative](#cwd-relative) | `CwdRelative` | カーネルが覚えた綴りが cwd からの相対パス | 相対パス (abstract の名前は除く) で bind が standard を通った |
| [forked](#forked) | `Forked` | fork の副作用 | fork-cwd・fork-cwd-sockdir・fork-cwd-direct を通った |
| [recreatable-spelling](#recreatable-spelling) | `RecreatableSpelling` | 綴りを他の利用者が作り直せる | `LastResort::Allow` の `bind_ex` が共有の置き場の symlink-dir を通った |

- `Loss` は `#[non_exhaustive]` の enum で、`as_str()` と `Display` は仕様の名前
- 名前は約束で、変えずに足すだけ。知らない名前は「何かを失った」とだけ読む
- 名前の間に大小や包含の関係は無い (`forked` と `recreatable-spelling` は互いを含まない)
- connect の綴りは相手に残らないので、connect で付くのは `forked` だけ
- 持ち主だけが書ける置き場の symlink-dir は `respelled` だけ。symlink-file の bind は置き場によらず `respelled` だけ

各節のコードは、crate doc に同じものがあり doctest で動かしている。

### respelled

- 起きること: bind した側の `local_addr()` (と、そこへ connect した側の `peer_addr()`) が元のパスでない綴りを返す ([カーネルが返すアドレスは元のパスではない](#カーネルが返すアドレスは元のパスではない))。listener を閉じてもソケットのファイルは消えない (std と同じ)
- 注意すること: カーネルが返す綴りでファイルを消す・`stat` しない。綴りの中の乱数の名前 R は消してあるので普段は何にも当たらないが、綴りは他の利用者に見えうる (Linux の `/proc/net/unix`)。プロセスの cwd が他人も書ける場所 (`/tmp` 等) だと、相手が見て作り直した後にその綴りで消すと、相手の選んだディレクトリの同じ名前のファイルを消しうる
- 避け方: 元のパスを持っておき、消す時はそれを使う。`bind_ex` の `SocketFile::discard()` を使う

```rust
use std::io;
use std::path::Path;

fn serve(path: &Path) -> io::Result<()> {
    let bound = sockat::bind_ex(path, sockat::LastResort::Avoid)?;
    // bound.listener で受ける (tokio 等へ渡してもよい)
    let removed = bound.file.discard(); // bind したファイルがまだそこにある時だけ消す
    drop(bound.listener);
    removed
}

fn serve_and_remove(path: &Path) -> io::Result<()> {
    let listener = sockat::bind(path)?;
    // listener で受ける
    drop(listener);
    std::fs::remove_file(path) // 元のパスで消す
}
```

### cwd-relative

- 起きること: 渡した相対パスが std にそのまま渡り (std が受け付ける長さの時)、カーネルが相対の綴りを覚える
- 注意すること: cwd を変えた後に、その綴り (や渡した相対パス) で消す・`stat` すると別の場所に当たる。sockat が経路で失わせたものでなく std が元から持つ危険だが、知らせる
- 避け方: bind の前にパスを絶対パスにする。消す時も絶対パスか `discard()` を使う

```rust
use std::io;
use std::path::Path;

fn serve(path: &Path) -> io::Result<()> {
    let path = std::path::absolute(path)?; // bind の前に絶対パスにする
    let bound = sockat::bind_ex(&path, sockat::LastResort::Avoid)?;
    // bound.listener で受ける
    let removed = bound.file.discard();
    drop(bound.listener);
    removed
}
```

### forked

- 起きること: fork した子が一瞬できる ([fork の経路の、相手から見えない副作用](#fork-の経路の相手から見えない副作用))。子は全 fd の複製を持ち、親に `SIGCHLD` が届き、`pthread_atfork` の handler が走る。`connect_ex` 系の最終手段ではサーバに見える peer の pid が子 ([fork-cwd-direct で connect した時の副作用](#fork-cwd-direct-で-connect-した時の副作用))
- 注意すること: `SIGCHLD` の handler や `waitpid(-1)` を使うプログラムは、子を刈り取ったり、結果を取り違えたりしうる。fd を閉じて相手に EOF を伝えるつもりのプログラムは、子が終わるまで伝わらない。最終手段を使う `connect_ex` 系の相手のサーバは、peer の pid での認可を当てにできない
- 避け方: 他のスレッドで `waitpid(-1)` を使わない (pid を指定して待つ)。connect では `LastResort::Allow` を渡さなければ fork しない。bind の fork-cwd は既定の経路なので、それで届く所では避けられない

```rust
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitStatus};

fn run_tool() -> io::Result<ExitStatus> {
    let mut child = Command::new("true").spawn()?;
    child.wait() // この子の pid を指定して待つ (waitpid(-1) を使わない)
}

fn dial(path: &Path) -> io::Result<UnixStream> {
    sockat::connect(path) // 最終手段を使わない、fork しない connect
}
```

### recreatable-spelling

- 起きること: カーネルが返す綴りが共有の置き場 (`/tmp` 等) を通る (`<置き場>/sockat.XXXXXXXXXX/<ファイル名>`)
- 注意すること: 名前は乱数なので事前には当てられないが、bind の後は他の利用者に見えうる (Linux の `/proc/net/unix`)。他の利用者がそれを作り直した後にその綴りで消すと、相手の選んだディレクトリの同じ名前のファイルを消しうる。名前の長さは防御でない (名前は bind の後に見える)
- 避け方: カーネルが返す綴りで消さず、元のパスか `discard()` で消す。持ち主だけが書ける置き場 (`$XDG_RUNTIME_DIR` や、持ち主だけが書ける `$TMPDIR`) を用意すれば、そちらが先に使われてこの名前は付かない

```rust
use std::io;
use std::path::Path;

fn serve(path: &Path) -> io::Result<()> {
    let bound = sockat::bind_ex(path, sockat::LastResort::Allow)?;
    // bound.listener で受ける
    let removed = bound.file.discard(); // local_addr() の綴りでなく、覚えた元のパスで消す
    drop(bound.listener);
    removed
}
```

## 使ってはいけない書き方

sockat を使う時に、無関係なファイルを消したり、ファイルを消し損ねたりする書き方 ([sockat の仕様の「使ってはいけない書き方」](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md#使ってはいけない書き方))。

| 書き方 | Rust での形 | なぜいけないか | 代わりに |
|---|---|---|---|
| カーネルが返す綴りでソケットのファイルを消す・`stat` する | `std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())` | 長いパスでは元のパスでない ([respelled](#respelled))。cwd や共有の置き場しだいで、作り直された別のファイルに当たりうる ([cwd-relative](#cwd-relative)、[recreatable-spelling](#recreatable-spelling)) | 元のパスを持っておいて `std::fs::remove_file(path)`<br>`bind_ex` の `SocketFile::discard()` |
| 相対パスで bind し、cwd を変えた後にその相対パスで消す | `sockat::bind("app.sock")` の後の<br>`std::env::set_current_dir(..)` と<br>`std::fs::remove_file("app.sock")` | 別の場所のファイルに当たる ([cwd-relative](#cwd-relative)) | bind の前に `std::path::absolute` で絶対パスにする<br>`discard()` |
| listener を閉じればソケットのファイルが消えると当てにする | `drop(listener)` だけ | ソケットのファイルは消えない (std と同じ) | `discard()`<br>元のパスで消す |
| `discard()` で listener も閉じると当てにする | `bound.file.discard()` だけ | `discard()` はファイルを消すだけ ([DR-0031](./decisions/DR-0031-ex-functions.md)) | listener は自分で閉じる (`drop(bound.listener)`) |
| `_ex` の `route`・`skipped`・`reason` の文字列で分岐する | `if bound.route == "symlink-dir"` | 診断用で、中身を約束しない (経路は増えたり変わったりする) | `losses` で分岐する (`bound.losses.contains(&sockat::Loss::RecreatableSpelling)`) |
| fork を通りうるプログラムで、他のスレッドが `waitpid(-1)` で子を刈る | `libc::waitpid(-1, ..)` | sockat の子を刈り取り、結果を取り違えうる ([forked](#forked)) | pid を指定して待つ (`std::process::Child::wait`)<br>connect では `LastResort::Allow` を渡さない |

次のコードは、表の書き方を並べたもの (どれもしてはいけない)。

```rust
use std::io;
use std::path::Path;

fn wrong(path: &Path) -> io::Result<()> {
    let listener = sockat::bind(path)?;
    // カーネルが返す綴りで消す: 長いパスでは元のパスでなく、別のファイルに当たりうる
    std::fs::remove_file(listener.local_addr()?.as_pathname().unwrap())?;
    // listener を閉じても、ソケットのファイルは消えない
    drop(listener);

    let bound = sockat::bind_ex(path, sockat::LastResort::Avoid)?;
    // discard() は listener を閉じない
    bound.file.discard()?;
    let _still_open = bound.listener;
    Ok(())
}

fn wrong_relative() -> io::Result<()> {
    let _listener = sockat::bind("app.sock")?;
    std::env::set_current_dir("/")?;
    // cwd を変えた後に相対パスで消す: 別の場所のファイルに当たる
    std::fs::remove_file("app.sock")
}

fn wrong_ex(path: &Path) -> io::Result<()> {
    let bound = sockat::bind_ex(path, sockat::LastResort::Allow)?;
    // route の文字列で分岐する: 中身を約束しない (losses で分岐する)
    if bound.route == "symlink-dir" {
        eprintln!("bound through a symlink");
    }
    Ok(())
}
```

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
| どの経路にも収まらない ([どこから届かなくなるか](#どこから届かなくなるか))。`connect` / `connect_nonblocking` と `LastResort::Avoid` の `connect_ex` 系では、fork-cwd-direct でしか届かない時も | `ENAMETOOLONG` |
| std にそのまま渡せない長さのパスで、名前で終わらない (`dir/`、`dir/.`、`..` 等) | `InvalidInput` |
| NUL を含むパス (長さによらない。Linux / Android で先頭が NUL のパスは std にそのまま渡す) | `InvalidInput` |
| relative の途中、または相対パスの `bind_ex` の途中で cwd が変わった | `Other` |
| fork した子が正常に終了しなかった。または子の終了状態を他で刈り取られ (`SIGCHLD` を `SIG_IGN` にしている、他のスレッドの `waitpid(-1)`)、ソケットが bind / connect されていなかった | `Other` |

エラーは `std::io::Error` (`_ex` は、それと試した経路を持つ `ExError`。`?` で `io::Error` に戻る) で、OS のエラーは `raw_os_error()` で errno を取れる。

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

`connect_nonblocking` は相手を待たないので、runtime の worker から直接呼べる。`bind` も相手を待たない (bind と listen はすぐ返る。thread-cwd ではスレッドの、fork の経路では子の終了を待つが、どちらも bind / connect してすぐ終わる)。`bind_ex` の listener も同じく渡せて、`SocketFile` は手元に残る。

## 確かめていないこと

- Android は compile もしていない。Linux と同じ経路を使う。Android のアプリは seccomp の filter の下で動くので、スレッド cwd は使われない見込み (確かめていない)
- NetBSD・OpenBSD・illumos・FreeBSD は CI の VM で試験を回している。fork-cwd-direct で connect した時の peer の資格情報 (pid) の振る舞いと、fork の経路で bind した時にクライアントに見えるサーバの peer は確かめていない
- ブロックしない connect が backlog の満杯で返すエラーと、connect が呼び出しの中で終わることは、Linux と macOS でだけ確かめた
- macOS 以外の Apple の OS (iOS 等) は macOS と同じ経路を使うが、確かめたのは iOS の clippy だけ。`pthread_fchdir_np` があるかも確かめていない (無ければ thread-cwd は使えないとして次へ進む)
