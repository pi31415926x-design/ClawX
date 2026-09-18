# Build

项目是 Rust/Cargo 项目。默认产物为 `mcp-shell-server`。

## Linux

在 Linux 上直接构建：

```bash
cargo build --release
```

产物：

```text
target/release/mcp-shell-server
```

## Windows

### 在 Windows 原生构建（推荐）

安装 Rust，然后在项目目录执行：

```powershell
cargo build --release
```

产物：

```text
target\release\mcp-shell-server.exe
```

### 在 Linux 交叉编译

需要 Rust target 和 Windows GNU 工具链。使用 `cargo-zigbuild` 时：

```bash
cargo install cargo-zigbuild
rustup target add x86_64-pc-windows-gnu
cargo zigbuild --release --target x86_64-pc-windows-gnu
```

产物：

```text
target/x86_64-pc-windows-gnu/release/mcp-shell-server.exe
```

## macOS

### 在 macOS 原生构建

```bash
cargo build --release
```

### 在 Linux (haoglenode) 构建 Universal 二进制

项目已有脚本，直接执行：

```bash
scripts/build-macos-shell.sh
```

脚本会完成：

1. 安装/检查 `cargo-zigbuild` 和 `apple-codesign`
2. 添加 `x86_64-apple-darwin`、`aarch64-apple-darwin` targets
3. 使用 `cargo zigbuild --release --target universal2-apple-darwin`
4. 使用 `rcodesign` 做 ad-hoc 签名

产物：

```text
dist/mcp-shell-server-macos-universal
```

## macOS GUI（可选）

在真实 Mac 上执行：

```bash
scripts/build-macos-app.sh
```

依赖 Xcode Command Line Tools、Node.js。脚本负责构建 `ClawLink.app`、嵌入 macOS shell-server，并重新进行 ad-hoc 签名。

## 常用命令

清理：

```bash
cargo clean
```

检查：

```bash
cargo check
```

验证版本：

```bash
cargo --version
rustc --version
```
