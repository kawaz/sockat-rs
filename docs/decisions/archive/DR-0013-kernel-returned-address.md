# DR-0013: local_addr / peer_addr はカーネルに渡した綴りを返すままにし、元のパスは利用者が持つ

- Status: Superseded by DR-0032
- Date: 2026-11-18

## Context

戻り値は std の `UnixListener` / `UnixStream` ([DR-0010](DR-0010-public-api.md)) で、`local_addr()` / `peer_addr()` は std が `getsockname` / `getpeername` から作る。長いパスの経路 ([DR-0027](DR-0027-own-directory-and-fallback-routes.md)) では、カーネルに渡す綴りが元のパスと違う。

## Decision

- `local_addr()` / `peer_addr()` は、listener を bind した時にカーネルに渡した綴りを返す。sockat はそれを元のパスに直さない (std の型は返す口を差し替えられない)

  | bind の経路 | 返るもの |
  |---|---|
  | スレッド cwd、fork、bindat (D の中の R を通す) | `R/<ファイル名>` |
  | /proc 版 | `/proc/self/fd/<fd>/R/<ファイル名>` |
  | D 無しの fork | `.sockat.XXXXXXXXXX/<ファイル名>` |

- R (と D 無しの fork の `.sockat.XXXXXXXXXX`) は bind の後に消えているので、返った綴りで消す・`stat` する・`chmod` しても何にも当たらない (`/proc/self/fd/<n>` の番号が使い回されても、どの cwd から解決しても)
- どの綴りも `sun_path` に収まるので、返るアドレスは途中で切れない (bind は広げた sockaddr を使わない、[DR-0027](DR-0027-own-directory-and-fallback-routes.md))
- 利用者には元のパスを持ってもらう。doc にそう書く。ソケットのファイルを消すなら元のパスか `bind_ex` ([DR-0031](DR-0031-ex-functions.md)) で。同じ綴りが `Debug` やフレームワークのログ・接続元の情報にも出ることも書く
- OpenBSD では、103 バイト以内の短いパス ([DR-0002](DR-0002-std-handles-paths-it-accepts.md) で std にそのまま渡す) でも、std の `local_addr()` がパスの末尾 1 バイトを欠いて返す (CI の VM で観測、std の `UnixListener::bind` を直接使った場合)。sockat は手を入れない。std が綴りそのものを返す前提を見る試験は、OpenBSD では外す

## 理由

- 綴りで後始末や再接続をするのは上のライブラリや利用者のコードで、sockat からは止められない。なので綴りの側を、後でどこで解決されても何にも当たらない形にする ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))
- 綴りで後始末や再接続をする Rust のライブラリは、調べた範囲 (tokio、mio、axum、tonic、hyper-util、warp、tower、actix-server / actix-web、tokio-listener、interprocess、async-std、async-net / smol、async-io、hyperlocal、2026-11-19) に無い。R を挟まない綴り (`/proc/self/fd/<n>/<ファイル名>`) では、利用者の `remove_file` が無関係なファイルを消すことを docker で確かめた (試験 `the_spelling_kept_for_a_bound_socket_names_nothing_afterwards` が、R を挟む今の綴りでは当たらないことと、R を挟まない綴りなら当たることを確かめる)
- OpenBSD の振る舞いは sockat ではなく std / OS のもの

## 未確定

- OpenBSD の末尾 1 バイトの欠けの原因が、OpenBSD の `getsockname` の返す長さか std の解釈かは調べていない

## 判断の出所

- 綴りを元のパスに直さない: 出所の記録なし
- 綴りに R を挟んで何にも当たらない形にする: kawaz 裁定 (ハブ DR-0012、2026-11-19)
