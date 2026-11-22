# sockat justfile
#
# version の正本は Cargo.toml の package.version で、push 時に
# check-version-bumped が「src/ を変えたのに version が据え置き」を止める。
# README / docs/MANUAL は ja が正本、en が翻訳先。push 時に
# check-outdated-translations が「en が ja より古い」を止める。
# VCS 操作は bump-semver vcs に委譲して jj / git を透過にする
# (kawaz/llm-gateway・kawaz/hyoui と同じ流儀)。

set shell := ["bash", "-euo", "pipefail", "-c"]

set script-interpreter := ["bash", "-euo", "pipefail"]

set positional-arguments

# default: list
default: list

# show recipes
list:
    @just --list --unsorted

# ---------- lint / test ----------

# cargo fmt --check + clippy (-D warnings) + unsafe 封じ込め
#
# workspace (ライブラリ sockat と CLI sockat-cli) を見る。workspace の build は
# sockat-cli がライブラリの feature __cli-internal を入れるので、利用者が使う
# feature なしのライブラリも別に clippy に掛ける。
check: lint-unsafe
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo clippy -p sockat --all-targets -- -D warnings

# cargo fmt (書き換える)
fmt:
    cargo fmt --all

# unsafe は src/sys.rs にだけ置く。他のファイル (CLI の cli/ も) に漏れたら fail
# (grep が何も拾わないのが成功)。
[private]
[script]
lint-unsafe:
    leaked=$(grep -rnE '\bunsafe[[:space:]]+(fn|impl|trait|extern|\{)' src tests examples cli --include='*.rs' \
      | grep -v '^src/sys.rs:' || true)
    if [ -n "$leaked" ]; then
      echo "ERROR: unsafe leaked outside src/sys.rs:" >&2
      echo "$leaked" >&2
      exit 1
    fi

# 試験 (nextest があれば nextest、無ければ cargo test)。doctest は nextest 非対応なので別に回す
#
# nextest を使う理由: ハングした試験 1 本を .config/nextest.toml の slow-timeout で
# 打ち切り、job 全体の timeout でなくその試験の失敗として出すため。CI (CI=true) で
# nextest が無いのは打ち切りが効かない事故なので fail させる。
[doc("試験 (nextest か cargo test、と doctest)")]
[script]
test *ARGS: check
    if cargo nextest --version >/dev/null 2>&1; then
        cargo nextest run --workspace --no-fail-fast "$@"
        cargo test --doc --workspace
    elif [ -n "${CI:-}" ]; then
        echo "error: cargo-nextest が CI に無い (ハング打ち切りが効かない)" >&2
        exit 1
    else
        cargo test --workspace --no-fail-fast "$@"
    fi

# Linux の試験を docker で回す (macOS の手元から Linux の経路を確かめる)
#
# target は target-linux/ に分ける (macOS の target/ と混ぜない)。registry は
# named volume に置いて 2 回目以降の取得を省く。docker の既定の seccomp の
# profile は filter を掛けるので、スレッド cwd の経路は使われない。
[doc("Linux の試験を docker (rust:1、既定の seccomp) で回す")]
test-linux *ARGS:
    docker run --rm -v "$PWD":/work -w /work -e CARGO_TARGET_DIR=/work/target-linux/docker \
      -v sockat-cargo-registry:/usr/local/cargo/registry rust:1 cargo test --workspace "$@"

# seccomp の filter を外した Linux (スレッド cwd の経路を通す)
[doc("Linux の試験を docker (seccomp=unconfined) で回す")]
test-linux-unconfined *ARGS:
    docker run --rm --security-opt seccomp=unconfined -v "$PWD":/work -w /work \
      -e CARGO_TARGET_DIR=/work/target-linux/docker \
      -v sockat-cargo-registry:/usr/local/cargo/registry rust:1 cargo test --workspace "$@"

# root でない uid 1000 の Linux (置き場と B の持ち主の確かめを root 以外で通す)。
# リポは読み取り専用で mount し、target と cargo の置き場はコンテナの中に置く。
[doc("Linux の試験を docker (uid 1000) で回す")]
test-linux-user *ARGS:
    docker run --rm --user 1000:1000 -e HOME=/tmp/home -e CARGO_HOME=/tmp/cargo \
      -e CARGO_TARGET_DIR=/tmp/target -v "$PWD":/work:ro -w /work rust:1 cargo test --workspace "$@"

# ---------- 適合試験 (ハブの conformance) ----------

# 適合試験の CLI の target
conformance_target := env("CARGO_TARGET_DIR", justfile_directory() / "target")

# 適合試験の CLI を build する
conformance-build:
    cargo build -q --example conformance --target-dir '{{conformance_target}}'

# 適合試験の CLI を起こす。標準入力の JSON 1 つで 1 回 bind / connect し、結果の JSON を標準出力に出す。cwd と環境は呼んだ所のまま (DR-0030)
[no-cd]
conformance: conformance-build
    @'{{conformance_target}}/debug/examples/conformance'

# check + test (CI entry point)
ci: check test

# ---------- gates (push の内部) ----------

