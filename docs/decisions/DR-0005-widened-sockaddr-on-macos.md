# DR-0005: Apple の OS の connect 系では広げた sockaddr で 253 バイトまで渡す (bind と FreeBSD では使わない)

- Status: Accepted
- Date: 2026-11-18

## Context

XNU は `bind(2)` / `connect(2)` で `SOCK_MAXADDRLEN` (255) バイトまでのアドレスを受け付け (`getsockaddr`)、`sun_len - offsetof(sun_path)` バイトをパスとして取る (`unp_bind` / `unp_connect`、それが 255 未満であること)。長さと family の 2 バイトを除いて 253 バイトのパスが、NUL 無しで入る。libuv が #5047 で同じ形を使っている。

広げた形で bind したソケットのアドレスは `struct sockaddr_un` (106 バイト) に入らない。`getsockname` は `2 + パスの長さ` を報告し、`sockaddr_un` の大きさのバッファには NUL 無しで詰めて返す (macOS 26.5、試験 `the_address_of_a_widened_bind_does_not_fit_a_sockaddr_un`: 200 バイトのパスで 202 バイトを報告し、std の `local_addr` は 104 バイトで切れたパスを返す)。CPython の `getsockname` は後ろに無関係なバイトを付け、Go の `net.FileConn` は相手の `RemoteAddr` が nil になる (ハブの観測)。connect は自分のソケットを bind しないので、広げた形で connect しても自分のアドレスは切れない。

## Decision

- Apple の OS の connect の順 ([DR-0032](DR-0032-resolve-through-persistent-base.md)) で、相対パスで届かない時 ([DR-0004](DR-0004-relative-path-from-cwd.md)) に、元のパス、無理なら相対パスを、253 バイト以内なら広げた sockaddr で渡す
- bind には使わない。bind の綴りは全部、標準の `sun_path` に入る長さで判定する。広げた形で渡す関数 (`route::apple::wide_connect`) は connect の options しか受け取らず、bind を広げる口を crate の中に置かない
- thread-cwd-direct と link-file は、標準の `sun_path` に入る長さで判定する
- `sun_len` とアドレス長はどちらも `2 + パスの長さ` にする
- カーネルがこの形を受け付けない印は `EINVAL` と `ENAMETOOLONG` とし、それなら次の経路 (thread-cwd-direct) に進む
- FreeBSD では使わない (connectat がある)

## 理由

- 253 バイトで connect が通ることを実機で確かめた (試験 `the_widened_sockaddr_carries_253_bytes`)
- 元のパスをそのままカーネルに解決させるので、ディレクトリを先に引く限界が無い
- bind で使うと、できたソケットのアドレスが読む側を壊す。bind は B・D・E を通す綴りと thread-cwd で届く ([DR-0032](DR-0032-resolve-through-persistent-base.md))
- 受け付けない印で次へ進むのは、将来の macOS が受け付けなくなった場合の逃げ道。今の macOS では起きない

## 却下した案

| 案 | 却下の理由 |
|---|---|
| FreeBSD でも広げた sockaddr を使う | libuv が #5047 の後に「FreeBSD で uv_pipe_bind が壊れた」(#5134) と報告しており、XNU と同じ前提を置けない。connectat の方が副作用も無い |
| bind も広げた sockaddr を使う | できたソケットのアドレスが `sockaddr_un` に入らず、CPython / Go の標準を壊す |

## 未確定

- std の上限を広げる PR (rust-lang/rust#161149、[DR-0002](DR-0002-std-handles-paths-it-accepts.md)) は NetBSD・DragonFly でも 253 バイトを主張している。NetBSD でも広げた sockaddr が効くなら、NetBSD の経路の候補になる (未検証)

## 判断の出所

- 広げた sockaddr の経路: kawaz 指示 2026-11-18
- connect 系だけに使う: kawaz 裁定 (ハブ DR-0009、2026-11-18)
- connect の経路に置く: kawaz 裁定 (ハブ DR-0012、2026-11-19)。connect の順で relative の次 (thread-cwd の前) に置く: kawaz 裁定 (ハブ SPEC 12dc1577)
- FreeBSD で使わない: 統括の裁定 2026-11-18
- 上に無い細部: 出所の記録なし
