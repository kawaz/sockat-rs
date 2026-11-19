# DR-0027: 長いパスは bind が symlink を辿る OS ではソケットのファイルを指す symlink、他は自分専用のディレクトリ D の中の乱数の symlink R を通して bind し、connect は書き込みもスレッドも要らない経路を先に試す

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

### 経路の名前

経路はハブ SPEC の「経路の名前」で呼び、`route::Route` の variant と 1 対 1 にする (`Route::name`)。名前は `_ex` の戻り値 ([DR-0031](DR-0031-ex-functions.md)) と試験に出る。

| 名前 | `Route` | 中身 |
|---|---|---|
| standard | `Standard` | std にそのまま ([DR-0002](DR-0002-std-handles-paths-it-accepts.md)) |
| thread-cwd | `ThreadCwd` | 使い捨てのスレッドの cwd を D に移し `R/<ファイル名>` (Linux / Android、Apple の OS) |
| at | `At` | `bindat` / `connectat` に Dfd と `R/<ファイル名>` (FreeBSD) |
| proc | `Proc` | `/proc/self/fd/<Dfd>/R/<ファイル名>` (`/proc` を使える Linux / Android) |
| fork-cwd | `ForkCwd` | fork した子が `fchdir(Dfd)` して `R/<ファイル名>` で bind (FreeBSD 以外) |
| thread-cwd-sockdir / at-sockdir / proc-sockdir / fork-cwd-sockdir | `ThreadCwdSockdir` / `AtSockdir` / `ProcSockdir` / `ForkCwdSockdir` | 上の 4 つを D 無しの形で (bind だけ) |
| proc-direct | `ProcDirect` | `/proc/self/fd/<ソケットのディレクトリの fd>/<ファイル名>` (connect) |
| proc-held | `ProcHeld` | D の中のソケットのファイルへの symlink `s` を `/proc/self/fd/<Dfd>/s` で (connect) |
| relative | `Relative` | cwd からの相対パス (connect、[DR-0004](DR-0004-relative-path-from-cwd.md)) |
| widened | `Widened` / `WidenedRelative` | 広げた sockaddr に元のパス / 相対パス (connect、[DR-0005](DR-0005-widened-sockaddr-on-macos.md)) |
| symlink-file | `SymlinkFile` | 置き場のソケットのファイルを指す symlink。connect と、bind が symlink を辿る OS の bind ([DR-0006](DR-0006-symlink-in-safe-temp-dir.md)) |
| symlink-dir | `SymlinkDir { shared }` | 置き場のソケットのディレクトリを指す symlink。`bind_ex` の最終手段 ([DR-0031](DR-0031-ex-functions.md)) |
| at-direct | `AtDirect` | `connectat(<ソケットのディレクトリの fd>, <ファイル名>)` (FreeBSD、connect) |
| fork-cwd-direct | `ForkCwdDirect` | fork した子がソケットのディレクトリに `fchdir` してファイル名で connect (`connect_ex` 系の最終手段) |

- 名前は「手段 + 何についてか」(ハブ DR-0016)。接尾辞 `-sockdir` は R をソケットのディレクトリに置く形、`-direct` は R を通さずファイル名で渡す形

### 試す順番

| 順 | bind |
|---|---|
| 1 | standard (std に渡す長さ以下) |
| 2 | symlink-file (Apple の OS、NetBSD、未確認の OS) |
| 3 | thread-cwd (FreeBSD は at) |
| 4 | proc |
| 5 | fork-cwd |
| 6 | `ENAMETOOLONG` (最終手段を使う `bind_ex` は symlink-dir) |

