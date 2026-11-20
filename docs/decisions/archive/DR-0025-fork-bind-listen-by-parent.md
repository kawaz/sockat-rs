# DR-0025: fork の経路の bind は listen を必ず親がする

- Status: Superseded by DR-0032
- Date: 2026-11-18

## Context

- fork の経路 ([DR-0007](DR-0007-fork-child-fchdir.md)) の bind に、相手 (connect するクライアント) から見える副作用があるかで、fork を避けるために bind に別の綴り (Apple の OS の広げた sockaddr 等) が要るかが決まる ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))
- クライアントが自分のソケットに問う peer の資格情報 (macOS の `LOCAL_PEERPID`、Linux の `SO_PEERCRED`) は、サーバ側で listen したプロセスを指す
- 実測 (ハブ [DR-0010](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0010-fork-bind-listen-by-parent.md)、macOS 26.5.2 と Linux の docker)

| 手順 | クライアントに見えるサーバの peer |
|---|---|
| 子は `fchdir` + bind + `_exit`、listen は親 | send の前から親 |
| 子に listen までさせる (対照) | 死んだ子。macOS は send の前だけ、Linux はずっと |

## Decision

- fork の経路の bind の後の listen は、必ず呼び出し元 (親) がする (`route::fork_in`)。子は `fchdir` と bind だけをして `_exit` する
- 副作用は範囲つきで書く

| 範囲 | 副作用 |
|---|---|
| 相手から見える | 無い |
| 相手から見えない | 子が終了まで全 fd の複製を持つ<br>親に `SIGCHLD` が届く<br>`pthread_atfork` の handler が走る<br>監視ツールに短命の子が見える<br>他のスレッドの `waitpid(-1)` が子を刈り取りうる (その時はソケットの状態から結果を読む、[DR-0007](DR-0007-fork-child-fchdir.md)) |

## 理由

- listen した側が peer の資格情報の持ち主になる。子に listen させると、クライアントは死んだプロセスを相手と見る
- sockat-rs でも確かめた (2026-11-18、macOS 26.5 と Linux の docker): 試験 `a_client_sees_this_process_as_the_server_of_a_bind_by_fork` は、クライアント側の `LOCAL_PEERPID` / `SO_PEERCRED` が accept の前、送受信の前、送受信の後のどれも親の pid であることを確かめる。子に listen させ親の listen を外すと、どちらの OS でも accept の前の時点で子の pid が見えて落ちる

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 子が listen までする | クライアントに見えるサーバの peer が死んだ子になる |

## 未確定

- NetBSD・OpenBSD・illumos で、クライアントに見えるサーバの peer の資格情報は確かめていない (試験は Linux と Apple の OS だけ)
- macOS の `LOCAL_PEEREPID` / audit token、Linux の `SO_PEERPIDFD` は sockat-rs の試験では見ていない (ハブ DR-0010 の実測だけ)

## 判断の出所

- fork の経路の bind の listen は親がする、副作用を「相手から見える / 見えない」で分けて書く: kawaz 裁定 (ハブ DR-0010、2026-11-18)
