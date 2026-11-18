# DR-0027: 長いパスは自分専用のディレクトリ D の中の乱数の symlink R を通して bind / connect し、connect 系には予備の経路を残す

- Status: Accepted
- Date: 2026-11-19

## Context

- `sun_path` (NUL 込みで Linux / illumos 108 バイト、Apple の OS / BSD 104 バイト) を超えるパスは、std に渡せない。同じ場所を指す短い綴りでカーネルに渡す必要がある
- カーネルは bind に渡された綴りを覚えて `getsockname` / `getpeername` で返し、上のライブラリや利用者はその綴りで unlink / stat する (Python の asyncio の後始末、Go の `UnixListener.Close`、利用者の `remove_file(local_addr)`)。綴りは後で別の文脈で解決され直す

| bind の綴り | 後で解決すると |
|---|---|
| `/proc/self/fd/<n>/<ファイル名>` | 閉じた fd の番号が使い回され、別のディレクトリの同名ファイルに当たる (docker で実測、無関係なファイルを消した) |
| ファイル名だけ (スレッド cwd / fork / bindat) | プロセスの cwd の同名ファイルに当たる |
| cwd からの相対パス | cwd が変わると別の場所に当たる |
| 広げた sockaddr | アドレスが `sockaddr_un` に入らず途中で切れ、読む側 (CPython、Go の `net.FileConn`) を壊す |

- connect した側のソケットは bind されないので、connect に渡す綴りはどこにも残らない
- fork は副作用 (子が全 fd の複製を持つ、`SIGCHLD`、`pthread_atfork` 等) があり、最後の手にしたい
- 言語によらない仕様はハブの [SPEC](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md) と [ハブ DR-0012](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0012-own-directory-and-thread-cwd.md) (kawaz 裁定 2026-11-19)。この DR はその Rust の実装の判断

## Decision

### 試す順番

| 順 | bind | `connect` / `connect_nonblocking` | `connect_any` 系 |
|---|---|---|---|
| 1 | std に渡す長さ以下なら std にそのまま ([DR-0002](DR-0002-std-handles-paths-it-accepts.md)) | 同じ | 同じ |
| 2 | 中心の経路 (スレッド cwd → /proc 版 → fork。FreeBSD は bindat) | 中心の経路 (スレッド cwd → /proc 版。FreeBSD は connectat) | 同左 |
| 3 | `ENAMETOOLONG` | 予備の経路 | 予備の経路 |
| 4 | | `ENAMETOOLONG` | fork (ソケットのディレクトリの fd とファイル名) |

| OS | connect 系の予備の経路 |
|---|---|
| Linux / Android (`/proc` あり) | /proc → D の中のファイルへの symlink → symlink |
| Linux / Android (`/proc` 無し) | 相対パス → symlink |
| Apple の OS | 相対パス → 広げた sockaddr → symlink |
| FreeBSD | connectat |
| NetBSD / OpenBSD / illumos / その他 | 相対パス → symlink |

- 最初にソケットのディレクトリを `stat` で引く。無ければ `ENOENT`、ディレクトリでなければ `ENOTDIR` (長さの判定や D を作るより前)
- ソケットのディレクトリを開くのは、その fd が要る経路 (/proc、connectat、fork、ソケットのディレクトリに作る D と R) だけ。開けなければその経路は使えない (`O_RDONLY` で開く OS は読み取り権限の無いディレクトリを開けない)
- 経路が実際に bind / connect した結果の失敗は返し、次を試さない ([DR-0012](DR-0012-route-failure-handling.md))

### 中心の経路

1. 置き場を `open_dir` で開き、`mkdirat` で mode 0700 の D を作る。名前は `sockat.` (ソケットのディレクトリでは `.sockat.`) + 英数 62 種 10 文字。`EEXIST` で引き直し 16 回で打ち切る
2. D を `O_DIRECTORY | O_NOFOLLOW` と、Linux は `O_PATH`、Apple の OS は `O_SEARCH` (`EISDIR` / `EINVAL` なら `O_RDONLY`)、他は `O_RDONLY` で開く。`fstat` で「ディレクトリ・持ち主が実効 uid・group と other に権限が無い」を確かめ、通った D だけを値 (`route::own::OwnDir`) にする。0700 にそろえる: `O_PATH` の fd は `/proc` があれば `chmod("/proc/self/fd/<fd>")`、無ければ名前で `O_RDONLY` で開き直して `(st_dev, st_ino)` を照合して `fchmod`、他は `fchmod`。そろえられなければ D を片付けて次の置き場へ
3. D の中に `symlinkat` で R を作る。先はソケットのディレクトリの realpath、名前は英数 10 文字 (引き直し 16 回)
4. D の fd を基準に `R/<ファイル名>` で、使える最初の手段で bind / connect する
5. R は自分が作った時だけ `unlinkat(Dfd, R)`。D は `fstatat(置き場の fd, 名前, AT_SYMLINK_NOFOLLOW)` の `(st_dev, st_ino)` が D の fd と一致する時だけ `unlinkat(.., AT_REMOVEDIR)`。成功でも失敗でも、値の drop でする