- D をどこにも作れない bind は、3〜5 を D 無しの形で試す (thread-cwd-sockdir (FreeBSD は at-sockdir) → proc-sockdir → fork-cwd-sockdir)
- bind の symlink-file: 安全な置き場 (connect の symlink-file と同じ候補と条件、`Config::file_link_places`) にソケットのファイル (渡されたパス、相対パスなら cwd を前に付ける) を指す symlink `<置き場>/sockat.XXXXXXXXXX` を作り、その綴りで bind して、すぐ消す (`route::links::symlink_file`)。bind が最後の要素の symlink を辿る OS (macOS と NetBSD で実測) でだけ元のパスにソケットができる。辿らないと実測した Linux / Android、FreeBSD、OpenBSD、illumos では列に入れない (cfg で外す。FreeBSD は connect にも symlink-file が無いので、symlink-file の部品ごと外す)。他の OS では、bind が失敗した時に元のパスを `lstat` し、物があればそのエラーを返して次を試さず、無いか確かめられなければ辿らなかったとみなして「bind did not follow the symlink」で飛ばす (ハブ DR-0016。試験 `a_bind_through_symlink_file_reaches_any_file_name_length`、`a_bind_through_symlink_file_onto_an_existing_file_is_the_failure`、`a_bind_through_symlink_file_failing_with_nothing_at_the_path_goes_on`)。綴りにファイル名が出ないので、macOS と NetBSD の bind はファイル名の長さによらず届く。失うのは `respelled` だけ

| 順 | `connect` / `connect_nonblocking`、`LastResort::Avoid` の `connect_ex` 系 | `LastResort::Allow` の `connect_ex` 系 |
|---|---|---|
| 1 | standard (std に渡す長さ以下) | 同じ |
| 2 | OS ごとの順 (下の表) | 同じ |
| 3 | `ENAMETOOLONG` | fork-cwd-direct |

| OS | connect の順 |
|---|---|
| Linux / Android (`/proc` あり) | proc-direct → thread-cwd → proc → proc-held → symlink-file |
| Linux / Android (`/proc` 無し) | relative → thread-cwd → symlink-file |
| Apple の OS | relative → widened → thread-cwd → symlink-file |
| FreeBSD | at-direct → at |
| NetBSD / OpenBSD / illumos / その他 | relative → symlink-file |

- connect は、書き込み (D・R・symlink を作る) もスレッドも要らない経路を先に、D を作る経路とスレッドを立てる経路をその後に試す

- 最初にソケットのディレクトリを `stat` で引く。無ければ `ENOENT`、ディレクトリでなければ `ENOTDIR` (長さの判定や D を作るより前)
- 続けてソケットのディレクトリの realpath を取る。引けなければそのエラーを返す (R を作れないことを `ENAMETOOLONG` に化けさせない。試験 `an_unresolvable_socket_directory_reports_its_error_not_enametoolong`、ハブ SPEC ecb61c32)
- ソケットのディレクトリを開くのは、その fd が要る経路 (proc-direct、at-direct、fork-cwd-direct、`-sockdir` の経路、ソケットのディレクトリに作る D) だけ。開けなければその経路は使えない (`O_RDONLY` で開く OS は読み取り権限の無いディレクトリを開けない)。D を通す経路と proc-held は、D を他の置き場に作れればソケットのディレクトリを開かない (試験 `a_socket_directory_that_cannot_be_opened_costs_only_the_routes_that_need_its_fd`、`proc_and_proc_held_need_no_socket_directory_fd`)
- `/proc` を使えるかは、`/` を開いた fd で `/proc/self/fd/<fd>` を引いて確かめる (1 回の呼び出しで 1 度)。D の fd では、D を作る前に要る判断 (connect の順の選び方、proc と proc-held の綴りの見積もり) に間に合わない
- 経路が実際に bind / connect した結果の失敗は返し、次を試さない ([DR-0012](DR-0012-route-failure-handling.md))

### D を通す経路 (thread-cwd・at・proc・fork-cwd・proc-held)

D は 1 回の呼び出しで 1 度だけ作り、D を使う経路で共有する (`route::own_dir`、`route::Target` が持つ)。最初に D を求めた経路が、D と自分の要る物 (R、proc-held は `s`) を作れた置き場の D を使い、後の経路は同じ D に要る物を足す。どこにも作れなかったら、それを覚えて、後の D を使う経路はどれも理由 `no place for D` で飛ばし、作り直さない。D を使う経路より前の経路で届けば D は作らない。D と中身は呼び出しが返る前に片付ける (試験 `a_connect_by_a_route_that_needs_no_writing_makes_nothing`、`d_is_made_once_for_the_routes_through_it`、`with_d_made_nowhere_every_route_through_d_is_passed_over_and_d_tried_once`)

