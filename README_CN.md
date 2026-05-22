# qscreen

`qscreen` 是一个轻量终端会话管理器。它把 shell 会话放在后台 daemon 中运行，支持 detach、reattach，并提供一组简洁的 `tmux` 风格命令。

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
qscreen                      # 智能启动
qscreen new work             # 创建并进入会话
qscreen new                  # 创建时间戳命名的会话
qscreen attach work          # 重新进入会话
qscreen -r work              # attach 的别名
qscreen ls                   # 列出会话
qscreen kill work            # 终止会话
qscreen shutdown             # 停止 daemon 并关闭所有会话
```

会话内热键：

- `Ctrl+A D`：detach，会话继续后台运行。
- `Ctrl+A Ctrl+A`：向 shell 发送字面 `Ctrl+A`。

`qscreen ls` 输出格式：

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
