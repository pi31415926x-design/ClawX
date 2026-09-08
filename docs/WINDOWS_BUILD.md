# 编译 Windows 版本

`mcp-shell-server` 支持通过 Docker 在 Linux 上交叉编译出 Windows 可执行文件
（`x86_64-pc-windows-gnu`），不需要 Windows 机器、不需要本机装 mingw-w64。

## 一键编译

```bash
cd mcp-shell-server
sudo ./docker/build-windows.sh
```

产物：`dist/mcp-shell-server.exe`

脚本内部就是一条 `docker buildx build`，用 `docker/windows-cross.Dockerfile`：

```bash
docker buildx build \
  -f docker/windows-cross.Dockerfile \
  --output type=local,dest=./dist \
  .
```

用 `sudo` 是因为当前 haogle 节点上 docker daemon 需要 root 权限（用户不在
可免密访问 docker.sock 的组里）。导出的文件会属主为 root，如有需要自行
`sudo chown $(whoami) dist/mcp-shell-server.exe`。

## 原理

`docker/windows-cross.Dockerfile` 是个两段式 Dockerfile：

1. **builder 阶段**：基于 `rust:alpine`，`apk add mingw-w64-gcc` 装好
   GNU ABI 的交叉链接器，`rustup target add x86_64-pc-windows-gnu` 加上
   目标平台的标准库，然后 `cargo build --release --target
   x86_64-pc-windows-gnu`。
2. **export 阶段**：`FROM scratch`，只 `COPY` 出编译好的 `.exe`，配合
   `--output type=local` 直接把文件吐到宿主机 `dist/` 目录，不需要
   `docker create` + `docker cp` 这种额外步骤。

选 `rust:alpine` 是因为这台机器上已经缓存了这个镜像，不用等拉镜像；换成
`rust:slim`/`rust:bookworm` 之类的 Debian 系镜像也一样能跑，装的包换成
`apt-get install -y mingw-w64` 即可。

`win32job`、`windows-sys` 这些 Windows 专用 crate 都是纯 Rust 绑定，对
`-windows-gnu` target（而非 MSVC）完全没问题，不需要 Visual Studio 或
Windows SDK。

## 源码里的平台差异

`main.rs` 用 `#[cfg(windows)]` 做平台分支，核心是一处：Unix 下杀超时命令
用的是 `setsid` + 进程组信号，Windows 没有这套机制，改用 Job Object
（`win32job` crate）把整个子进程树（不只是 `cmd.exe`/`powershell.exe`
本身）挂到一个 Job 上，超时时 `TerminateJobObject` 一次性带走全部子孙进程。

另外 `socket2::TcpKeepalive::with_retries()` 只在类 Unix 平台上存在
（Windows 没有 `TCP_KEEPCNT` 等价的可调项），所以这个调用也用
`#[cfg(not(windows))]` 挡在了 Windows 之外，Windows 端保留
time/interval 两个 keepalive 参数，重试次数用系统默认值。

这两处是唯二需要关心的平台差异；其余代码（JSON-RPC 处理、工具实现本身）
在两个平台上是完全共享的同一份 `main.rs`。

## 验证过

在 haogle 节点上完整跑通过一次：`cargo build --release` 编译本机 Linux
版本无警告 → `sudo ./docker/build-windows.sh` 产出
`dist/mcp-shell-server.exe`，`file` 确认是合法的
`PE32+ executable (console) x86-64 ... for MS Windows`。

2026-09-08 补充：在真实 Windows 机器（`mywin` 节点，Windows 10
10.0.19045，通过 `\\<haogle-ip>\haogle` 共享盘把 `dist/` 挂载成 `Z:`
直接跑，`Get-FileHash` 确认跑的和这次交叉编译产物 SHA256 完全一致，不是
另外拷贝的旧版本）上实测了 Job Object 超时清理这条路径：起一个父命令，
父命令先 `Start-Process` 拉起一个独立子进程持续写心跳日志（每秒一条，
写够 280 条的量），子进程注册进系统进程表后父命令自己睡到超过
`COMMAND_TIMEOUT`（120s），触发服务端超时杀掉父进程。结果：心跳日志正好
在父进程被杀的时间点附近戛然而止（不是继续跑到 280 条），子进程 PID 之后
用 `Get-Process` 查询直接返回"不存在"——证明 `attach_job` +
`TerminateJobObject` 真的把整棵子进程树一起带走了，不是只杀了顶层
`powershell.exe`、把子进程遗留成孤儿进程继续跑。至此下面"已知限制"里
"还没有在真实 Windows 机器上跑起来验证行为"这条已经解决，只保留 ABI 那条。

## 已知限制 / 待办

- 交叉编译走的是 `-gnu` ABI，不是 `-msvc`；两者产出的 `.exe` 都能在
  Windows 上直接运行，通常不需要额外装运行时，但如果之后要签名或者对接
  依赖 MSVC ABI 的外部组件，需要另外评估。
