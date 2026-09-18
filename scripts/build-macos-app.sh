#!/usr/bin/env bash
# 在真实 Mac 上跑：构建 Tauri GUI，塞入已签名的 shell-server 作为 sidecar，整体重新签名一次。
# 依赖：Xcode Command Line Tools、Node.js。运行前建议先跑一次 `npm run tauri icon <logo.png>`
# 补齐 .icns（tauri.conf.json 里目前只有 icon.ico）。
set -euo pipefail

app_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../dist/app" && pwd)"
dist_dir="$(dirname "$app_dir")"

cd "$app_dir"
npm install
npm run tauri build -- --target universal-apple-darwin

bundle="$app_dir/src-tauri/target/universal-apple-darwin/release/bundle/macos/ClawLink.app"
cp "$dist_dir/clawx-service-macos-universal" "$bundle/Contents/MacOS/clawx-service"
chmod +x "$bundle/Contents/MacOS/clawx-service"

echo "== 塞完 sidecar 二进制后，整体重新签名一次 =="
codesign --force --deep -s - "$bundle"

echo "完成: $bundle"