# uncommitted change がない状態か確認
[private]
ensure-clean:
    bump-semver vcs is clean

# 現在の bookmark/branch が default (= main) 上にあるか確認
[private]
[script]
check-on-default-branch:
    if ! bump-semver vcs is on-default-branch; then
        bn=$(bump-semver vcs get default-branch)
        printf >&2 "default branch (%s) に合流してから push してください\n  1. just sync         # %s@origin に rebase\n  2. just promote      # %s bookmark を current commit に forward\n" "$bn" "$bn" "$bn"
        exit 1
    fi

# 翻訳ペアの鮮度 (正本 = *-ja.md、翻訳先 = 同じ場所の *.md)
[private]
check-outdated-translations: ensure-clean
    bump-semver vcs outdated 'glob:**/*-ja.md' '$1/$2.md'

# src/ が main@origin から変わっているのに Cargo.toml の version が上がっていなければ fail
#
# 対象を src/ に絞るのは、crate の中身に影響しない変更 (docs / justfile / CI) で
# version bump を強制しないため。
#
# bump-semver vcs diff の exit code:
#   0 = 対象 path に変更なし → bump 不要
#   1 = 変更あり → version bump 済みかチェックに進む
#   その他 = VCS error (main@origin 未 track 等)
#
# remote にまだ main が無い (= 初回の push) 時は比べる相手が無いので通す。
# git ls-remote --exit-code は ref が無いと 2 を返す。
[private]
[script]
check-version-bumped:
    lr=0
    git ls-remote --exit-code origin refs/heads/main >/dev/null || lr=$?
    if [ "$lr" = 2 ]; then
      echo "origin に main がまだ無い (初回の push) ので version の検査を飛ばす"
      exit 0
    fi
    rc=0
    bump-semver vcs diff -q main@origin -- src/ || rc=$?
    case "$rc" in
      0) exit 0 ;;
      1) ;;
      *) echo "ERROR: bump-semver vcs diff failed (rc=$rc). main@origin が track されていない可能性。先に 'jj git fetch' / 'git fetch' を試してください" >&2; exit 1 ;;
    esac
    bump-semver compare gt Cargo.toml vcs:main@origin:Cargo.toml --no-hint && exit 0
    echo 'ERROR: src/ が変わっているが Cargo.toml version 未 bump。"just bump-version" を実行してください' >&2
    exit 1

# ---------- release flow ----------

# Cargo.toml の version を上げて (既定 patch)、Cargo.lock と一緒に Release commit を作る
#
# sockat-cli (cli/Cargo.toml) はライブラリを version = "=x.y.z" で固定している
# (DR-0033) ので、同じ commit でその固定も新しい version に動かす。
[doc("version を上げて Release commit を作る")]
[script]
bump-version level="patch": ensure-clean
    new_version=$(bump-semver "$1" Cargo.toml --write --no-hint)
    perl -pi -e 's/^(sockat = \{ path = "\.\.", version = "=)[^"]*(")/${1}'"$new_version"'${2}/; s/^(version = ")[^"]*(")/${1}'"$new_version"'${2}/' cli/Cargo.toml
    if ! grep -qF "version = \"=${new_version}\"" cli/Cargo.toml || ! grep -qx "version = \"${new_version}\"" cli/Cargo.toml; then
      echo "ERROR: cli/Cargo.toml の version と sockat の固定を ${new_version} に動かせなかった" >&2
      exit 1
    fi
    cargo check --workspace --offline >/dev/null 2>&1 || cargo check --workspace >/dev/null
    bump-semver vcs commit -m "Release v${new_version}" Cargo.toml Cargo.lock cli/Cargo.toml
    echo "Version: -> ${new_version}"

# 現在の version を表示
version:
    @bump-semver get Cargo.toml --no-hint

# 現在の worktree を default branch (= origin/<default>) に rebase
sync:
    bump-semver vcs sync --onto $(bump-semver vcs get default-branch)@origin

# default branch を現在の commit に forward (push しない)
promote:
    bump-semver vcs promote

# push (gate を全部通してから)
push: ci check-on-default-branch ensure-clean check-outdated-translations check-version-bumped
    bump-semver vcs push --branch main --jj-bookmark-auto-advance
    ccmsg notify --self --text "Monitor で 'just watch' を起動して" 2>/dev/null || true

# push した commit の workflow (CI と Release) を SHA-pinned で watch する
#
# watch-workflow.sh は gh-monitor plugin の中にあり PATH に乗らないので、
# cache から最新版を探して使う。
[doc("push した commit の workflow (CI と Release) の完了を SHA-pinned watch する")]
[script]
watch:
    ws=$(ls "$HOME"/.claude-personal/plugins/cache/gh-monitor/gh-monitor/*/scripts/watch-workflow.sh 2>/dev/null | sort -V | tail -1)
    if [ -z "$ws" ]; then
        echo "watch-workflow.sh が見つかりません (gh-monitor plugin が未インストール?)" >&2
        exit 1
    fi
    "$ws" --sha "$(bump-semver vcs get commit-id --rev "$(bump-semver vcs get default-branch)")" kawaz/sockat-rs
