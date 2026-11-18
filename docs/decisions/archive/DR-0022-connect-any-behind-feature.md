# DR-0022: connect_any 系は feature `any` の後ろに置く

- Status: Superseded by DR-0029
- Date: 2026-11-18

## Context

- 中心の経路と予備の経路 ([DR-0027](../DR-0027-own-directory-and-fallback-routes.md)) で、`connect` / `connect_nonblocking` が届かない場面はほぼ無い。`connect_any` 系 ([DR-0010](../DR-0010-public-api.md)) が要るのは、fork が最後の手になる環境 (中心の経路を connect に使えない NetBSD / OpenBSD / illumos、seccomp の filter が掛かり `/proc` の無い Linux、`pthread_fchdir_np` の無い Apple の OS で、予備の経路も届かない時) だけ
- 既定の API に `connect` と並んで出ていると、エディタの補完などで利用者がどちらを使うか迷う

## Decision

- `connect_any` / `connect_any_nonblocking` と、その戻り値の `AnyConnection` ([DR-0026](DR-0026-any-connection-and-feature-any.md)) は crate feature `any` を有効にした時だけ公開する。feature は既定では無効
- 有効にする書き方: `sockat = { version = "0.4", features = ["any"] }`
- fork の経路と経路の順番の実装は `bind` も使うので、feature によらずコンパイルする。feature が切り替えるのは公開する 2 つの関数と 1 つの型だけ
- docs.rs は全 feature を有効にして build する (`[package.metadata.docs.rs] all-features = true`)
- feature が要ることは doc に stable で書ける形で示す: 2 つの関数と型の doc の先頭の 1 文 (「Available on crate feature `any` only.」) と、crate doc の Features の節。`doc(cfg)` (nightly) は使わない
- feature を有効にしない時に存在しない関数への intra-doc link を、他の item の doc から張らない (名前だけ書く)
- 試験と CI は `--all-features` で回す。feature 無しの clippy も `just check` で回す (CI の test job が `just ci` 経由で macOS と Linux で回す)
- API を壊すので 0.3.0 にする。feature 名を `any` にするのは 0.4.0 ([DR-0026](DR-0026-any-connection-and-feature-any.md))

## 理由

- 有効にする手間が「fork の副作用を受け入れる」という意思表示になる
- ほぼ使わない関数で、普段の補完と doc の入口を散らかさない
- `doc(cfg)` は nightly の機能で、docs.rs の nightly の変化に doc の build が引きずられる。1 文で足りる
- 実装を feature で分けないのは、`bind` が同じ fork の経路を使い、`connect` 系も同じ順番の関数を通るから。feature 無しで dead_code は出ない (clippy で確認)

## 却下した案

| 案 | 却下の理由 |
|---|---|
| 既定の API にそのまま残す | 補完で `connect` と並び、利用者を迷わせる |
| 消す | fork が最後の手になる環境で、fork を受け入れてでもつなぎたい人の逃げ道が無くなる |
| feature を既定で有効にする | 既定の API から外す目的に合わない |
| `cfg_attr(docsrs, doc(cfg(...)))` で docs.rs にだけ印を出す | nightly の機能に頼る。doc の 1 文で同じことが伝わる |

## 判断の出所

- connect_any 系を既定の API に出さず feature の後ろに置く、docs.rs では全 feature を有効にし feature が要ることを表示する、0.3.0 にする: kawaz 裁定 (ハブ [DR-0007](https://github.com/kawaz/sockat/blob/main/docs/decisions/DR-0007-connect-any-is-opt-in.md)、2026-11-18)
- feature 名を `any` にする: kawaz 裁定 (ハブ DR-0007、2026-11-18)
- `doc(cfg)` を使わず doc の 1 文にする、feature 無しの clippy を `just check` に足す: 実装側の判断 2026-11-18
