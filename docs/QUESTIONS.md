# 裁定・確認待ち一覧 (ユーザ用)

## 運用規約

<details>
<summary>ゼロコンテキストエージェント向け（本セクションは消さない）</summary>

- 裁定/確認待ち項目を 1項目=1ラベル=1セクション で記載
- ラベル形式: XX-Q1（XX は 2-3 文字、バッチやセッション内で一意、Qn単独の使い回し禁止、長期一意性は不要)
- 依頼形式: 「👺XX-Q1 の裁定お願いします」（参照用途ではラベルに👺を付けない。誤陽性がユーザのハイライト/アラームを汚す）
- チャット提示と同一ターンで本ファイルに記録 + path 指定 commit
- 裁定が下りたら該当セクションを即削除し、内容は正規の記録先 (DR / issue / journal) へ反映。本ファイルは常に「現在待ち」だけを持つ
- 参照は[]()で提示（リポ内は相対、リポ外はフルパス）
- 選択肢・確認項目は `- [ ] a: …` 形式（チェックボックス + ラベル）で書く

</details>

## 裁定待ち

### SK-Q5 ブロックしない connect と、子の pid を返す connect (SK-Q4 の裁定) を、どんな形の API で足すか

足す機能は 2 つある。

1. **ブロックしない connect** (llm-gateway からの要望、2026-11-18)
   - 今の `connect` は、Linux では相手の backlog が満杯の間ずっと待つ。macOS はすぐに `ECONNREFUSED` を返す (どちらも実機で確認)
   - tokio の connect は非ブロッキングのソケットで connect するので、満杯ならすぐに `EAGAIN` が返る
   - llm-gateway は `spawn_blocking` で包んでいる。そのため、待ち続ける connect が blocking pool に残り、runtime を落とす時に待たされる
   - 非ブロッキングのソケットで connect すれば、tokio と同じく `EAGAIN` がすぐに返る (Linux で実機確認)。Unix ソケットの connect は途中で止まらず、すぐに結果が出る。返す stream は非ブロッキングのままなので、そのまま `tokio::net::UnixStream::from_std` に渡せる
2. **子の pid も返す connect** (SK-Q4 の裁定: `connect` とは別に足す、次の版で)

この 2 つは独立した軸である。1 つは入力の設定 (ブロックするか) で、もう 1 つは出力 (サーバに見える子の pid) である。別々の関数で足すと、組み合わせの数だけ関数が増える。

関係: [DR-0001](decisions/DR-0001-long-socket-path-routes.md) 決定 6・8

- [ ] a: 設定を持つ connect の型を 1 つ足し、`connect` はそのまま残す (統括推し)
  - 例: `sockat::Connect::new().nonblocking(true).connect(path)` が、stream とサーバに見える子の pid (fork でなければ `None`) を返す
  - 後から設定を足しても関数は増えない
- [ ] b: 関数を 2 つ足す (`connect_nonblocking(path)` と、子の pid も返す connect)
  - 今の 2 つの機能だけなら一番短い
  - 「ブロックせず、かつ pid も知りたい」は、どちらの関数でも満たせない
- [ ] c: ブロックしない connect だけを関数で足し、子の pid の手段は改めて考える