1. 置き場を `open_dir` で開き、`mkdirat` で mode 0700 の D を作る。名前は `sockat.` (ソケットのディレクトリでは `.sockat.`) + 英数 62 種 10 文字。`EEXIST` で引き直し 16 回で打ち切る
2. D を `O_DIRECTORY | O_NOFOLLOW` と、Linux は `O_PATH`、Apple の OS は `O_SEARCH` (`EISDIR` / `EINVAL` なら `O_RDONLY`)、他は `O_RDONLY` で開く。`fstat` で「ディレクトリ・持ち主が実効 uid・group と other に権限が無い」を確かめ、通った D だけを値 (`route::own::OwnDir`) にする。0700 にそろえる: `O_PATH` の fd は `/proc` があれば `chmod("/proc/self/fd/<fd>")`、無ければ名前で `O_RDONLY` で開き直して `(st_dev, st_ino)` を照合して `fchmod`、他は `fchmod`。そろえられなければ D を片付けて次の置き場へ
3. D の中に `symlinkat` で R を作る。先はソケットのディレクトリの realpath、名前は英数 10 文字 (引き直し 16 回)
4. D の fd を基準に `R/<ファイル名>` で、使える最初の経路で bind / connect する
5. R は自分が作った時だけ `unlinkat(Dfd, R)`。D は `fstatat(置き場の fd, 名前, AT_SYMLINK_NOFOLLOW)` の `(st_dev, st_ino)` が D の fd と一致する時だけ `unlinkat(.., AT_REMOVEDIR)`。成功でも失敗でも、値の drop でする

- 作った直後の D を開けない時 (umask が持ち主の権限を削った: Apple の OS の `O_SEARCH` は検索、他の OS の `O_RDONLY` は読み取りの権限が要る) は、`fstatat(置き場の fd, D の名前, AT_SYMLINK_NOFOLLOW)` が手順 2 と同じ条件なら `unlinkat(置き場の fd, D の名前, AT_REMOVEDIR)` で消し、次の置き場へ (`route::own::remove_unopened`、ハブ DR-0012 D12-Q5)。確かめてから消すまでにすり替えられても、`AT_REMOVEDIR` は空のディレクトリしか消さない。試験は umask 0177 / 0477 の子プロセス (Apple の OS と BSD で実際に開けない) と、D を開く関数を差し替えて失敗させる形 (`Config::open_own_dir`。Linux の `O_PATH` は開けてしまう)

| 経路 | OS | 使えない時 |
|---|---|---|
| thread-cwd: 使い捨てのスレッドで `unshare(CLONE_FS)` → `fchdir(Dfd)` (Linux)、`pthread_fchdir_np(Dfd)` (Apple) | Linux / Android、Apple の OS | seccomp の filter が掛かっている (`prctl(PR_GET_SECCOMP)` が 0 以外か失敗。filter が SIGSYS でプロセスを kill しうるので `unshare` を呼ばない)、`unshare` が断られた、`pthread_fchdir_np` が無い・失敗した、スレッドを作れない |
| proc: `/proc/self/fd/<Dfd>/R/<ファイル名>` | `/proc` を使える Linux / Android | `/proc/self/fd/<Dfd>` を引けない、綴りが 107 バイトを超える (ファイル名およそ 79 まで) |
| at: bindat / connectat に Dfd | FreeBSD | (無い) |
| fork-cwd: 子が `fchdir(Dfd)` して bind、listen は親 ([DR-0025](DR-0025-fork-bind-listen-by-parent.md)) | FreeBSD 以外 (bind だけ) | |

- 長さと、経路を使えるか (確かめられる所まで: thread-cwd は呼び出し元のスレッドの seccomp と `pthread_fchdir_np` の有無、proc と proc-held は見積もった綴り) は D を作る前に確かめ、使えなければ何も作らずに次へ
- proc と proc-held の綴りは D の fd の桁で変わる。D がまだ無ければ、`/proc` を確かめた時に `/` の fd が取った番号 + 1 (置き場を先に開く) を D の fd とみなして見積もり、D を開いた後に D の fd で引き直して長さを測り直す。収まらなければその経路を飛ばして次の経路へ (D は後の経路のために残す)
- D の置き場は `${TMPDIR:-/tmp}` (相対パスも可)、`/tmp` (同じ綴りなら飛ばす)、ソケットのディレクトリの順。安全も長さも問わない (D は fd で掴み、綴りに出ない)
- 置き場で「作れない」のは、開けない、D を作れない、名前を使い切った、乱数が取れない、開けない (消して次へ)、確認が通らない、0700 にそろえられない、最初に D を求めた経路の要る物 (R か `s`) を作れない時。次の置き場へ進む。D を作れた後に、後の経路の要る物を作れなければ、その経路だけを飛ばす

