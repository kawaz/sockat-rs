# DR-0015: 自前のソケットとソケットファイルの扱いは std と同じにする

- Status: Accepted
- Date: 2026-11-18

## Context

ブロックしない connect ([DR-0010](DR-0010-public-api.md))、スレッド cwd・bindat ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))、広げた sockaddr ([DR-0005](DR-0005-widened-sockaddr-on-macos.md))、fork ([DR-0007](DR-0007-fork-child-fchdir.md)) の経路では、std を通さず sockat がソケットを作り、それを std の型にして返す。

## Decision

- 自前で作るソケットは std と同じ設定にする
  - close-on-exec
  - `SO_NOSIGPIPE`: std が付ける OS (Apple、FreeBSD、NetBSD、DragonFly) で付ける (閉じた相手への書き込みが SIGPIPE でなく `EPIPE` になる)
  - listen の backlog: std と同じ値 (macOS / Linux / FreeBSD / OpenBSD は -1、他は `SOMAXCONN`)
- ソケットファイルは listener を閉じても消さない (std と同じ)。消すのは利用者で、元のパスで `remove_file` するか、`bind_ex` の `SocketFile::discard` を使う ([DR-0028](DR-0028-bind-ex-and-socket-file.md))。unlink には `sun_path` の上限が無い

## 理由

- 理由の記録なし

## 判断の出所

- 出所の記録なし
