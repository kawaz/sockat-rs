# DR-0015: 自前のソケットとソケットファイルの扱いは std と同じにする

- Status: Accepted
- Date: 2026-11-18

## Context

ブロックしない connect、thread-cwd・thread-cwd-direct、bindat・connectat、広げた sockaddr、DGRAM の経路 ([DR-0032](DR-0032-resolve-through-persistent-base.md)、[DR-0005](DR-0005-widened-sockaddr-on-macos.md)) では、std を通さず sockat がソケットを作り、それを std の型にして返す。

## Decision

- 自前で作るソケットは std と同じ設定にする
  - close-on-exec
  - `SO_NOSIGPIPE`: std が付ける OS (Apple、FreeBSD、NetBSD、DragonFly) で付ける (閉じた相手への書き込みが SIGPIPE でなく `EPIPE` になる)
  - listen の backlog: std と同じ値 (macOS / Linux / FreeBSD / OpenBSD は -1、他は `SOMAXCONN`)
- ソケットファイルは listener を閉じても消さない (std と同じ)。消すのは利用者で、元のパスで `remove_file` するか、`bind_ex` の `SocketFile::remove` を使う

## 理由

- 戻り値は std の型で、std の代わりに置き換えて使う。自前のソケットだけ設定が違うと、経路によって振る舞いが変わる

## 判断の出所

- 出所の記録なし
