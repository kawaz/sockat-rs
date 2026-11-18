# DR-0005: macOS では広げた sockaddr で 253 バイトまで渡す (FreeBSD では使わない)

- Status: Accepted
- Date: 2026-11-18

## Context

XNU は `bind(2)` / `connect(2)` で `SOCK_MAXADDRLEN` (255) バイトまでのアドレスを受け付け (`getsockaddr`)、`sun_len - offsetof(sun_path)` バイトをパスとして取る (`unp_bind` / `unp_connect`、それが 255 未満であること)。長さと family の 2 バイトを除いて 253 バイトのパスが、NUL 無しで入る。libuv が #5047 で同じ形を使っている。

## Decision

- macOS では、相対パスで届かない時 ([DR-0004](DR-0004-relative-path-from-cwd.md)) に、元のパス、無理なら相対パスを、253 バイト以内なら広げた sockaddr で渡す
- `sun_len` とアドレス長はどちらも `2 + パスの長さ` にする
- カーネルがこの形を受け付けない印は `EINVAL` と `ENAMETOOLONG` とし、それなら symlink の経路 ([DR-0006](DR-0006-symlink-in-safe-temp-dir.md)) に進む。以降は std にそのまま渡せる長さで見る
- 受け付ける間は、symlink の経路の綴りと fork の経路のファイル名 ([DR-0007](DR-0007-fork-child-fchdir.md)) にも広げた sockaddr を使う
- FreeBSD では使わない ([DR-0008](DR-0008-freebsd-bindat-and-other-unix.md))

## 理由

- 253 バイトで bind / connect が通り、socket がそのパスにできることを実機で確かめた (試験 `the_widened_sockaddr_carries_253_bytes`)
- 元のパスをそのままカーネルに解決させるので、ディレクトリを先に引く限界 ([DR-0001](DR-0001-long-socket-path-routes.md)) が無い
- 受け付けない印で次へ進むのは、将来の macOS が受け付けなくなった場合の逃げ道。今の macOS では起きない

## 却下した案

| 案 | 却下の理由 |
|---|---|
| FreeBSD でも広げた sockaddr を使う | libuv が #5047 の後に「FreeBSD で uv_pipe_bind が壊れた」(#5134) と報告しており、XNU と同じ前提を置けない。`bindat` の方が副作用も無い |

## 未確定

- std の上限を広げる PR (rust-lang/rust#161149、[DR-0002](DR-0002-std-handles-paths-it-accepts.md)) は NetBSD・DragonFly でも 253 バイトを主張している。NetBSD でも広げた sockaddr が効くなら、NetBSD の経路の候補になる (未検証)

## 判断の出所

- 広げた sockaddr の経路: kawaz 指示 2026-11-18
- FreeBSD で使わない: 統括の裁定 2026-11-18
- 上に無い細部: 出所の記録なし
