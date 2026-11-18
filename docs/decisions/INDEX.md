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
| [DR-0004](DR-0004-relative-path-from-cwd.md) | ✅ 実装済 | connect 系の予備に cwd からの相対パス。cwd の接頭辞を削る、だめなら実パスに揃えて `..` で上る |
| [DR-0005](DR-0005-widened-sockaddr-on-macos.md) | ✅ 実装済 | Apple の OS の connect 系は予備と最後の fork で広げた sockaddr を 253 バイトまで。bind と FreeBSD では使わない |
| [DR-0006](DR-0006-symlink-in-safe-temp-dir.md) | ✅ 実装済 | connect 系の予備に、`TMPDIR` → `/tmp` の安全な置き場にソケットのファイルを指す `sockat.XXXXXXXXXX` の symlink |
| [DR-0007](DR-0007-fork-child-fchdir.md) | ✅ 実装済 | 最後の手として fchdir した子を fork する (bind は D の中の R を通す)。子は async-signal-safe だけ、副作用は doc に書く |
| [DR-0009](DR-0009-windows-out-of-scope.md) | ✅ 実装済 | Windows は対象外 (Unix 以外は compile_error!) |
| [DR-0010](DR-0010-public-api.md) | ✅ 実装済 | 公開 API は bind・bind_ex・bind_any と 4 つの connect。`connect` は fork せず、fork するのは `connect_any` 系だけ |
| [DR-0011](DR-0011-routes-not-exposed.md) | ✅ 実装済 | 経路は公開 API に出さない。試験は crate 内で各経路を強制し、環境を読む部分 (`route::Config`) を値で差し替える |
| [DR-0012](DR-0012-route-failure-handling.md) | ✅ 実装済 | 経路の本当の失敗はやり直さない。fork しない connect は予備の経路の後で ENAMETOOLONG、ディレクトリが無ければどの OS でも ENOENT |
| [DR-0013](DR-0013-kernel-returned-address.md) | ✅ 実装済 | local_addr / peer_addr はカーネルに渡した綴り (`R/<ファイル名>` 等) を返すまま。R は消えているので何にも当たらない。元のパスは利用者が持つ |
| [DR-0014](DR-0014-no-tokio-dependency.md) | ✅ 実装済 | tokio は依存に入れない (dev-dependency だけ)。std の型から変換する |
| [DR-0015](DR-0015-own-socket-matches-std.md) | ✅ 実装済 | 自前のソケットは std と同じ設定 (close-on-exec、SO_NOSIGPIPE、backlog)。ソケットファイルは消さない |
| [DR-0016](DR-0016-crate-distribution.md) | ✅ 実装済 | crate 名 sockat で crates.io に公開。MSRV 1.85、同梱物は include で絞る |
| [DR-0017](DR-0017-release-flow.md) | ✅ 実装済 | release は ci.yml を呼んで通ってから trusted publishing で publish し、その後に tag と GitHub release |
| [DR-0018](DR-0018-repo-name-and-spec-hub.md) | N/A | リポは kawaz/sockat-rs、crate 名は sockat、仕様の正本はハブ kawaz/sockat の SPEC |
| [DR-0019](DR-0019-ci-targets.md) | ✅ 実装済 | CI は macOS / Linux / FreeBSD・NetBSD・OpenBSD・illumos の VM で全試験、cross check、MSRV、cargo-audit |
| [DR-0021](DR-0021-nul-is-invalid-at-any-length.md) | ✅ 実装済 | NUL を含むパスは長さによらず InvalidInput で何も作らない。Linux の先頭の NUL は std にそのまま渡す |
| [DR-0025](DR-0025-fork-bind-listen-by-parent.md) | ✅ 実装済 | fork の経路の bind は listen を必ず親がする。クライアントに見えるサーバの peer は send の前から親。相手から見えない副作用は残る |
| [DR-0027](DR-0027-own-directory-and-fallback-routes.md) | ✅ 実装済 | 長いパスは自分専用のディレクトリ D を fd で掴み、その中の乱数の symlink R を通して `R/<ファイル名>` で bind / connect する (thread-cwd → proc → fork、FreeBSD は at)。D をどこにも作れない bind は `-sockdir` の経路。connect 系は予備に proc-direct・proc-held・relative・widened・symlink・at-direct。開けない D は名前で確かめて消す |
| [DR-0028](DR-0028-bind-ex-and-socket-file.md) | ✅ 実装済 | 後始末つきの bind `bind_ex` は listener と `SocketFile` を返し、`discard()` は bind 直後に覚えた `(st_dev, st_ino)` が同じ時だけ消す。listener を閉じるのは利用者 |
| [DR-0029](DR-0029-any-functions.md) | ✅ 実装済 | any 系 (bind_any、connect_any 系) は feature `any` の後ろの「何かを失ってでも届かせる」opt-in。bind_any は最後に置き場の symlink (持ち主だけが書ける置き場 → 共有の置き場)。戻り値は `#[non_exhaustive]` の struct で、std の型と `losses`・`route`・`skipped`、失敗は `AnyError` (errno を保つ) |

退避した DR は [archive/INDEX.md](archive/INDEX.md)。
