# DR-0014: tokio は依存に入れない

- Status: Accepted
- Date: 2026-11-18

## Context

async ランタイム (tokio 等) から sockat を使いたい利用者がいる。戻り値は std の型 ([DR-0010](DR-0010-public-api.md))。

## Decision

- tokio は dev-dependency にだけ入れる。feature で任意にもしない
- listener は `set_nonblocking(true)` してから `tokio::net::UnixListener::from_std` に、stream は `connect_nonblocking` で得たものをそのまま `tokio::net::UnixStream::from_std` に渡す (runtime の worker で待たない)
- この手順を crate doc の例に書き、doctest で compile を確かめ、結合試験 (`tests/tokio_interop.rs`) で実際に送受信する

## 理由

- 戻り値が std の型なので、変換は利用者側の 2 行で済む
- feature にすると、crate が tokio の版を縛る

## 却下した案

| 案 | 却下の理由 |
|---|---|
| tokio を feature で任意にする | 変換は利用者側の 2 行で済む。feature にすると crate が tokio の版を縛る |

## 判断の出所

- 実装側の判断 2026-11-18
