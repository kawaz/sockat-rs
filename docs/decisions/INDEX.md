# Decision Records 一覧

Status は各 DR ファイルの `Status:` 行が正本。

状態は絵文字とラベルだけ。日付・裁定の内訳は各 DR 本文に書く。

- `💭 提案`: Decision がまだ裁定されていない。実装に着手しない (本文の `Status: Proposed`)
- `✅ 実装済`: Decision の全部に実装エビデンスがある
- `🟡 部分実装`: 一部のみ実装
- `⬜ 未実装`: 裁定済みで設計のみ、実装エビデンスなし (着手してよい)
- `🚧 進行中`: 実装の途中
- `N/A`: 実装対象でない (命名・思想・プロセス等)
- `❌ 撤退`: 撤退判断済

| DR | 状態 | 説明 |
|---|---|---|
| [DR-0002](DR-0002-std-handles-paths-it-accepts.md) | ✅ 実装済 | std が受け付けるパス (OS ごとの sun_path − 1 バイト以内) は std にそのまま渡す |
| [DR-0004](DR-0004-relative-path-from-cwd.md) | ✅ 実装済 | connect 系に cwd からの相対パス (`/proc` の無い Linux・Apple の OS 等で、書き込みもスレッドも要らない経路)。cwd の接頭辞を削る、だめなら実パスに揃えて `..` で上る |
| [DR-0005](DR-0005-widened-sockaddr-on-macos.md) | ✅ 実装済 | Apple の OS の connect 系は relative の次に広げた sockaddr を 253 バイトまで。bind と FreeBSD では使わない |
| [DR-0009](DR-0009-windows-out-of-scope.md) | ✅ 実装済 | Windows は対象外 (Unix 以外は compile_error!) |
| [DR-0011](DR-0011-routes-not-exposed.md) | ✅ 実装済 | 経路は公開 API に出さない (名前は診断用)。試験は crate 内で各経路を強制し、環境を読む部分 (`route::Config`) を値で差し替える |
| [DR-0012](DR-0012-route-failure-handling.md) | ✅ 実装済 | 経路の本当の失敗はやり直さない (bind が失敗したら作った E を消す)。どの経路も使えなければ ENAMETOOLONG、ディレクトリが無ければどの OS でも ENOENT |
| [DR-0014](DR-0014-no-tokio-dependency.md) | ✅ 実装済 | tokio は依存に入れない (dev-dependency だけ)。std の型から変換する |
| [DR-0015](DR-0015-own-socket-matches-std.md) | ✅ 実装済 | 自前のソケットは std と同じ設定 (close-on-exec、SO_NOSIGPIPE、backlog)。閉じてもソケットファイルは消さない |
| [DR-0016](DR-0016-crate-distribution.md) | ✅ 実装済 | crate 名 sockat で crates.io に公開。MSRV 1.85、同梱物は include で絞る |
| [DR-0017](DR-0017-release-flow.md) | ✅ 実装済 | release は ci.yml を呼んで通ってから trusted publishing で publish し、その後に tag と GitHub release |
| [DR-0018](DR-0018-repo-name-and-spec-hub.md) | N/A | リポは kawaz/sockat-rs、crate 名は sockat、仕様の正本はハブ kawaz/sockat の SPEC |
| [DR-0019](DR-0019-ci-targets.md) | ✅ 実装済 | CI は macOS / Linux / FreeBSD・NetBSD・OpenBSD・illumos の VM で全試験、cross check (FreeBSD・NetBSD・iOS・illumos)、MSRV、cargo-audit |
| [DR-0021](DR-0021-nul-is-invalid-at-any-length.md) | ✅ 実装済 | NUL を含むパスは長さによらず InvalidInput で何も作らない。Linux の先頭の NUL は std にそのまま渡す |
| [DR-0030](DR-0030-conformance-cli.md) | ✅ 実装済 | 適合試験の CLI は `examples/conformance.rs`。公開の `bind_ex` / `connect_ex` に入力の `Options` を渡して呼び、経路・B・綴りの性質を出す。`just conformance` で起こす |
| [DR-0032](DR-0032-resolve-through-persistent-base.md) | ✅ 実装済 | ハブ DR-0017 の Rust の形: 公開 API (stream・`datagram`・resolve・`sweep`、`Options`・`Spelling`・`Bound`・`SocketFile`)、B・D・E をプロセスの登録簿と flock で持つ、OS ごとの試す順。fork・最終手段・losses は無い |
| [DR-0033](DR-0033-cli-crate-and-internal-entry.md) | 🚧 進行中 | コマンド `sockat` は workspace の別パッケージ sockat-cli (`cli/`)。B の作り方は feature `__cli-internal` の後ろの doc に出さない `sockat::__cli` から使い、ライブラリの依存と API は増やさない。ライブラリは `=x.y.z` で固定 |

退避した DR は [archive/INDEX.md](archive/INDEX.md)。
