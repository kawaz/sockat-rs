# DR-0018: リポは kawaz/sockat-rs、crate 名は sockat、言語によらない仕様の正本はハブ kawaz/sockat の SPEC

- Status: Accepted
- Date: 2026-11-18

## Context

同じ振る舞いを他の言語でも実装する可能性を調べた ([research](../research/2026-11-18-ports-to-other-languages.md))。言語ごとの実装と、言語によらない振る舞いの定義をどこに置くかを決める。

## Decision

- このリポは `kawaz/sockat-rs` (Rust 実装)
- crate 名は `sockat` のまま ([DR-0016](DR-0016-crate-distribution.md))
- 言語によらない振る舞いの正本は、ハブのリポ `kawaz/sockat` の [SPEC](https://github.com/kawaz/sockat/blob/main/docs/SPEC-ja.md)。sockat-rs はその Rust 実装
- 振る舞いを変える時は、先にハブの SPEC を直し、sockat-rs の docs と実装はその後に追従させる

## 理由

- 言語によらない振る舞いを 1 か所で決め、各言語の実装がそれに従う
- それ以外の理由の記録なし

## 判断の出所

- kawaz 指示 2026-11-18