### D 無しの形 (`-sockdir`)

- どこにも D を作れない時、bind はソケットのディレクトリに R (`.sockat.XXXXXXXXXX`、ソケットのディレクトリの realpath を指す) を作り、D の fd の代わりにソケットのディレクトリの fd を基準に `.sockat.XXXXXXXXXX/<ファイル名>` で bind する。経路は D の時と同じ並び (thread-cwd-sockdir (FreeBSD は at-sockdir) → proc-sockdir `/proc/self/fd/<fd>/.sockat.XXXXXXXXXX/<ファイル名>` → fork-cwd-sockdir)。ソケットのディレクトリを開けない、綴りが収まらない、R も作れなければ `ENAMETOOLONG` (最終手段を使う `bind_ex` は symlink-dir へ。試験 `with_d_made_nowhere_a_bind_takes_the_sockdir_routes`)。connect は D 無しの形を使わず、connect の順の次の経路へ
- D 無しの形は D を通す経路と同じ並び (`route::bind_means` の結果) をそのまま使う (`route::without_own_dir`)。proc-sockdir は D の綴りで見積もった後、`.sockat.XXXXXXXXXX/<ファイル名>` とソケットのディレクトリの fd で測り直す (D 無しの綴りは 8 バイト長いので、D の見積もりで外れた proc が D 無しで収まることは無い)

### connect の経路

D を通す経路 (thread-cwd・proc・at) は bind と同じ手順で R を通す。他の経路は綴りにランダムな部品を挟まない (connect した側のソケットは bind されず、綴りが残らない)。

| 経路 | やり方 |
|---|---|
| proc-direct | `/proc/self/fd/<ソケットのディレクトリの fd>/<ファイル名>` (ファイル名およそ 90 まで)。`/proc/self/fd/<fd>` を引けなければ使えない |
| proc-held | D (上の「D を通す経路」の D、無ければ手順 1・2・5 で作る) の中に `symlinkat` でソケットのファイル (渡されたパス、相対パスなら cwd を前に付ける) を指す `s` を作り、`/proc/self/fd/<Dfd>/s` で connect |
| relative | [DR-0004](DR-0004-relative-path-from-cwd.md) |
| widened | [DR-0005](DR-0005-widened-sockaddr-on-macos.md)。カーネルが断ったら、fork-cwd-direct も広げた sockaddr を使わない |
| symlink-file | [DR-0006](DR-0006-symlink-in-safe-temp-dir.md) |
| at-direct | `connectat(ソケットのディレクトリの fd, <ファイル名>)` (103 バイトまで) |

### fork-cwd-direct (`connect_ex` 系の最終手段)

- 子がソケットのディレクトリの fd に `fchdir` してファイル名で connect する ([DR-0007](DR-0007-fork-child-fchdir.md))。Apple の OS で、カーネルが広げた sockaddr を断っていなければ、103 バイトを超えるファイル名は広げた sockaddr で渡す
- ソケットのディレクトリを開けなければ `ENAMETOOLONG`

### 実装の形

- 経路の順番は `route::run_long_with` から、bind は `route::bind_order`、connect は `route::connect_order` (OS ごとの経路の列 `connect_routes` を順に試す)。1 回の呼び出しの状態 (ソケットのディレクトリの fd、`/proc` を使えるか、realpath、D (どこにも作れなかったことも)、広げた sockaddr をまだ使えるか、飛ばした経路) は `route::Target` に必要になった時に引いて持つ
- 経路が使えないと判断した所で `Target::skip(名前, 理由)` を呼び、試した順に集める (`route::Reached::skipped`、[DR-0031](DR-0031-ex-functions.md))
- 環境を読む部分は `route::Config` に集め、試験から差し替える ([DR-0011](DR-0011-routes-not-exposed.md)): D の置き場 (`own_dir_places`。ソケットのディレクトリは `Place::SocketDir`)、symlink-file の置き場 (`file_link_places`)、bind で symlink-file を試すか (`symlink_file_bind`、bind の列に symlink-file がある OS だけ)、symlink-dir の置き場 (`dir_link_places`)、`/proc/self/fd` の場所、スレッド cwd を試すか、広げた sockaddr を試すか、ソケットのディレクトリの開き方 (`open_socket_dir`)、D の開き方 (`open_own_dir`)。開き方の 2 つは、試験が開けない状況の代わりに失敗する関数を置く
- bind は広げた sockaddr を使わない。広げた形で渡す関数 (`route::apple::wide_connect`) は connect の options しか受け取らない

