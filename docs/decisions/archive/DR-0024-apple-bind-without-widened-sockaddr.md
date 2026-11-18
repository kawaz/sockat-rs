# DR-0024: Apple の OS の bind は広げた sockaddr を使わない

- Status: Superseded by DR-0027
- Date: 2026-11-18

## Context

- 広げた sockaddr ([DR-0005](../DR-0005-widened-sockaddr-on-macos.md)) で bind したソケットのアドレスは `struct sockaddr_un` (106 バイト) に入らない。`getsockname` は `2 + パスの長さ` を報告し、`sockaddr_un` の大きさのバッファには NUL 無しで詰めて返す (macOS 26.5、試験 `the_address_of_a_widened_bind_does_not_fit_a_sockaddr_un`: 200 バイトのパスで 202 バイトを報告し、std の `local_addr` は 104 バイトで切れたパスを返す)
- それが読む側を壊す (ハブ [DR-0009](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0009-apple-bind-without-widened-sockaddr.md) の観測。sockat-rs では確かめていない)

| 読む側 | 壊れ方 |
|---|---|
| CPython | `getsockname` が後ろに無関係なバイトを付ける。asyncio の後始末 (`sock.getsockname()` を `os.stat`) が `ENOTDIR` で確率で落ちる |
| Go | `net.FileConn` は相手が広げた形で bind していると `RemoteAddr` が nil で、`.String()` が panic する |

- connect は自分のソケットを bind しないので、広げた sockaddr で connect しても自分のアドレスは切れない

## Decision

| 操作 | Apple の OS の順番 |
|---|---|
| `bind` | 相対パス → symlink → スレッド cwd → fork |
| `connect` 系 | 相対パス → 広げた sockaddr → symlink → スレッド cwd → fork |

- bind の綴りは全部、sockaddr に入る長さ (103 バイト) で判定する: symlink の経路の `<置き場>/sockat.XXXXXXXXXX/<ファイル名>`、スレッド cwd と fork の経路のファイル名。ファイル名が 103 バイトを超える bind は `ENAMETOOLONG`
- 253 バイトまで広げるのは connect 系だけ
- 実装: 広げてよいかを `Request::may_widen` (connect だけ真) で決め、広げた形で渡す関数 `apple::wide_connect` は connect の options しか受け取らない。bind を広げた形で渡す口を crate の中に置かない

## 理由

- bind は fork の経路でも相手から見える副作用が無い ([DR-0025](../DR-0025-fork-bind-listen-by-parent.md)) ので、広げた sockaddr で fork を避ける理由が無い
- bind を広げる口を作らなければ、経路の順番を直した時に bind が広げた形に戻る事故が型の上で起きない

悪い面:

- Apple の OS の bind はファイル名 103 バイトまでになる (253 から下がる)
- `/tmp` を通した bind はファイル名 80 バイトまでになる (230 から下がる)。それを超えるとスレッド cwd、使えなければ fork の経路に進む

## 却下した案

| 案 | 却下の理由 |
|---|---|
| bind も広げた sockaddr を使う | できたソケットのアドレスが `sockaddr_un` に入らず、CPython / Go の標準を壊す |
| bind で広げた sockaddr を fork の後に回す | fork が相手から見える副作用なく届くので要らない。回しても、使った時のアドレスは同じく壊れる |

## 試験

- 広げた形での bind のアドレスが `sockaddr_un` に入らないこと (この決定の前提): `the_address_of_a_widened_bind_does_not_fit_a_sockaddr_un`
- connect 系が広げる綴り (パス、相対パス、symlink、ファイル名) の各々で、bind は広げずに通り、できたアドレスが `sockaddr_un` に入る。103 バイトを超えるファイル名の bind は `ENAMETOOLONG`: `a_bind_never_widens_so_its_address_fits_a_sockaddr_un`
- 公開 API で、長いパスの bind のアドレス (listener、accept した stream、クライアントの `peer_addr`) が std の受け付ける長さに収まる、bind できるファイル名は `sun_path` − 1 まで: `the_address_of_a_long_path_bind_fits_sun_path`、`a_bind_reaches_a_file_name_up_to_sun_path_less_the_nul`
- connect 系は 253 バイトのパスを広げた sockaddr で通す: `the_widened_sockaddr_carries_253_bytes`

## 判断の出所

- Apple の OS の bind は広げた sockaddr を使わない、bind の綴りは 103 バイトで判定する: kawaz 裁定 (ハブ DR-0009、2026-11-18)
- `may_widen` と、connect しか受け取らない `wide_connect` で bind を広げる口を無くす: 実装側の判断 2026-11-18
