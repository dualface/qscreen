# 规格说明: Windows cmd.exe shell support

## 状态

- workflow_id: wf-cmd-shell-support-20260522-91e9
- status: approved
- owner: rider
- latest_review: docs/reviews/specs/wf-cmd-shell-support-20260522-91e9.md

## 目标

让 `qscreen` 在 Windows 上支持启动 `cmd.exe` 作为新 session 的 shell。

现有 Windows 默认 shell 是 Windows PowerShell。此 workflow 增加一个明确配置入口，使用户可以选择 `cmd.exe`，并保证默认行为对现有用户保持兼容。

## 非目标

- 不改变 Linux/macOS 的 `$SHELL -l` 行为。
- 不在本 workflow 中新增完整 CLI 参数或协议字段来为每个 session 选择 shell。
- 不实现任意用户命令启动、profile 管理、shell 自动探测优先级列表。
- 不改变 named pipe、PTY attach/detach、scrollback、resize、hotkey 行为。
- 不移除 PowerShell 支持。

## 约束

- 保持 Rust 2024 edition 与现有 `rustfmt` 风格。
- 保持 protocol wire format 稳定，不新增会影响兼容性的 `qscreen-protocol` 字段，除非执行阶段发现无法通过 daemon 本地配置满足需求。
- Windows-only shell 选择逻辑必须放在 `#[cfg(windows)]` 路径内；非 Windows 测试不能依赖 ConPTY 或 Windows named pipe。
- 默认 Windows shell 必须继续是当前 PowerShell 路径，避免行为回归。
- 新配置入口应简单、可文档化、可单元测试。

## 成功标准

- Windows 下未设置配置时，新 session 仍启动 `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`。
- Windows 下设置 `QSCREEN_WINDOWS_SHELL=cmd` 或 `QSCREEN_WINDOWS_SHELL=cmd.exe` 时，新 session 启动 `C:\Windows\System32\cmd.exe`。
- Windows 下设置 `QSCREEN_WINDOWS_SHELL=powershell` 或 `QSCREEN_WINDOWS_SHELL=powershell.exe` 时，新 session 启动现有 Windows PowerShell。
- 不支持的 `QSCREEN_WINDOWS_SHELL` 值不会静默启动错误 shell；必须回退到 PowerShell 或返回清晰错误。执行阶段需选定其一并覆盖测试。
- 文档说明 Windows shell 默认值和 `cmd.exe` 启用方式。
- `cargo fmt --all` 通过。
- `cargo test --workspace` 通过，或明确记录平台限制。
- 尽可能运行 `cargo check --workspace --target x86_64-pc-windows-gnu`；若本机缺目标或链接环境，记录失败原因。

## 选定方案

在 daemon 的 `default_shell_command()` Windows 分支中增加一个小型解析层，读取环境变量 `QSCREEN_WINDOWS_SHELL`。

建议行为：

- 空值或未设置: PowerShell。
- `powershell` / `powershell.exe`: PowerShell。
- `cmd` / `cmd.exe`: `C:\Windows\System32\cmd.exe`。
- 其他值: 返回错误，阻止 session 创建，并把错误传回 client。

为了让错误可传播，执行阶段可以把 `default_shell_command()` 从返回 `CommandBuilder` 改为返回 `anyhow::Result<CommandBuilder>`，并在 `Session::new()` 中 `?` 传播。若实现者判断兼容性更重要，也可选择 unknown 值回退 PowerShell，但必须在代码和测试中明确该策略。

## 架构与边界

- `crates/qscreen-daemon/src/session.rs`
  - 负责 shell selection。
  - 增加 Windows-only shell preference parser。
  - 保持 PTY spawn 点仍是 `pair.slave.spawn_command(cmd)`。
- `crates/qscreen-client`
  - 不需要新增 CLI 参数。
  - 只接收 daemon 创建 session 失败后的现有 error path。
- `crates/qscreen-protocol`
  - 不改变 message schema。
- `README.md` / `README_CN.md`
  - 更新 Windows platform note，说明默认 PowerShell，可用 `QSCREEN_WINDOWS_SHELL=cmd` 启动 `cmd.exe`。

## Phase 假设

- Phase 1: 调整 daemon shell selection，并添加 focused unit tests。
- Phase 2: 更新 README 和中文 README。
- Phase 3: 运行格式化、测试、可用的 Windows target check。
- 不需要多进程集成测试来真实启动 ConPTY；单元测试覆盖选择逻辑即可。

## 测试与验证预期

- 添加 Windows shell preference 解析单元测试。测试函数应不要求 Windows runtime，可通过把纯解析函数放在非平台特定可测代码中，或用 `#[cfg(test)]` 辅助函数覆盖。
- 测试覆盖：
  - unset/empty -> PowerShell。
  - `cmd` / `cmd.exe` -> cmd。
  - `powershell` / `powershell.exe` -> PowerShell。
  - unknown 值 -> 明确错误或明确回退，匹配选定策略。
- 运行：
  - `cargo fmt --all`
  - `cargo test --workspace`
  - `cargo check --workspace --target x86_64-pc-windows-gnu` if target/toolchain available

## 外部依赖风险

- none

## 开放问题

- unknown `QSCREEN_WINDOWS_SHELL` 值应 hard error 还是回退 PowerShell？当前建议 hard error，因为配置拼写错误时静默回退会让用户误以为 `cmd.exe` 已启用。

## 审查历史

- 2026-05-22: 初始 spec，等待 adversarial review。
- 2026-05-22: adversarial review pass；无 critical findings；implementation planning 需先固定 unknown `QSCREEN_WINDOWS_SHELL` 策略。

## 批准

- spec_review_verdict: pass
- user_approved: yes

## Override 记录

- none