### 長さの上限

| | Linux / illumos | Apple の OS / BSD |
|---|---|---|
| bind のファイル名 (`R/` の 11 バイト) | 96 | 92 |
| D 無しの bind のファイル名 (`.sockat.XXXXXXXXXX/` の 19 バイト) | 88 | 84 |
| proc-sockdir のファイル名 | およそ 72 (fd の桁で変わる) | |
| proc のファイル名 | およそ 79 (fd の桁で変わる) | |

## 理由

- 綴りの中に、自分しか知らず既に消した R があるので、後で fd の番号が使い回されても、cwd が何でも、綴りは何にも当たらない
- D を fd で掴み、作ってから開くまでのすり替えは `O_NOFOLLOW` と `fstat` で弾くので、置き場が安全でなくても深くても使える。名前を差し替えられても fd は変わらない
- 確認を `O_PATH` / `O_SEARCH` の fd でするので、umask が持ち主の読み取りを削っていても確かめられ、片付けられる。それでも開けない D は名前で同じ条件を確かめて空のディレクトリとして消すので、呼ぶたびに D が積もらない。`O_PATH` の fd には `fchmod` できない (`EBADF`) ので、`/proc` が無い時は名前で開き直して同じ物か照合する
- thread-cwd と proc は副作用が無い (peer の pid は呼び出し元のまま、fd の複製も無く、他のスレッドの cwd は変わらない)。`connect` も fork 無しで届く
- connect の綴りは残らないので、connect は R を挟まない経路で足りる。書き込みもスレッドも要らない経路 (proc-direct・relative・widened・at-direct) は速く、relative と widened は D を通す経路より遠くまで届く場面がある。D を作る経路とスレッドを立てる経路はその後に回し、前の経路で届けば D を作らない (kawaz 裁定)
- D を 1 度だけ作って共有するので、connect で D を使う経路が続いても (thread-cwd が断られた後の proc、proc が長すぎた後の proc-held) D を作り直さない。どこにも作れなかったことも覚えるので、同じ置き場を何度も試さない
- 呼び出し元のスレッドに `pthread_fchdir_np` して `-1` で戻すと、呼び出し元がそのスレッドに付けた cwd を消しうる。使い捨てのスレッドなら触らずに済む
- D を通す経路はソケットのディレクトリを開かず R で届くので、`O_RDONLY` で開く OS でも読み取り権限の無いディレクトリに通常の bind / connect と同じ権限で届く
- connect の綴りは残らないので、R を挟まない経路が今の綴りのままでも何にも当たらない。D を通さない分、届くファイル名が長い

## 却下した案

| 案 | 却下の理由 |
|---|---|
| bind の綴りにランダムな部品を挟まない (/proc、相対パス、ファイル名だけ) | 後で解決すると無関係なファイルに当たる。消すのは上のライブラリで、利用者が避けられない |
| 置き場の安全 (sticky 等) を確かめて名前で辿る、ソケットのディレクトリを指す symlink を置く形を既定の bind に使う | 他の利用者が綴りを作り直せる (`recreatable-spelling`)。D を fd で掴めば置き場の安全も長さも関係しない。最終手段 (symlink-dir) にだけ使う |
| Apple の OS の bind で広げた sockaddr を使う | アドレスが切れて CPython / Go の標準を壊す。fork の bind は相手から見える副作用が無い ([DR-0025](DR-0025-fork-bind-listen-by-parent.md)) |
| connect の R を挟まない経路も D を通す | connect の綴りは残らないので R を挟む理由が無く、届くファイル名が 11 バイト短くなるだけ |
| connect も D を通す経路を先に試す (bind と同じ並び) | 書き込みとスレッドが要る経路を、要らない経路で届く場面でも使うことになる。relative と widened が D を通す経路より遠くまで届く場面で、遠回りになる |
| D を使う経路ごとに D を作る | 同じ呼び出しで D を作っては消すことを繰り返し、どこにも作れない時に同じ置き場を何度も試す |
| thread-cwd を使わず fork だけにする | fork の副作用があり、`connect` は fork しないので届かない範囲が残る |
| 確認より前に読み取り権限を要する開き方 (`O_RDONLY`) で D を開く (Linux、Apple の OS) | umask が持ち主の読み取りを削ると D を確かめられず、呼ぶたびに残る |