| 手段 | OS | 使えない時 |
|---|---|---|
| スレッド cwd: 使い捨てのスレッドで `unshare(CLONE_FS)` → `fchdir(Dfd)` (Linux)、`pthread_fchdir_np(Dfd)` (Apple) | Linux / Android、Apple の OS | seccomp の filter が掛かっている (`prctl(PR_GET_SECCOMP)` が 0 以外か失敗。filter が SIGSYS でプロセスを kill しうるので `unshare` を呼ばない)、`unshare` が断られた、`pthread_fchdir_np` が無い・失敗した、スレッドを作れない |
| /proc 版: `/proc/self/fd/<Dfd>/R/<ファイル名>` | `/proc` を使える Linux / Android | `/proc/self/fd/<Dfd>` を引けない、綴りが 107 バイトを超える (ファイル名およそ 79 まで) |
| bindat / connectat に Dfd | FreeBSD | (無い) |
| fork: 子が `fchdir(Dfd)` して bind、listen は親 ([DR-0025](DR-0025-fork-bind-listen-by-parent.md)) | 全部 (bind だけ) | |

- 長さと、手段を使えるか (確かめられる所まで: スレッド cwd は呼び出し元のスレッドの seccomp と `pthread_fchdir_np` の有無、/proc 版はソケットのディレクトリの fd の桁で見積もった綴り) は D を作る前に確かめ、使える手段が無ければ何も作らない
- D の置き場は `${TMPDIR:-/tmp}` (相対パスも可)、`/tmp` (同じ綴りなら飛ばす)、ソケットのディレクトリの順。安全も長さも問わない (D は fd で掴み、綴りに出ない)
- 置き場で「作れない」のは、開けない、D を作れない、名前を使い切った、乱数が取れない、確認が通らない、0700 にそろえられない、R を作れない時。次の置き場へ進む
- どこにも D を作れない時、bind はソケットのディレクトリに R (`.sockat.XXXXXXXXXX`) を作り、fork の子がソケットのディレクトリの fd に `fchdir` して `.sockat.XXXXXXXXXX/<ファイル名>` で bind する。綴りが収まらないか R も作れなければ `ENAMETOOLONG`。connect 系は予備の経路へ
- D 無しで試す手段は `route::MEANS_WITHOUT_OWN_DIR` の 1 か所で決める (今は fork だけ)

### 予備の経路 (connect 系)

綴りにランダムな部品を挟まない (connect した側のソケットは bind されず、綴りが残らない)。

| 経路 | やり方 |
|---|---|
| /proc | `/proc/self/fd/<ソケットのディレクトリの fd>/<ファイル名>` (ファイル名およそ 90 まで) |
| D の中のファイルへの symlink | 中心の経路の手順 1・2・5 で D を作り、その中に `symlinkat` でソケットのファイル (渡されたパス、相対パスなら cwd を前に付ける) を指す `s` を作り、`/proc/self/fd/<Dfd>/s` で connect |
| 相対パス | [DR-0004](DR-0004-relative-path-from-cwd.md) |
| 広げた sockaddr | [DR-0005](DR-0005-widened-sockaddr-on-macos.md) |
| symlink | [DR-0006](DR-0006-symlink-in-safe-temp-dir.md) |
| connectat | `connectat(ソケットのディレクトリの fd, <ファイル名>)` (103 バイトまで) |

- 中心の経路で D をどこにも作れなかった時は、D の中のファイルへの symlink も試さない (同じ置き場で作れない)

### 最後の fork (`connect_any` 系)

- 子がソケットのディレクトリの fd に `fchdir` してファイル名で connect する ([DR-0007](DR-0007-fork-child-fchdir.md))。Apple の OS で、カーネルが広げた sockaddr を断っていなければ、103 バイトを超えるファイル名は広げた sockaddr で渡す
- ソケットのディレクトリを開けなければ `ENAMETOOLONG`

### 実装の形

- 経路の順番は `route::run_long_with`。1 回の呼び出しの状態 (ソケットのディレクトリの fd、`/proc` を使えるか、realpath、D をどこにも作れなかったか) は `route::Target` に必要になった時に引いて持つ
- 環境を読む部分は `route::Config` に集め、試験から差し替える ([DR-0011](DR-0011-routes-not-exposed.md)): D の置き場 (`own_dir_places`。ソケットのディレクトリは `Place::SocketDir`)、symlink の置き場 (`link_places`)、`/proc/self/fd` の場所、スレッド cwd を試すか、広げた sockaddr を試すか
- bind は広げた sockaddr を使わない。広げた形で渡す関数 (`route::apple::wide_connect`) は connect の options しか受け取らない

### 長さの上限

| | Linux / illumos | Apple の OS / BSD |
|---|---|---|
| bind のファイル名 (`R/` の 11 バイト) | 96 | 92 |
| D 無しの bind のファイル名 (`.sockat.XXXXXXXXXX/` の 19 バイト) | 88 | 84 |
| /proc 版のファイル名 | およそ 79 (fd の桁で変わる) | |

## 理由

