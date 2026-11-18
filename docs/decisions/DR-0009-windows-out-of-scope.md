# DR-0009: Windows は対象外

- Status: Accepted
- Date: 2026-11-18

## Context

sockat の経路 ([DR-0027](DR-0027-own-directory-and-fallback-routes.md)) は Unix のシステムコールと libc の `sockaddr_un` で組まれている。Unix 以外の target で何をするかを決める必要がある。

## Decision

- Unix 以外は `compile_error!` で止める
- 将来入れるとしたら、相対パスで短くする経路だけになる見込み。そのための作り込み (抽象化や型の分岐) は今はしない

## 理由

- 理由の記録なし

## 判断の出所

- Windows を対象外にする: kawaz 指示 2026-11-18
