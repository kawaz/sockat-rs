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
| [DR-0001](DR-0001-long-socket-path-routes.md) | ✅ 実装済 | sun_path を超えるパスに届く経路と OS ごとの試す順番。副作用の無い安い経路から試し、fork は最後 |
| [DR-0002](DR-0002-std-handles-paths-it-accepts.md) | ✅ 実装済 | std が受け付けるパス (OS ごとの sun_path − 1 バイト以内) は std にそのまま渡す |
| [DR-0003](DR-0003-linux-proc-self-fd.md) | ✅ 実装済 | Linux は `/proc/self/fd/<dirfd>/<name>`。/proc が無い時とファイル名が収まらない時だけ相対パス → symlink → スレッド cwd → fork |
| [DR-0004](DR-0004-relative-path-from-cwd.md) | ✅ 実装済 | cwd からの相対パスの経路。cwd の接頭辞を削る、だめなら実パスに揃えて `..` で上る |
| [DR-0005](DR-0005-widened-sockaddr-on-macos.md) | ✅ 実装済 | macOS は広げた sockaddr で 253 バイトまで。FreeBSD では使わない |
| [DR-0006](DR-0006-symlink-in-safe-temp-dir.md) | ✅ 実装済 | `TMPDIR` → `/tmp` の安全な置き場に `sockat.XXXXXXXXXX` の symlink。bind はディレクトリ、connect はファイルを指す |
| [DR-0007](DR-0007-fork-child-fchdir.md) | ✅ 実装済 | 最後の経路として fchdir した子を fork する。子は async-signal-safe だけ、副作用は doc に書く |
| [DR-0008](DR-0008-freebsd-bindat-and-other-unix.md) | ✅ 実装済 | FreeBSD は bindat / connectat。dirfd 基準の primitive の無い Unix は相対パス → symlink → fork |
| [DR-0009](DR-0009-windows-out-of-scope.md) | ✅ 実装済 | Windows は対象外 (Unix 以外は compile_error!) |
| [DR-0010](DR-0010-public-api.md) | ✅ 実装済 | 公開 API は bind と 4 つの connect。`connect` は fork せず、`connect_any` 系は fork したかを bool で返す |
| [DR-0011](DR-0011-routes-not-exposed.md) | ✅ 実装済 | 経路は公開 API に出さない。試験は crate 内で各経路を強制し、環境を読む部分を値で差し替える |
| [DR-0012](DR-0012-route-failure-handling.md) | ✅ 実装済 | 経路の本当の失敗はやり直さない。fork しない connect はスレッド cwd の後で ENAMETOOLONG、ディレクトリが無ければどの OS でも ENOENT |
| [DR-0013](DR-0013-kernel-returned-address.md) | ✅ 実装済 | local_addr / peer_addr はカーネルに渡した綴りを返すまま。元のパスは利用者が持つ |
| [DR-0014](DR-0014-no-tokio-dependency.md) | ✅ 実装済 | tokio は依存に入れない (dev-dependency だけ)。std の型から変換する |
| [DR-0015](DR-0015-own-socket-matches-std.md) | ✅ 実装済 | 自前のソケットは std と同じ設定 (close-on-exec、SO_NOSIGPIPE、backlog)。ソケットファイルは消さない |
| [DR-0016](DR-0016-crate-distribution.md) | ✅ 実装済 | crate 名 sockat で crates.io に公開。MSRV 1.85、同梱物は include で絞る |
| [DR-0017](DR-0017-release-flow.md) | ✅ 実装済 | release は ci.yml を呼んで通ってから trusted publishing で publish し、その後に tag と GitHub release |
| [DR-0018](DR-0018-repo-name-and-spec-hub.md) | N/A | リポは kawaz/sockat-rs、crate 名は sockat、仕様の正本はハブ kawaz/sockat の SPEC |
| [DR-0019](DR-0019-ci-targets.md) | ✅ 実装済 | CI は macOS / Linux / FreeBSD・NetBSD・OpenBSD・illumos の VM で全試験、cross check、MSRV、cargo-audit |
| [DR-0020](DR-0020-thread-cwd-route.md) | ✅ 実装済 | Linux と Apple の OS は symlink の後、fork の前に、使い捨てのスレッドだけ cwd を移す経路。connect も試す |
| [DR-0021](DR-0021-nul-is-invalid-at-any-length.md) | ✅ 実装済 | NUL を含むパスは長さによらず InvalidInput で何も作らない。Linux の先頭の NUL は std にそのまま渡す |
