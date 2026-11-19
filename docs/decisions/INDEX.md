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
| [DR-0004](DR-0004-relative-path-from-cwd.md) | ✅ 実装済 | connect 系に cwd からの相対パス (`/proc` の無い Linux・Apple の OS 等の connect の順の最初)。cwd の接頭辞を削る、だめなら実パスに揃えて `..` で上る |
| [DR-0005](DR-0005-widened-sockaddr-on-macos.md) | ✅ 実装済 | Apple の OS の connect 系は relative の次と最後の fork で広げた sockaddr を 253 バイトまで。bind と FreeBSD では使わない |
| [DR-0006](DR-0006-symlink-in-safe-temp-dir.md) | ✅ 実装済 | connect 系の最後に、`TMPDIR` → `/tmp` の安全な置き場にソケットのファイルを指す `sockat.XXXXXXXXXX` の symlink |
| [DR-0007](DR-0007-fork-child-fchdir.md) | ✅ 実装済 | 最後の手として fchdir した子を fork する (bind は D の中の R を通す)。子は async-signal-safe だけ、副作用は doc に書く |
| [DR-0009](DR-0009-windows-out-of-scope.md) | ✅ 実装済 | Windows は対象外 (Unix 以外は compile_error!) |
| [DR-0010](DR-0010-public-api.md) | ✅ 実装済 | 公開 API は bind・connect・connect_nonblocking と、その `_ex`。`connect` は fork せず、fork するのは最終手段を使う `connect_ex` 系だけ |
| [DR-0011](DR-0011-routes-not-exposed.md) | ✅ 実装済 | 経路は公開 API に出さない。試験は crate 内で各経路を強制し、環境を読む部分 (`route::Config`) を値で差し替える |
| [DR-0012](DR-0012-route-failure-handling.md) | ✅ 実装済 | 経路の本当の失敗はやり直さない。fork しない connect は connect の順の経路の後で ENAMETOOLONG、ディレクトリが無ければどの OS でも ENOENT |
| [DR-0013](DR-0013-kernel-returned-address.md) | ✅ 実装済 | local_addr / peer_addr はカーネルに渡した綴り (`R/<ファイル名>` 等) を返すまま。R は消えているので何にも当たらない。元のパスは利用者が持つ |
| [DR-0014](DR-0014-no-tokio-dependency.md) | ✅ 実装済 | tokio は依存に入れない (dev-dependency だけ)。std の型から変換する |
| [DR-0015](DR-0015-own-socket-matches-std.md) | ✅ 実装済 | 自前のソケットは std と同じ設定 (close-on-exec、SO_NOSIGPIPE、backlog)。ソケットファイルは消さない |
| [DR-0016](DR-0016-crate-distribution.md) | ✅ 実装済 | crate 名 sockat で crates.io に公開。MSRV 1.85、同梱物は include で絞る |
| [DR-0017](DR-0017-release-flow.md) | ✅ 実装済 | release は ci.yml を呼んで通ってから trusted publishing で publish し、その後に tag と GitHub release |
| [DR-0018](DR-0018-repo-name-and-spec-hub.md) | N/A | リポは kawaz/sockat-rs、crate 名は sockat、仕様の正本はハブ kawaz/sockat の SPEC |
| [DR-0019](DR-0019-ci-targets.md) | ✅ 実装済 | CI は macOS / Linux / FreeBSD・NetBSD・OpenBSD・illumos の VM で全試験、cross check、MSRV、cargo-audit |
| [DR-0021](DR-0021-nul-is-invalid-at-any-length.md) | ✅ 実装済 | NUL を含むパスは長さによらず InvalidInput で何も作らない。Linux の先頭の NUL は std にそのまま渡す |
| [DR-0025](DR-0025-fork-bind-listen-by-parent.md) | ✅ 実装済 | fork の経路の bind は listen を必ず親がする。クライアントに見えるサーバの peer は send の前から親。相手から見えない副作用は残る |
| [DR-0027](DR-0027-own-directory-and-fallback-routes.md) | ✅ 実装済 | 長いパスの bind は自分専用のディレクトリ D を fd で掴み、その中の乱数の symlink R を通して `R/<ファイル名>` で bind する (thread-cwd → proc → fork、FreeBSD は at)。D をどこにも作れない bind は `-sockdir` の経路。connect は書き込みもスレッドも要らない経路を先に試す OS ごとの順 (例: `/proc` のある Linux は proc-direct → thread-cwd → proc → proc-held → symlink)。D は 1 回の呼び出しで 1 度だけ作り共有する。開けない D は名前で確かめて消す |
| [DR-0030](DR-0030-conformance-cli.md) | ✅ 実装済 | 適合試験の CLI は `examples/conformance.rs`。公開の `bind_ex` / `connect_ex` を `LastResort::Avoid` で呼んで経路の情報を出す。`just conformance` で起こす |
| [DR-0031](DR-0031-ex-functions.md) | ✅ 実装済 | 既定の操作ごとに後始末・診断つきの `_ex` (`bind_ex`、`connect_ex`、`connect_nonblocking_ex`) を置き、最終手段 (bind は symlink-dir、connect は fork-cwd-direct) を使うかを `LastResort` で選ぶ。戻り値は std の型と `losses`・`route`・`skipped` (`bind_ex` は加えて `SocketFile`)、失敗は `ExError` (errno を保つ)。feature は持たない |

退避した DR は [archive/INDEX.md](archive/INDEX.md)。
