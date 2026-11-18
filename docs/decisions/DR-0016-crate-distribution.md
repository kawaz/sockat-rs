# DR-0016: crates.io に crate 名 sockat で公開し、MSRV は 1.85、同梱物は include で絞る

- Status: Accepted
- Date: 2026-11-18

## Context

sockat は公開 crate として配る。crate の名前、対応する Rust の最小の版、publish する中身を決める。

## Decision

- crates.io に crate 名 `sockat` で公開する
- MSRV は 1.85 (`Cargo.toml` の `rust-version`)。CI で 1.85 の `cargo check` を回す ([DR-0019](DR-0019-ci-targets.md))
- publish する中身は `include` で `src/**`、`README.md`、`LICENSE`、`Cargo.toml` に絞る

## 理由

- publish するのは、コードと docs.rs / crates.io が表示するもの。リポジトリの道具 (CI、justfile、エージェント向けのルール) と設計の docs はリポジトリに置く
- MSRV の値の理由は記録なし。MSRV の std の上限は 103 バイトのままで、std の上限が広がった時の閾値の決め方に関わる ([DR-0002](DR-0002-std-handles-paths-it-accepts.md) の未確定)

## 判断の出所

- 公開すること: kawaz 裁定 2026-11-18 (SK-Q3)
- crate 名・MSRV・include: 統括の裁定 2026-11-18
