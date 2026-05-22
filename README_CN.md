# qscreen

`qscreen` 是一个轻量终端会话管理器。它把 shell 会话放在后台 daemon 中运行，支持 detach、reattach，并通过 `qscn` 执行文件提供一组简洁的 `tmux` 风格命令。

## 功能

- 创建、列出、进入、脱离、终止终端会话。
- 智能默认命令：无会话时创建 `main`，只有一个会话时自动进入，多个会话时列出。
- 后台 daemon 按需自动启动。
- 重新进入会话时回放 scrollback。
- 转发终端 resize。
- 支持 Windows、Linux、macOS。

## 平台说明

- Windows 使用 named pipe，并启动 Windows PowerShell。
- Linux/macOS 使用 Unix domain socket，并启动 `$SHELL -l`，缺失时回退 `/bin/sh -l`。
- 会话名必须匹配 `[A-Za-z0-9._-]`，最长 64 字符。

## 构建

需要 stable Rust。本项目使用 Rust 2024 edition。

```sh
cargo build
cargo build --release
cargo test
```

Makefile 封装了常用 Cargo 命令：

```sh
make build
make release
make test
make clean
```

## 使用

```sh
qscn                         # 智能启动
qscn new work                # 创建并进入会话
qscn new                     # 创建时间戳命名的会话
qscn attach work             # 重新进入会话
qscn -r work                 # attach 的别名
qscn ls                      # 列出会话
qscn kill work               # 终止会话
qscn shutdown                # 停止 daemon 并关闭所有会话
```

自定义前缀键：

```sh
qscn --prefix C-b attach work # 使用 Ctrl+B 作为会话前缀进入会话
qscn --prefix C-b new work    # 使用 Ctrl+B 作为会话前缀创建并进入会话
qscn --prefix C-b             # 使用 Ctrl+B 作为会话前缀智能启动
```

前缀值支持 `C-a` 到 `C-z`，也支持 `Ctrl+A` 到 `Ctrl+Z`。
`QSCREEN_PREFIX` 可为所有命令设置备用前缀：

```sh
QSCREEN_PREFIX=C-b qscn attach work
```

同时设置时，`--prefix` 优先于 `QSCREEN_PREFIX`。两者都未设置时，
`qscn` 使用默认前缀 `Ctrl+A`。

会话内热键：

- `<prefix> d`：detach，会话继续后台运行。
- `<prefix> <prefix>`：向 shell 发送字面前缀键。
- `<prefix> s`：打开会话列表；选择 detached 会话后切换 attach。

默认前缀下，这些热键是 `Ctrl+A d`、`Ctrl+A Ctrl+A` 和 `Ctrl+A s`。
使用 `qscn --prefix C-b ...` 时，它们是 `Ctrl+B d`、`Ctrl+B Ctrl+B`
和 `Ctrl+B s`。

`qscn ls` 输出格式：

```text
<name>  <状态>  <创建时间>  <终端尺寸>
```

状态为 `attached`、`detached` 或 `exited(<退出码>)`。

## 项目结构

- `crates/qscreen-client`：CLI 二进制、终端 UI、daemon 启动器。
- `crates/qscreen-daemon`：会话状态、PTY 生命周期、IPC 服务端。
- `crates/qscreen-protocol`：JSON-line wire protocol 和校验逻辑。
- `crates/qscreen-shared`：共享 IPC 名称、路径和用户信息工具。

## 开发

提交改动前运行：

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Windows 交叉检查：

```sh
cargo check --workspace --target x86_64-pc-windows-gnu
```
