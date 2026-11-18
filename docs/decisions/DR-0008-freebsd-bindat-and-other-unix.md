# DR-0008: FreeBSD は bindat / connectat、dirfd 基準の primitive の無い Unix は相対パス → symlink → fork

- Status: Accepted
- Date: 2026-11-18

## Context

FreeBSD 10 以降には、ディレクトリの fd を基準にする `bindat(2)` / `connectat(2)` がある。NetBSD / OpenBSD / illumos などにはそれに当たる primitive が無い。相対パスと symlink の経路 ([DR-0004](DR-0004-relative-path-from-cwd.md)、[DR-0006](DR-0006-symlink-in-safe-temp-dir.md)) は POSIX の範囲の手 (`getcwd`、`realpath`、`stat`、`symlink`) だけで組める。

## Decision

- FreeBSD は `bindat` / `connectat` を使い、他の経路は試さない
  - libc crate に宣言が無いので、FreeBSD の `<sys/socket.h>` の宣言を自前で書く
  - ディレクトリを `O_RDONLY` で開くので、ディレクトリの読み取り権限も要る
  - ファイル名は 103 バイトまで。超えると `ENAMETOOLONG`
  - 初回の CI で落ちたら、fork に倒さずに原因を直す
- NetBSD / OpenBSD / illumos など、ディレクトリの fd を基準にする primitive が無い Unix は、相対パス → symlink → fork の順で試す ([DR-0001](DR-0001-long-socket-path-routes.md))。/proc の無い Linux と Apple の OS は、この順の fork の前にスレッド cwd の経路が入る ([DR-0003](DR-0003-linux-proc-self-fd.md)、[DR-0020](DR-0020-thread-cwd-route.md))
- NetBSD・OpenBSD・illumos は CI の VM で全部の試験を回す ([DR-0019](DR-0019-ci-targets.md))

## 理由

- fork より副作用の無い手がある OS では、そちらを使う
- `bindat` は Linux の /proc と同じく、fd を基準にファイル名で bind / connect でき、cwd も一時ファイルも fork も要らない
- 相対パスと symlink の経路は POSIX の範囲の手なので、どの Unix でも使える。fork に行く前にそれで届く

## 却下した案

| 案 | 却下の理由 |
|---|---|
| FreeBSD でも広げた sockaddr を使う | [DR-0005](DR-0005-widened-sockaddr-on-macos.md) の却下した案 |

## 判断の出所

- FreeBSD の扱い: 統括の裁定 2026-11-18
- 相対パスと symlink の経路を macOS 以外 (dirfd 基準の primitive の無い Unix、/proc の無い Linux) にも使う: kawaz 裁定 2026-11-18
- 上に無い細部: 出所の記録なし
