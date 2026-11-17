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

### SK-Q1 Linux で `/proc` が無い時にどうするか

Linux は `/proc/self/fd` の経路で確定しているが、一部のコンテナや chroot では `/proc` が無い。今はエラーにしている。fork に倒すと、connect の時にサーバから見える peer pid (`SO_PEERCRED`) が終了した子の pid に固定される。関係: [DR-0001](decisions/DR-0001-long-socket-path-routes.md)

- [ ] a: fork に倒す (統括推し。「全部だめなら fork」の方針に揃う。pid がずれるのは `/proc` の無い環境で connect した時だけ)
- [ ] b: 今のままエラーにする

### SK-Q2 `TMPDIR` が長すぎる時に `/tmp` も試すか

macOS の symlink の経路は、今は `${TMPDIR:-/tmp}` の 1 か所だけに symlink を作る。`TMPDIR` が長いと、そこで短くならずに fork まで落ちる。

- [ ] a: `TMPDIR` で短くならなければ `/tmp` も試す (統括推し。fork に落ちる場面が減る)
- [ ] b: 今のまま `${TMPDIR:-/tmp}` だけにする

### SK-Q3 GitHub に public リポ `kawaz/sockat` を作って push し、crates.io に 0.1.0 を publish してよいか

作ったものはまだ手元にしかない。初版は 0.1.0、MSRV は 1.85。publish する中身は src / README / LICENSE / Cargo.toml に絞る。

- [ ] a: リポ作成・push・publish まで進める (統括推し。push 後の CI で FreeBSD などの未検証分を確かめてから publish する)
- [ ] b: リポ作成と push までにして、publish は CI を見てから改めて聞く
