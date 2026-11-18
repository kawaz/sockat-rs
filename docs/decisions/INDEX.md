# Decision Records 一覧

Status は各 DR ファイルの `Status:` 行が正本。

- `💭 提案`: Decision がまだ裁定されていない。実装に着手しない (本文の `Status: Proposed`)
- `✅ 実装済`: Decision の全部に実装エビデンスがある
- `🟡 部分実装`: 一部のみ実装
- `⬜ 未実装`: 裁定済みで設計のみ、実装エビデンスなし (着手してよい)
- `N/A`: 実装対象でない (命名・思想・プロセス等)
- `❌ 撤退`: 撤退判断済

| DR | 状態 | 説明 |
|---|---|---|
| [DR-0001](DR-0001-long-socket-path-routes.md) | ✅ 実装済 | sun_path を超える socket のパスに届く経路と、試す順番 (Linux は /proc、FreeBSD は bindat、他の Unix と /proc の無い Linux は相対パス → 広げた sockaddr (macOS だけ) → 安全な置き場の symlink → fork。`connect` は fork せず、fork するのは `bind` と `connect_any` 系。Windows は対象外) |