- 綴りの中に、自分しか知らず既に消した R があるので、後で fd の番号が使い回されても、cwd が何でも、綴りは何にも当たらない
- D を fd で掴み、作ってから開くまでのすり替えは `O_NOFOLLOW` と `fstat` で弾くので、置き場が安全でなくても深くても使える。名前を差し替えられても fd は変わらない
- 確認を `O_PATH` / `O_SEARCH` の fd でするので、umask が持ち主の読み取りを削っていても確かめられ、片付けられる (確認できない D は消せないので、確認の前に読み取り権限を要すると呼ぶたびに D が残る)。`O_PATH` の fd には `fchmod` できない (`EBADF`) ので、`/proc` が無い時は名前で開き直して同じ物か照合する
- スレッド cwd と /proc 版は副作用が無い (peer の pid は呼び出し元のまま、fd の複製も無く、他のスレッドの cwd は変わらない)。`connect` も fork 無しで届く
- 呼び出し元のスレッドに `pthread_fchdir_np` して `-1` で戻すと、呼び出し元がそのスレッドに付けた cwd を消しうる。使い捨てのスレッドなら触らずに済む
- 中心の経路はソケットのディレクトリを開かず R で届くので、`O_RDONLY` で開く OS でも読み取り権限の無いディレクトリに通常の bind / connect と同じ権限で届く
- connect の綴りは残らないので、予備の経路が今の綴りのままでも何にも当たらない。D を通さない分、届くファイル名が長い

## 却下した案

| 案 | 却下の理由 |
|---|---|
| bind の綴りにランダムな部品を挟まない (/proc、相対パス、ファイル名だけ) | 後で解決すると無関係なファイルに当たる。消すのは上のライブラリで、利用者が避けられない |
| 置き場の安全 (sticky 等) を確かめて名前で辿る symlink を置く形を bind に使う | D を fd で掴めば置き場の安全も長さも関係しない。処理と試験が増えるだけ |
| Apple の OS の bind で広げた sockaddr を使う | アドレスが切れて CPython / Go の標準を壊す。fork の bind は相手から見える副作用が無い ([DR-0025](DR-0025-fork-bind-listen-by-parent.md)) |
| connect 系の予備の経路も D を通す | connect の綴りは残らないので R を挟む理由が無く、届くファイル名が 11 バイト短くなるだけ |
| スレッド cwd を使わず fork だけにする | fork の副作用があり、`connect` は fork しないので届かない範囲が残る |
| 確認より前に読み取り権限を要する開き方 (`O_RDONLY`) で D を開く (Linux、Apple の OS) | umask が持ち主の読み取りを削ると D を確かめられず、呼ぶたびに残る |

## 悪い面

- bind のファイル名の上限が `R/` の 11 バイト分短い (Linux / illumos 96、Apple の OS / BSD 92)
- 長いパスの bind / connect のたびに D と R を作って消す
- 途中でプロセスが死ぬと D や R が残る (置き場に `sockat.XXXXXXXXXX`、ソケットのディレクトリに `.sockat.XXXXXXXXXX`)
- R はソケットのディレクトリの realpath を指すので、R を作った後のディレクトリの rename や symlink の張り替えは反映されない
- D を `O_RDONLY` で開く OS (Linux と Apple の OS 以外) では、umask が持ち主の読み取りを削っていると D を開けず、確かめられないので残る
- `pthread_fchdir_np` は libSystem の非公開 API で、将来消えうる (消えたら次の手段へ進む)
- 上のライブラリが元のパスのソケットのファイルを自動で消すことは効かない (消すのは `bind_ex` の `discard()`、[DR-0028](DR-0028-bind-ex-and-socket-file.md))

## 未確定

- D をどこにも作れない時の bind で、fork の前にスレッド cwd と /proc 版を D 無しで試すか (ハブ QUESTIONS.md D12-Q1)。今は fork だけ
- `connect_any` 系の fork と FreeBSD の connect の予備は D を通さない (ハブ D12-Q2)。今は通さない
- 作った直後の D を開けない時 (umask が持ち主の検索・読み取りを削った)、名前で確かめて空なら消すか (ハブ D12-Q5)。今は残す
- Android は compile もしていない。macOS 以外の Apple の OS に `pthread_fchdir_np` があるか、iOS で動くかは確かめていない
- FreeBSD の bindat / connectat に D の fd を渡す形、NetBSD・OpenBSD・illumos の振る舞いは CI の VM で確かめる。手元では compile (clippy) まで
- 使い捨てのスレッドを作るコストと、D と R を毎回作るコストは測っていない

## 判断の出所

- 中心の経路、D の置き場、試す順番、予備の経路: kawaz 裁定 (ハブ DR-0012、2026-11-19)
- D の開き方 (Linux は `/proc` の有無によらず `O_PATH`、Apple の OS は `O_SEARCH`)、ソケットのディレクトリを `stat` で引き fd が要る経路だけが開く: ハブの SPEC (2026-11-19)
- `ENOTDIR`、D 無しで試す手段を 1 か所で決める形、`Config` の項目: 実装側の判断 2026-11-19
