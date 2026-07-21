# qscreen

`qscreen` 是一个轻量终端会话管理器。它把 shell 会话放在后台 daemon 中运行，支持 detach、reattach，并通过 `qscn` 执行文件提供一组简洁的 `tmux` 风格命令。

## 功能

- 创建、列出、进入、脱离、终止终端会话。
- 智能默认命令：无会话时创建一个会话，只有一个会话时自动进入，多个会话时列出。
- 后台 daemon 按需自动启动。
- 重新进入会话时回放 scrollback。
- 内置会话列表，可在 attach 界面内切换、新建、重命名和终止会话。
- 可选状态栏显示活动会话和当前会话。
- 多个客户端可以进入同一个会话；输出广播给所有已进入的客户端，每个客户端可独立 detach。
- 终端尺寸跟随最近活跃客户端：attach、获得焦点或发送输入会让该客户端的尺寸接管 PTY，非活跃客户端的 resize 会先记录到该客户端，等它再次活跃时再应用。
- CLI 和交互界面支持英文、简体中文、繁体中文、日文、西班牙文、德文和法文，并根据系统 locale 自动选择。
- 支持 Windows、Linux、macOS。

## 平台说明

- Windows 使用 named pipe，默认启动 `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`。使用 `qscn new --shell cmd --name work` 可为单个会话启动 `C:\Windows\System32\cmd.exe`；也可以在 daemon 环境中设置 `QSCREEN_WINDOWS_SHELL=cmd` 或 `QSCREEN_WINDOWS_SHELL=cmd.exe`，把 cmd 设为 daemon 默认。显式设置 `powershell` 或 `powershell.exe` 会保持默认 PowerShell 行为。其他取值一律按 shell 可执行文件处理：完整路径（如 `C:\Program Files\PowerShell\7\pwsh.exe`）会校验文件是否存在，裸命令名（如 `pwsh`）则通过 `PATH` 解析。路径不存在会返回错误并阻止创建会话。
- Linux/macOS 使用 Unix domain socket，并启动 `$SHELL -l`，缺失时回退 `/bin/sh -l`。`qscn new --shell <path>` 可为单个会话覆盖 shell 路径。
- 新会话默认继承运行 `qscn` 时所在的目录；`qscn new --cwd <path>` 可覆盖该目录。从交互会话列表新建会话时，会继承当前选中会话所记录的启动目录；此值不会跟随 shell 内后续执行的 `cd` 更新。
- 会话通过 daemon 分配的数字 `session_id` 访问。会话名只是显示名；自定义显示名必须匹配
  `[A-Za-z0-9._-]`，最长 64 字符。

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
qscn new                     # 创建以自动分配 session_id 命名的会话
qscn new --name work         # 创建显示名为 work 的会话并进入
qscn new --shell cmd         # Windows 上创建自动命名的 cmd 会话
qscn new --shell /bin/zsh     # Unix 上创建自动命名的 zsh 会话
qscn new --shell cmd --name work
qscn new --cwd C:\work --name work
qscn attach                  # 进入 ID 最大的活动会话
qscn attach 1                # 重新进入 session_id=1 的会话
qscn att 1                   # attach 的别名
qscn -r 1                    # attach 的别名
qscn ls                      # 列出会话
qscn list                    # ls 的别名
qscn rename 1 work           # 修改 session_id=1 的显示名
qscn kill 1                  # 终止 session_id=1 的会话
qscn shutdown                # 停止 daemon 并关闭所有会话
qscn --version               # 输出 qscn 版本（别名：-V）
```

自定义前缀键：

```sh
qscn --prefix C-a attach 1    # 使用 Ctrl+A 作为会话前缀进入会话
qscn --prefix C-a new --name work
qscn --prefix C-a             # 使用 Ctrl+A 作为会话前缀智能启动
qscn --status-bar off attach 1 # 本次 attach 隐藏状态栏
```

前缀值支持 `C-a` 到 `C-z`，也支持 `Ctrl+A` 到 `Ctrl+Z`。
`QSCREEN_PREFIX` 可为所有命令设置备用前缀：

```sh
QSCREEN_PREFIX=C-a qscn attach 1
```

同时设置时，`--prefix` 优先于 `QSCREEN_PREFIX`。两者都未设置时，
`qscn` 使用默认前缀 `Ctrl+B`。

attach 时默认启用状态栏。它占用底部一行列出活动会话，以 `*` 标记当前会话，
并每两秒刷新。使用 `--status-bar off` 可为本次命令关闭状态栏，或设置
`QSCREEN_STATUS_BAR=off` 作为默认值。支持 `on` / `off`，也接受
`1|true|yes` / `0|false|no`（不区分大小写）；CLI 参数优先于环境变量。
终端高度不足三行时，状态栏会自动停用。

会话内热键：

- `<prefix> ?`：显示快捷键帮助屏（按 Esc 或 q 关闭）。
- `<prefix> d`：detach，会话继续后台运行。
- `<prefix> <prefix>`：向 shell 发送字面前缀键。
- `<prefix> s`：打开会话列表。使用方向键或 `k`/`j` 移动，Enter 切换，`c` 新建，`r` 重命名，`x` 终止，`?` 查看帮助，Esc 或 `q` 关闭。
- `<prefix> n` / `<prefix> p`：按数字 ID 顺序切换到下一个 / 上一个活动会话，并在首尾循环。

当前会话退出或在会话列表中被终止后，qscreen 会自动进入 ID 最大的剩余活动会话；
如果没有活动会话，则关闭 daemon 并退出。

默认前缀下，这些热键是 `Ctrl+B ?`、`Ctrl+B d`、`Ctrl+B Ctrl+B`、
`Ctrl+B s`、`Ctrl+B n` 和 `Ctrl+B p`。使用 `qscn --prefix C-a ...` 时，
将其中的 `Ctrl+B` 替换为 `Ctrl+A`。

`qscn ls` 输出格式：

```text
<session_id>  <name>  <状态>  <创建时间>  <终端尺寸>  <启动目录>
```

状态为 `attached`、`detached` 或 `exited(<退出码>)`。

界面语言按 `LC_ALL`、`LC_MESSAGES`、`LANG` 选择；POSIX 系统还支持 GNU
`LANGUAGE` 优先级列表，Windows 则会回退到用户 locale。不支持的 locale 使用英文。
为保持脚本兼容，`qscn ls` 的状态值始终使用英文。颜色会自动检测，也可通过
`QSCREEN_COLOR=always|never|auto` 控制，并兼容 `NO_COLOR` 和 `CLICOLOR_FORCE`。

多个终端可以同时进入同一个已有会话。所有已进入的终端都会收到同一份会话输出。某个终端
detach 后，会话和其他已进入的终端继续运行。只要至少还有一个客户端连接，`qscn ls` 就会
显示该会话为 `attached`。

daemon 会为每个已进入的客户端记录独立尺寸。PTY 使用最近活跃客户端的尺寸；这里的活跃指
客户端刚 attach、获得焦点或发送了输入。非活跃客户端触发 resize 时，只更新该客户端保存
的尺寸，不会立即 resize PTY；等该客户端下一次变为活跃时，再应用保存的尺寸。

机器客户端可在 attach 请求中设置 `"attach_mode":"bytes"` 来请求字节流模式。
daemon 会通过带有 `payload_b64` 的 `output` 事件响应：先发送非空的当前 scrollback
快照，再发送实时 PTY 字节。省略 `attach_mode` 时，保持 `qscn` 使用的默认 frame attach。

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
