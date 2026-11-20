# DR-0010: 公開 API は std の型を返す同期関数。connect は「fork するか」「ブロックするか」で 4 つ

- Status: Superseded by DR-0032
- Date: 2026-11-18

## Context

std の `bind` / `connect` を置き換えて使えることが基本。fork の経路 ([DR-0007](DR-0007-fork-child-fchdir.md)) で connect すると、サーバに見える peer の pid が終了した子の pid になる。async ランタイムからは待たない connect が要る。

## Decision

| 関数 | 戻り値 | fork の経路 | ブロック |
|---|---|---|---|
| `sockat::bind(path)` | `io::Result<UnixListener>` | 試す (fork-cwd) | (待たない) |
| `sockat::connect(path)` | `io::Result<UnixStream>` | 試さない (要る時は `ENAMETOOLONG`) | する |
| `sockat::connect_nonblocking(path)` | `io::Result<UnixStream>` | 試さない | しない |
| `sockat::bind_ex(path, last_resort)` | `Result<ExListener, ExError>` ([DR-0031](DR-0031-ex-functions.md)) | 試す (fork-cwd。`LastResort::Allow` なら最後に置き場の symlink-dir) | (待たない) |
| `sockat::connect_ex(path, last_resort)` | `Result<ExStream, ExError>` | `LastResort::Allow` の時だけ最後に試す (fork-cwd-direct) | する |
| `sockat::connect_nonblocking_ex(path, last_resort)` | `Result<ExStream, ExError>` | 同上 | しない |

- `bind` と `connect` は std の `bind` / `connect` と同じ形で、置き換えるだけで使える
- **`connect` は fork の経路を試さない。** connect の順 ([DR-0027](DR-0027-own-directory-and-fallback-routes.md)) の経路までは試し、fork が要る場面では `ENAMETOOLONG` を返す ([DR-0012](DR-0012-route-failure-handling.md))
- `LastResort::Allow` の `connect_ex` 系は、`connect` でも届かないよほど珍しい環境で、fork の副作用を受け入れてでもつなぎたい時だけのもの。最後は fork してでもやり切る (fork-cwd-direct)。その環境は、connect の順のどの経路も届かない時
  - D を通す経路を connect に使えない: スレッド cwd の無い OS (NetBSD・OpenBSD・illumos 等)、seccomp の filter が掛かり `/proc` の無い Linux (docker の既定、systemd の `SystemCallFilter=` 等で `/proc` を外した時)、`pthread_fchdir_np` の無い Apple の OS
  - D を通さない経路も届かない: 安全で symlink を作れる一時ディレクトリが無く、相対パスにも (Apple の OS は広げた sockaddr にも) 収まらない
- `_ex` は std の型と、失ったものの並び (`losses`)、通った経路の名前 (`route`)、試して使えなかった経路の並び (`skipped`) を `#[non_exhaustive]` の struct で返す。`bind_ex` は加えて、ソケットのファイルを後で消す口 `SocketFile` を返す。失敗は `ExError` (`io::Error` に戻せる) ([DR-0031](DR-0031-ex-functions.md))。pid の数値は返さない
- ブロックしない 2 つは、ソケットを connect の前に `O_NONBLOCK` にし、返す stream も非ブロッキングのまま (tokio の `from_std` にそのまま渡せる)。相手の backlog が満杯なら Linux は待たず `EAGAIN` を返す (tokio の connect と同じ)
- `bind` は fork-cwd でやり切る (既定の経路)
- connect の軸「ブロックするか」は関数で表し、最終手段 (fork) を使うかは `_ex` の引数 `LastResort` で選ぶ ([DR-0031](DR-0031-ex-functions.md))

## 理由

- fork は connect に普通求められる動きではない例外で、サーバに見える peer の pid がもう終了したプロセスの pid になる。普通の環境では connect の順の経路 (Linux の proc-direct、FreeBSD の at-direct、相対パス、広げた sockaddr、Linux と Apple の OS の thread-cwd、Linux の proc、FreeBSD の at、`/tmp` の symlink 等) で副作用無しで届く
- `losses` は、fork の副作用などを受け入れたことを利用者が知り、警告するなどに使えるようにするため。`route` と `skipped` は、どの経路で届いたか・なぜその経路に落ちたかを知り、ログに残すなどに使えるようにするため ([DR-0031](DR-0031-ex-functions.md))
- fork の経路で connect した時にサーバに見える peer の pid は、もう終了したプロセスの pid で、呼び出し元にもサーバにも使い道が無い (サーバはその pid から呼び出し元のプロセスを辿れない)。fork が起き得るかは、`connect_ex` に `LastResort::Allow` を渡すかで利用者が選んでいる
- Unix domain の connect は呼び出しの中で終わるか失敗し、Linux も XNU も `EINPROGRESS` を返さない (確かめた範囲は `route::ConnectOptions` の doc)。なのでブロックしない connect に後で待つものは無い
- `bind` の fork は子が bind するだけで listen は親がするので、listener に相手から見える fork の副作用は残らない ([DR-0025](DR-0025-fork-bind-listen-by-parent.md))
- 軸は 2 つで閉じている (タイムアウトはブロックしない connect で足り、bind は待たない)。関数 4 つで足り、名前で振る舞いが読める

## 却下した案

| 案 | 却下の理由 |
|---|---|
| `connect` も最後は fork する | fork は connect に普通求められる動きではなく、サーバに見える peer の pid がもう終了したプロセスの pid になる。普通の環境では connect の順の経路で届くので、fork が要る場面は最終手段を選んだ利用者だけにする |
| `connect_ex` 系が fork した子の pid を返す | その pid はもう終了したプロセスのもので、呼び出し元にもサーバにも使い道が無い。利用者に要るのは fork したか (`losses` の `forked`) だけ |
| 設定の型 (ブロックするか、fork するか) を持つ connect を 1 つ公開する | 軸は 2 つで閉じていて、関数 4 つで足りる。名前で振る舞いが読める |
| backlog を指定できる bind | 長いパスでも std と同じに使えることが責務で、std の bind にも backlog の指定は無い。要る人は listen し直せばよい (Linux / macOS で効く) |
| fork しない bind を設ける | [DR-0007](DR-0007-fork-child-fchdir.md) の却下した案 |

## 判断の出所

- `connect` は fork しない、fork した子の pid を返さない: kawaz 裁定 2026-11-18
- any 系を作らず既定の操作ごとに `_ex` を置き、最終手段を使うかを引数で選ぶ: kawaz 裁定 (ハブ DR-0015、2026-11-19)。Rust の形: [DR-0031](DR-0031-ex-functions.md)
- 上に無い細部: 出所の記録なし