## 悪い面

- D を通す bind のファイル名の上限が `R/` の 11 バイト分短い (Linux / illumos 96、Apple の OS / BSD 92)。symlink-file で届く macOS と NetBSD には効かない
- bind が symlink を辿るかを確かめたのは macOS・NetBSD (辿る) と Linux・FreeBSD・OpenBSD・illumos (辿らない) だけ (実測 2026-11-19、Linux と macOS は手元、他は sockat-py の CI の VM)。他の OS は実行時に見分ける (辿らないのに bind が `EADDRINUSE` 以外を返しても、元のパスに物が無ければ次の経路へ進む)
- 長いパスの bind のたびに D と R を作って消す。connect も書き込みの要らない経路で届かなければ同じ
- connect で D を使う経路が続く時、D は最後の経路が終わるまで残る (呼び出しの中だけ)
- 途中でプロセスが死ぬと D や R が残る (置き場に `sockat.XXXXXXXXXX`、ソケットのディレクトリに `.sockat.XXXXXXXXXX`)
- R はソケットのディレクトリの realpath を指すので、R を作った後のディレクトリの rename や symlink の張り替えは反映されない
- 開けない D を名前で確かめて消す時、確かめてから消すまでにすり替えられると、他人の空のディレクトリが 1 つ消えうる
- `pthread_fchdir_np` は libSystem の非公開 API で、将来消えうる (消えたら次の手段へ進む)
- 上のライブラリが元のパスのソケットのファイルを自動で消すことは効かない (消すのは `bind_ex` の `SocketFile::discard()`、[DR-0031](DR-0031-ex-functions.md))

## 未確定

- Android は compile もしていない。macOS 以外の Apple の OS に `pthread_fchdir_np` があるか、iOS で動くかは確かめていない
- FreeBSD の bindat / connectat に D の fd を渡す形、NetBSD・OpenBSD・illumos の振る舞いは CI の VM で確かめる。手元では compile (clippy) まで
- 使い捨てのスレッドを作るコストと、D と R を毎回作るコストは測っていない

## 判断の出所

- D を通す経路、D の置き場、bind の試す順番: kawaz 裁定 (ハブ DR-0012、2026-11-19)
- connect は書き込みもスレッドも要らない経路を先に試す (connect の順)、D は 1 回の呼び出しで 1 度だけ作り共有する: kawaz 裁定 (ハブ SPEC・DR-0012 12dc1577)
- 経路の名前: ハブ SPEC (ed04caa6)
- 作った直後に開けない D は名前で確かめて消す (D12-Q5): kawaz 裁定 2026-11-19 (ハブ DR-0012)
- D の開き方 (Linux は `/proc` の有無によらず `O_PATH`、Apple の OS は `O_SEARCH`)、ソケットのディレクトリを `stat` で引き fd が要る経路だけが開く: ハブの SPEC (2026-11-19)
- D をどこにも作れない時の bind も D 無しの形でスレッド cwd → /proc 版 → fork の順に試す (D12-Q1)、connect の fork-cwd-direct と FreeBSD の at-direct は D を通さない (D12-Q2): kawaz 裁定 2026-11-19 (ハブ DR-0012・SPEC 0412b609)
- `ENOTDIR`、`Config` の項目、`Route` の variant を経路の名前に 1 対 1 にする: 実装側の判断 2026-11-19
- bind が symlink を辿る OS で standard の次に symlink-file を試す、経路の名前を「手段 + 何についてか」にそろえる: kawaz 裁定 (ハブ DR-0016、2026-11-19)。辿らないと実測した OS (Linux / Android、FreeBSD、OpenBSD、illumos) では試さず、他の OS では失敗時に元のパスに物が無ければ次へ進む: 統括の判断 (ハブ DR-0016・SPEC bcc8d9b1)
