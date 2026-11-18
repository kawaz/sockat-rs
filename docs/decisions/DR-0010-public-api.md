# DR-0010: 公開 API は std の型を返す同期関数。connect は「fork するか」「ブロックするか」で 4 つ

- Status: Accepted
- Date: 2026-11-18

## Context

std の `bind` / `connect` を置き換えて使えることが基本。fork の経路 ([DR-0007](DR-0007-fork-child-fchdir.md)) で connect すると、サーバに見える peer の pid が終了した子の pid になる。async ランタイムからは待たない connect が要る。

## Decision

| 関数 | 戻り値 | fork の経路 | ブロック |
|---|---|---|---|
| `sockat::bind(path)` | `io::Result<UnixListener>` | 最後に試す | (待たない) |
| `sockat::bind_ex(path)` | `io::Result<(UnixListener, SocketFile)>` ([DR-0028](DR-0028-bind-ex-and-socket-file.md)) | 最後に試す | (待たない) |
| `sockat::bind_any(path)` | `Result<AnyListener, AnyError>` ([DR-0029](DR-0029-any-functions.md)) | 最後に試す (その後に置き場の symlink) | (待たない) |
| `sockat::connect(path)` | `io::Result<UnixStream>` | 試さない (要る時は `ENAMETOOLONG`) | する |
| `sockat::connect_nonblocking(path)` | `io::Result<UnixStream>` | 試さない | しない |
| `sockat::connect_any(path)` | `Result<AnyConnection, AnyError>` | 最後に試す | する |
| `sockat::connect_any_nonblocking(path)` | `Result<AnyConnection, AnyError>` | 最後に試す | しない |

- `bind` と `connect` は std の `bind` / `connect` と同じ形で、置き換えるだけで使える
- `bind_any` と `connect_any` 系 (any 系) は feature `any` を有効にした時だけ公開する。既定では無効 ([DR-0029](DR-0029-any-functions.md))
- **`connect` は fork の経路を試さない。** D を通す経路と予備の経路 ([DR-0027](DR-0027-own-directory-and-fallback-routes.md)) までは試し、fork が要る場面では `ENAMETOOLONG` を返す ([DR-0012](DR-0012-route-failure-handling.md))
- `connect_any` 系は、`connect` でも届かないよほど珍しい環境で、fork の副作用を受け入れてでもつなぎたい時だけのもの。最後は fork してでもやり切る (fork-direct)。その環境は、D を通す経路を connect に使えず、予備の経路も届かない時
  - D を通す経路を connect に使えない: スレッド cwd の無い OS (NetBSD・OpenBSD・illumos 等)、seccomp の filter が掛かり `/proc` の無い Linux (docker の既定、systemd の `SystemCallFilter=` 等で `/proc` を外した時)、`pthread_fchdir_np` の無い Apple の OS
  - 予備の経路も届かない: 安全で symlink を作れる一時ディレクトリが無く、相対パスにも (Apple の OS は広げた sockaddr にも) 収まらない
- any 系は std の型と、失ったものの並び (`losses`)、通った経路の名前 (`route`)、試して使えなかった経路の並び (`skipped`) を `#[non_exhaustive]` の struct で返す。失敗は `AnyError` (`io::Error` に戻せる) ([DR-0029](DR-0029-any-functions.md))。pid の数値は返さない
- ブロックしない 2 つは、ソケットを connect の前に `O_NONBLOCK` にし、返す stream も非ブロッキングのまま (tokio の `from_std` にそのまま渡せる)。相手の backlog が満杯なら Linux は待たず `EAGAIN` を返す (tokio の connect と同じ)
- `bind` は最後は fork でやり切る
- 軸は「fork するか」と「ブロックするか」の 2 つで、設定の型でなく関数で表す
- 後始末つきの bind `bind_ex` は、listener と、ソケットのファイルを後で消す口 `SocketFile` を返す ([DR-0028](DR-0028-bind-ex-and-socket-file.md))

## 理由

- fork は connect に普通求められる動きではない例外で、サーバに見える peer の pid がもう終了したプロセスの pid になる。普通の環境では D を通す経路 (Linux と Apple の OS の thread-cwd、Linux の proc、FreeBSD の at) か予備の経路 (`/tmp` の symlink 等) で副作用無しで届く
- `losses` は、fork の副作用などを受け入れたことを利用者が知り、警告するなどに使えるようにするため。`route` と `skipped` は、どの経路で届いたか・なぜその経路に落ちたかを知り、ログに残すなどに使えるようにするため ([DR-0029](DR-0029-any-functions.md))
- fork の経路で connect した時にサーバに見える peer の pid は、もう終了したプロセスの pid で、呼び出し元にもサーバにも使い道が無い (サーバはその pid から呼び出し元のプロセスを辿れない)。fork が起き得るかは、`connect` と `connect_any` のどちらを呼ぶかで利用者が選んでいる
- Unix domain の connect は呼び出しの中で終わるか失敗し、Linux も XNU も `EINPROGRESS` を返さない (確かめた範囲は `route::ConnectOptions` の doc)。なのでブロックしない connect に後で待つものは無い
- `bind` の fork は子が bind するだけで listen は親がするので、listener に相手から見える fork の副作用は残らない ([DR-0025](DR-0025-fork-bind-listen-by-parent.md))
- 軸は 2 つで閉じている (タイムアウトはブロックしない connect で足り、bind は待たない)。関数 4 つで足り、名前で振る舞いが読める

## 却下した案

| 案 | 却下の理由 |
|---|---|
| `connect` も最後は fork する | fork は connect に普通求められる動きではなく、サーバに見える peer の pid がもう終了したプロセスの pid になる。普通の環境では D を通す経路か予備の経路で届くので、fork が要る場面は `connect_any` 系を選んだ利用者だけにする |
| `connect_any` 系が fork した子の pid を返す | その pid はもう終了したプロセスのもので、呼び出し元にもサーバにも使い道が無い。利用者に要るのは fork したか (`losses` の `forked`) だけ |
| 設定の型 (ブロックするか、fork するか) を持つ connect を 1 つ公開する | 軸は 2 つで閉じていて、関数 4 つで足りる。名前で振る舞いが読める |
| backlog を指定できる bind | 長いパスでも std と同じに使えることが責務で、std の bind にも backlog の指定は無い。要る人は listen し直せばよい (Linux / macOS で効く) |
| fork しない bind を設ける | [DR-0007](DR-0007-fork-child-fchdir.md) の却下した案 |

## 判断の出所

- 公開する connect を「fork するか」「ブロックするか」の 4 つの関数にし、fork するのは `connect_any` 系だけにする: kawaz 裁定 2026-11-18
- `connect_any` 系は pid を返さない: kawaz 裁定 2026-11-18
- any 系の戻り値の形: [DR-0029](DR-0029-any-functions.md)
- 上に無い細部: 出所の記録なし
