# DR-0019: CI は経路の違う OS ごとに全部の試験を回す

- Status: Accepted
- Date: 2026-11-18

## Context

経路は OS で違う ([DR-0027](DR-0027-own-directory-and-fallback-routes.md))。Linux の /proc とスレッド cwd、FreeBSD の `bindat`、macOS の `pthread_fchdir_np` と広げた sockaddr は、その OS のカーネルでしか確かめられない。

## Decision

| job | 対象 | 中身 |
|---|---|---|
| test | ubuntu-latest、macos-latest | `just ci` (fmt、clippy、unsafe の封じ込め、試験) |
| freebsd | FreeBSD の VM | `cargo test` |
| netbsd / openbsd / illumos | NetBSD・OpenBSD・illumos (OmniOS) の VM | `cargo test` |
| cross-check | x86_64-unknown-netbsd、x86_64-unknown-freebsd、aarch64-apple-ios | `cargo clippy --all-targets -- -D warnings` |
| msrv | Rust 1.85 | `cargo check` |
| audit | RUSTSEC | cargo-audit |

- 試験は nextest で回し、試験 1 本の slow-timeout を 60 秒 × 3 にする (`.config/nextest.toml`)。CI で nextest が無ければ失敗させる
- job の `timeout-minutes` は外側の上限
- 手元では `just test` (macOS) と `just test-linux` (docker で Linux) の両方で回す

## 理由

- 経路が OS で違うので、各 OS で全部の試験を回す。スレッド cwd も bindat も無い Unix (NetBSD・OpenBSD・illumos) は、D の中で fork する bind と、macOS と共有する connect の経路 (相対パス、symlink) を、それぞれのカーネルで確かめる
- cross check は、fork だけの経路 (dirfd の primitive が無い) と BSD の sockaddr の形を compile で確かめる
- CI と手元で同じ入口 (`just ci`) を使い、検査をそろえる
- nextest の slow-timeout で、ハングした試験 1 本を job 全体の timeout でなくその試験の失敗として出す

## 判断の出所

- 出所の記録なし
