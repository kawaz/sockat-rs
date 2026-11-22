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

(なし)

### VB-Q3 sockat-cli の crates.io への初回の公開を、どう行うか

crates.io の trusted publishing (今の release.yml の publish の仕組み) は、crate が既にある時にしか設定できない ([RFC 3691](https://github.com/rust-lang/rfcs/blob/master/text/3691-trusted-publishing-cratesio.md): "A Trusted Publisher Configuration can only be created after an initial manual publishing of a crate."。RFC は作る前の PENDING の設定を将来の案に挙げている。今の crates.io で作る前の設定ができるかは未確認)。sockat-cli は新しい crate なので、初回は CI から publish できない見込み。

統括推し: a。ライブラリ 0.6.0 を CI で出した後に、kawaz の手元で `cargo publish -p sockat-cli` を 1 回だけ行い、crates.io の sockat-cli の設定に trusted publisher (kawaz/sockat-rs、release.yml、環境 crates-io) を足す。次の版から release.yml が両方を出す。

あわせて sockat-cli の版は、ライブラリと同じ版で動かす形 (0.6.0 から。`just bump-version` が両方を上げる) にしたい。CLI はライブラリを `=x.y.z` で固定するので、ライブラリを出すたびに CLI も出し直しになり、別の版の番号を持つ利点が無い。

- [ ] a: 0.6.0 は CI でライブラリだけ出し、CLI の初回は kawaz の手元で publish して trusted publisher を設定する
- [ ] b: crates.io で作る前の trusted publisher の設定ができるか kawaz が確かめ、できれば CI で両方を出す
- [ ] c: CLI の公開は 0.6.0 の後に回す
