# DR-0017: release は CI を通った commit を crates.io の trusted publishing で publish し、その後に tag と GitHub release を作る

- Status: Accepted
- Date: 2026-11-18

## Context

crates.io への publish と、tag・GitHub release の作成をどこで・どの順で行うかを決める。kawaz の他のリポ (stable-which) の release workflow とそろえる。

## Decision

- `src/` を変えたら `Cargo.toml` の version を上げる (`just bump-version`)。`just push` が上げ忘れを止める
- `.github/workflows/release.yml` は main への push (`Cargo.toml` が変わった時) と手動の起動で走る
- release.yml は最初に ci.yml を `workflow_call` で呼び、全部の試験が通ってから先に進む
- version が最新の tag より新しく、まだ release されていない時だけ publish する
- publish は crates.io の trusted publishing (`rust-lang/crates-io-auth-action`、GitHub の environment `crates-io`) で行う。main の commit だけが対象
- tag と GitHub release は、crates.io に publish した後に作る

## 理由

- CI を通ったものだけを release する
- tag を publish の後に作るので、publish が失敗しても tag が残らず、手で起動し直せる
- 既に tag のある version は check-version が断るので、手で起動し直しても二重に release しない

## 却下した案

| 案 | 却下の理由 |
|---|---|
| release を `workflow_run` で起動し、push の CI の完了を待つ | crates.io の trusted publishing が `workflow_run` の起動を受け付けない |
| tag を publish より先に作る | publish が失敗すると tag だけが残る |

## 判断の出所

- kawaz 判断 2026-11-18
