# DR-0002: std が受け付けるパス (sun_path − 1 バイト以内) は std にそのまま渡す

- Status: Accepted
- Date: 2026-11-18

## Context

std の `UnixListener::bind` / `UnixStream::connect` は `sun_path` に収まるパスなら受け付ける。sockat はそれより長いパスに届くための crate で、どこから自前の経路 ([DR-0032](DR-0032-resolve-through-persistent-base.md)) に回すかの境界を決める必要がある。

## Decision

- パスが `sun_path` − 1 バイト以内なら `UnixListener::bind` / `UnixStream::connect` をそのまま呼ぶ。何も足さない (環境変数も読まない)
- 値は OS ごとに違う。libc の `sockaddr_un` の `sun_path` の配列の長さから求める (`sys::SUN_PATH_CAP`)

  | OS | `sun_path` (NUL 込み) | std にそのまま渡す長さ |
  |---|---|---|
  | Linux / Android / illumos | 108 | 107 バイトまで |
  | macOS / iOS / FreeBSD / NetBSD / OpenBSD | 104 | 103 バイトまで |

- std の判定と同じ境界にする。std (`library/std/src/os/unix/net/addr.rs` の `sockaddr_un`) は `bytes.len() >= addr.sun_path.len()` のパスを `InvalidInput` で断る (試験 `the_longest_path_std_accepts_goes_to_std_and_one_byte_more_does_not` が std 自身で確かめる)
- ブロックしない connect ([DR-0032](DR-0032-resolve-through-persistent-base.md)) は std では作れないので、自前のソケット ([DR-0015](DR-0015-own-socket-matches-std.md)) に std と同じアドレスのバイト列を渡す

## 理由

- std が受け付けるパスは std に完全に任せる。閾値を小さくすると std が受け付けるパスまで他の経路に回り、std より悪くなる (例: Linux の 101〜107 バイトのパスが B を通る経路に回り、`local_addr()` が `/proc/<pid>/fd/<n>/<D>/<E>/<name>` になる)
- どの OS でも同じパスが同じ扱いになることは利用者への約束ではない

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 全 OS 共通の閾値 (Linux の値より小さい 100 バイト等) | std が受け付けるパスまで他の経路に回り、std より悪くなる。OS をまたいで同じ扱いにすることは約束していない |

## 未確定

- std が macOS・NetBSD・DragonFly で 253 バイトまでのパスを受け付けるようにする PR ([rust-lang/rust#161149](https://github.com/rust-lang/rust/pull/161149)、issue [#160684](https://github.com/rust-lang/rust/issues/160684)) が review 待ち (2026-11-18 時点で未 merge)。merge されたら、std にそのまま渡す長さをそれに追従させる。ビルドに使う std の版で受け付ける長さが変わる (MSRV 1.85 の std は 103 のまま。[DR-0016](DR-0016-crate-distribution.md)) ので、閾値を版でどう決めるかも合わせて決める
- その PR は NetBSD でも 253 バイトを主張している。NetBSD で広げた sockaddr を使う経路の候補は [DR-0005](DR-0005-widened-sockaddr-on-macos.md) の未確定

## 判断の出所

- 閾値を OS ごとの `sun_path` − 1 にする: kawaz 裁定 2026-11-18
