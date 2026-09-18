# macOS 构建与签名

两段式，各自在正确的机器上跑，产物已签名，无需手动 codesign。

## 1. Linux (haoglenode)：编译 + 签名 shell-server

```bash
scripts/build-macos-shell.sh
```

- `cargo zigbuild --target universal2-apple-darwin` 交叉编译出 universal 二进制
- `rcodesign sign`（纯 Rust，无需 Xcode）做 ad-hoc 签名
- 产物：`dist/clawx-service-macos-universal`（已签名，可直接传到任意 Mac 执行）

首次运行前装好依赖：`cargo install cargo-zigbuild apple-codesign`，并 `rustup target add x86_64-apple-darwin aarch64-apple-darwin`。

## 2. Mac：打包 GUI（ClawLink.app）

```bash
scripts/build-macos-app.sh
```

- `tauri build --target universal-apple-darwin` 编出 GUI
- 把脚本 1 的产物拷进 `ClawLink.app/Contents/MacOS/`（与 GUI 同级，匹配 `main.rs` 里的查找逻辑）
- `codesign --deep -s -` 对整个 bundle 重新签一次，把内嵌的 shell-server 一起覆盖

依赖：Xcode Command Line Tools、Node.js。

**首次跑前**：`tauri.conf.json` 的 `bundle.icon` 目前只有 `icon.ico`（Windows 用），macOS 打包需要 `.icns`，否则会报错/警告。补一次：

```bash
npm run tauri icon path/to/logo.png
```

## 签名说明

两步都是 **ad-hoc 签名**（无 Apple Developer 证书）：满足内核 AMFI 的"必须有签名"要求，可以正常 `exec`，但没有 Apple 背书。别人首次打开 `.app` 时 Gatekeeper 仍会提示"未知开发者"，右键→打开一次即可，之后正常。

如果以后要消除这个提示，需要 Apple Developer Program 证书，签名/公证仍可用 `rcodesign --p12-file ...` / `rcodesign notarize` 完成，不需要额外买 Mac。
