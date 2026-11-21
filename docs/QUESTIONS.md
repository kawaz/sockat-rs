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

### VB-Q1 次の版は 0.5.2 か 0.6.0 か

手元の main に積んだ 9 commit (ハブ 80d6eec4・741dde83・be5e0b9a 等の反映) に、Rust の semver で破壊的な変更が入っている。kawaz の裁定 PB-Q2 = a は「破壊的な変更を含む版は kawaz に聞く」。ハブ統括は 0.5.2 (patch) で可と判断した。

- `Spelling::Linked` の variant に `#[non_exhaustive]`: Cargo book の SemVer Compatibility で major (既存の enum の variant に足す)。0.x では 0.5.1 → 0.5.2 は `cargo update` で自動で入るので、`Linked { removable, until, usable_cross_uid, usable_with_private_tmp }` と `..` 無しで分解している利用者は update だけでビルドが壊れる
- 振る舞いの変更: 空のパスが Linux でも `InvalidInput` (std は abstract に autobind して成功していた)、既定の名前の B が占拠されていたら乱数の B を作らず次の置き場へ

統括推し: b。理由は、cargo が自動で入れる版でビルドを壊さないため (0.5.1 は今日出たばかりで利用者は少なく、0.6.0 にする費用はほぼ無い)。

- [ ] a: 0.5.2 (patch) で出す
- [ ] b: 0.6.0 (minor = 0.x の major) で出す

