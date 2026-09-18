#!/usr/bin/env bash
# 在 Linux (haoglenode) 上跑：交叉编译 + ad-hoc 签名 clawx-service 的 macOS universal 二进制。
# 全程不需要 Mac；产物直接可在 Mac 上执行，无需再手动 codesign。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

command -v cargo-zigbuild >/dev/null || cargo install cargo-zigbuild
command -v rcodesign      >/dev/null || cargo install apple-codesign
rustup target add x86_64-apple-darwin aarch64-apple-darwin 2>/dev/null || true

echo "== cargo zigbuild universal2 =="
cargo zigbuild --release --target universal2-apple-darwin

out="dist/clawx-service-macos-universal"
cp "target/universal2-apple-darwin/release/clawx-service" "$out"

echo "== ad-hoc 签名（纯 Rust rcodesign，不需要 Mac/Xcode）=="
rcodesign sign "$out"
chmod +x "$out"

echo "已签名: $repo_root/$out"
