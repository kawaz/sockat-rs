# DR-0028: 後始末つきの bind は `bind_ex` で、listener と消す口 `SocketFile` を返す。listener を閉じるのは利用者

- Status: Accepted
- Date: 2026-11-19

## Context

- 長いパスの bind では、カーネルが覚える綴りは元のパスでない ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))。上のライブラリが綴りでソケットのファイルを消す後始末 (Go の `UnixListener.Close` 等) は効かない
- 利用者が元のパスで `remove_file` すると、間に同じパスへ別のソケットが bind されていた時に、それを消す
- listener は std の `UnixListener` のまま返し、tokio 等へ move できる必要がある ([DR-0010](DR-0010-public-api.md)、[DR-0014](DR-0014-no-tokio-dependency.md))
- ハブの SPEC は `_ex` を「bind したソケットのファイルの realpath と `(st_dev, st_ino)` を覚え、`discard()` でそのパスの `(st_dev, st_ino)` が同じ時だけ消す」とする。discard が listener を閉じるかはハブ QUESTIONS.md D12-Q4 で kawaz 確認中

## Decision

```rust
pub fn bind_ex<P: AsRef<Path>>(path: P) -> io::Result<(UnixListener, SocketFile)>;

#[derive(Debug)]
pub struct SocketFile { /* realpath, (st_dev, st_ino) */ }

impl SocketFile {
    pub fn discard(self) -> io::Result<()>;
}
```

- `bind_ex` は `bind` と同じ経路で bind し、ソケットのファイルの realpath と `(st_dev, st_ino)` を覚えた `SocketFile` を listener と組で返す
- 覚えるのは bind が通った直後、経路の中で listener を返す前 (`route::bind_remembering`)
  - std に渡す長さのパス: std の bind が返った直後に `realpath(3)` と `lstat(2)`
  - D を通す経路: D と R が残っている間に、bind と同じ綴り `R/<ファイル名>` で D の fd から `fstatat(AT_SYMLINK_NOFOLLOW)` (R は辿り、ソケットのファイル自体は辿らない)。パスは R の先 (ソケットのディレクトリの realpath) とファイル名。スレッド cwd はスレッドが終わった直後、fork は子の終了を受けた後
- 覚えられなければ listener を閉じ、ファイルは消さずにエラーを返す
- `discard()` は realpath を `lstat` し、`(st_dev, st_ino)` が同じ時だけ `unlink` する。違う物 (別のソケット、ファイル、symlink) があれば触らず、何も無ければ (`lstat` が `ENOENT` / `ENOTDIR`) 何もしない。どちらも `Ok(())`。確かめられない時 (`lstat` がそれ以外で失敗、`EACCES` 等) と `unlink` の失敗はエラー (試験 `bind_ex_discard_errs_only_when_it_cannot_tell`)
- `discard()` は listener を閉じない。閉じるのは listener を持っている利用者 (tokio、サーバのフレームワーク等)
- `SocketFile` を drop しても何もしない
- 短いパス (std にそのまま渡す) でも同じく覚えて消せる
- 名前 `bind_ex` / `SocketFile` / `discard` はハブの SPEC で仮置き

## 理由

- listener を tokio 等へ move した後も消せる。消す口が listener を持たないので、二重の close も起きない
- realpath で覚えるので、bind の後に途中の symlink が張り替えられても、bind した場所のファイルを見る
- `(st_dev, st_ino)` で照合するので、間に同じパスへ bind された別のソケットを消さない
- bind の直後に経路の中で覚えるので、bind が返ってから利用者の手に渡るまでの間のすり替えを覚えない。R を通した綴りで引くので、bind したファイルそのものを見る
- 覚えられなかった時に `SocketFile` を返すと、消せない `discard()` が成功に見える
- 自分のファイルが既に無い時 (消された、すり替えられた) にエラーにしないのは、`discard()` の目的 (bind したファイルを残さない) が満たされているから

## 却下した案

| 案 | 却下の理由 |
|---|---|
| `discard()` が listener を閉じてから消す | listener を tokio 等へ move すると、消す口が閉じる対象を持てない |
| `SocketFile` の drop で消す | listener より先に drop されると、生きている listener のファイルが消える。消す時を利用者が選べない |
| bind が返った後に `bind_ex` の中で元のパスを `lstat` する | bind してから引くまでの間のすり替えを覚えうる |
| すり替えられていた時にエラーを返す | 自分のファイルは既に無く、利用者にできることが無い |

## 悪い面

- 確かめてから消すまでと、bind してから覚えるまでの間に、ソケットのディレクトリに書ける者がファイルをすり替えるのは防げない (inode を指定して消す手段が POSIX に無い)
- 消されたファイルの inode 番号が同じデバイスの新しいファイルに使い回されると、そのファイルを bind したものと見なす
- `SocketFile` と listener を別々に持つので、閉じてから消す順は利用者が守る

## 未確定

- 名前 `bind_ex` / `SocketFile` は仮 (`discard` は確定)

## 判断の出所

- `_ex` の振る舞い (realpath と `(st_dev, st_ino)` を覚え、同じ時だけ消す、bind の直後に覚え、覚えられなければエラー): ハブの SPEC (2026-11-19)
- `discard()` は消すだけで listener を閉じない、名前 `discard`: kawaz 裁定 2026-11-19 (ハブ DR-0014、D12-Q4)
- Rust の形 (`bind_ex(path) -> (UnixListener, SocketFile)`、`discard(self)`、Drop では消さない、derive Debug): 統括の裁定 2026-11-19 (ハブ DR-0014 の表と同じ)
- 何も無い時・すり替えられていた時も `Ok(())`: 実装側の判断 2026-11-19 (ハブ SPEC で確定)
- 確かめられない時はエラー: ハブ SPEC 8b001a76 (sockat-py の指摘)
