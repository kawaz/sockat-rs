# DR-0013: local_addr / peer_addr はカーネルに渡した綴りを返すままにし、元のパスは利用者が持つ

- Status: Accepted
- Date: 2026-11-18

## Context

戻り値は std の `UnixListener` / `UnixStream` ([DR-0010](DR-0010-public-api.md)) で、`local_addr()` / `peer_addr()` は std が `getsockname` / `getpeername` から作る。長いパスの経路 ([DR-0001](DR-0001-long-socket-path-routes.md)) では、カーネルに渡す綴りが元のパスと違う。

## Decision

- `local_addr()` / `peer_addr()` は、listener を bind した時にカーネルに渡した綴りを返す。sockat はそれを元のパスに直さない

  | bind の経路 | 返るもの |
  |---|---|
  | /proc | `/proc/self/fd/<fd>/<ファイル名>` (fd はもう閉じている) |
  | 相対パス | 相対パス |
  | symlink | もう無い symlink を通したパス |
  | スレッド cwd、fork、bindat | ファイル名だけ |

- どの経路の綴りも `sun_path` に収まるので、返るアドレスは途中で切れない (Apple の OS の bind は広げた sockaddr を使わない、[DR-0024](DR-0024-apple-bind-without-widened-sockaddr.md))
- 利用者には元のパスを持ってもらう。doc にそう書く。返った綴りで消す・`stat` する・`chmod` すると別のファイルに当たりうること (`/proc/self/fd/<n>` の fd は閉じていて番号が使い回される、相対パスとファイル名だけの綴りはその時の cwd から解決される) と、同じ綴りが `Debug` やフレームワークのログ・接続元の情報にも出ることも書く
- OpenBSD では、103 バイト以内の短いパス ([DR-0002](DR-0002-std-handles-paths-it-accepts.md) で std にそのまま渡す) でも、std の `local_addr()` がパスの末尾 1 バイトを欠いて返す (CI の VM で観測、std の `UnixListener::bind` を直接使った場合)。sockat は手を入れない。std がパスそのものを返す前提を見る試験は、OpenBSD では外す

## 理由

- OpenBSD の振る舞いは sockat ではなく std / OS のもの
- それ以外の理由の記録なし
- 綴りで後始末や再接続をするライブラリは、調べた範囲 (tokio、mio、axum、tonic、hyper-util、warp、tower、actix-server / actix-web、tokio-listener、interprocess、async-std、async-net / smol、async-io、hyperlocal、2026-11-19) に無い。害が出るのは利用者のコードが綴りを使う時で、Linux の /proc の綴りで `remove_file` が無関係なファイルを消すことを docker で確かめた

## 未確定

- OpenBSD の末尾 1 バイトの欠けの原因が、OpenBSD の `getsockname` の返す長さか std の解釈かは調べていない

## 判断の出所

- 出所の記録なし
