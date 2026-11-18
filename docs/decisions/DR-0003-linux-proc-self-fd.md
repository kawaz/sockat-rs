# DR-0003: Linux は `/proc/self/fd/<dirfd>/<name>`、使えない時だけ他の Unix と同じ順

- Status: Accepted
- Date: 2026-11-18

## Context

Linux には、ディレクトリの fd を基準にファイル名で bind / connect する専用のシステムコールが無い。一方で `/proc/self/fd/<n>` はカーネルがその fd の指すディレクトリとして解決する。/proc を mount していない chroot や最小のコンテナもある。

## Decision

- ソケットを置くディレクトリを `O_PATH | O_DIRECTORY` で開き、`/proc/self/fd/<その fd>/<ファイル名>` を std に渡す。ソケットはそのディレクトリにできる (connect ならそこから引く)
- この綴りが使える時は、これ以外を試さない
- 次の時は /proc の経路は使えないとみなし、相対パス → symlink → スレッド cwd → fork の順に進む ([DR-0001](DR-0001-long-socket-path-routes.md)。dirfd 基準の primitive の無い Unix の順に、スレッド cwd の経路 ([DR-0020](DR-0020-thread-cwd-route.md)) を足したもの)
  - /proc が無い: ディレクトリを開いた後に `/proc/self/fd/<n>` を stat し、引けない
  - ファイル名が /proc の綴りに収まらない: `/proc/self/fd/<n>/<ファイル名>` が `sun_path` (107 バイト + NUL) を超える (ファイル名がおよそ 90 バイトを超える時)
- /proc の経路で実際に bind / connect した結果のエラーはそのまま返し、他の経路でやり直さない ([DR-0012](DR-0012-route-failure-handling.md))
- /proc が使えずスレッド cwd か fork の経路まで進んだ時は、/proc の経路のために開いた `O_PATH` の fd にそのスレッドか子が `fchdir` する。ファイル名の上限はその経路のもの (標準の `sun_path` に入ること) で、/proc の綴りの上限は当てない ([DR-0020](DR-0020-thread-cwd-route.md)、[DR-0007](DR-0007-fork-child-fchdir.md))
  - connect は先にファイルを指す symlink ([DR-0006](DR-0006-symlink-in-safe-temp-dir.md)) で届く。bind と、symlink の置き場の無い connect は、スレッド cwd の経路で 107 バイトのファイル名まで届く。それが使えない時 (`unshare` を断る seccomp) は、bind と `connect_any` 系は fork で届く
- fork の副作用は、/proc の経路もスレッド cwd の経路も使えない時に長いパスを bind する時と、`connect_any` 系で fork の経路まで進んだ時だけ乗る。connect では **サーバに見える peer の pid (`SO_PEERCRED`) は、もう終了した子の pid になり、呼び出し元が send しても変わらない** (試験 `without_proc_the_server_sees_the_forked_child_as_the_peer`)
- Android も同じ扱いにする

## 理由

- /proc の綴りはディレクトリ部分の長さに上限が無く、一時ファイルも fork も要らず、cwd も変えない
- fork を使わない。Linux の `SO_PEERCRED` は connect した時点の pid を記録し、子が終了した後もその pid のままになる (試験 `so_peercred_keeps_the_pid_of_a_forked_connector` で実機確認)。サーバが peer の pid を見る用途を壊す
- /proc が無い時は他の Unix と同じ順 (と、Linux にあるスレッド cwd の経路) に倒し、副作用の無い経路で届くならそれを使う
- /proc を確かめずに bind / connect すると、/proc が無いだけなのに「ディレクトリが無い」と読める `ENOENT` になる。なので stat で先に確かめる

## 却下した案

| 案 | 却下の理由 |
|---|---|
| /proc の綴りが使える Linux でも相対パス・symlink・fork を試す | /proc の経路が長さの制約無しで通るので要らない。fork は `SO_PEERCRED` の pid を壊す |

## 未確定

- Android で同じ扱いが通るかは確かめていない (compile もしていない)

## 判断の出所

- /proc の経路: kawaz 指示 2026-11-18
- /proc が無い時に他の Unix と同じ順に倒す: kawaz 裁定 2026-11-18
- /proc の綴りに収まらないファイル名を同じ順に回す: kawaz 承認 2026-11-18
- 上に無い細部: 出所の記録なし
